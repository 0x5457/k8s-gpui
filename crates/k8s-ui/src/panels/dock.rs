//! Logs and terminal sessions in the bottom Dock.
//!
//! Log storage is virtualized and capped at 10,000 lines.
//! Interrupted log streams reconnect after an increasing delay.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::scroll::{ScrollableElement as _, ScrollbarAxis};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, AnyView, App, Bounds, ClipboardItem, Context, CursorStyle, Entity, FocusHandle,
    Focusable as _, FollowMode, Font, FontFeatures, Hsla, KeyContext, KeyDownEvent, Keystroke,
    ListAlignment, ListHorizontalSizingBehavior, ListOffset, ListState, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Role, ScrollHandle, SharedString, Task,
    UniformListScrollHandle, WeakEntity, Window, div, list, point, px, relative, uniform_list,
};
use k8s_actions::Copy as CopyAction;
use k8s_core::{
    atomic_file::{create_private_dir_all, write_atomic},
    ops::LogOptions,
    paths::{config_file, download_dir},
};

use crate::design::{self, Severity, space};
use crate::session::{LOG_EVENT_MAX_BYTES, TextInput};
use crate::settings::DataTypography;

use super::common::{
    buffer_font, label_body, label_panel_title, label_small, label_text, labelled, menu_item,
    spinner, status_message,
};
use super::logs::{
    LOG_LEVEL_COLUMNS, LOG_SEVERITY_COLUMNS, LogBuffer, LogEvent, LogFactory, LogFailure,
    LogLevelScope, LogLine, LogPhase, LogRequest, LogSubscription, RING_CAPACITY, TailLines,
};
use super::terminal::{
    ALL_NAMESPACES, ForwardBinding, ForwardHandle, ForwardRequest, StartedForward, TerminalEvent,
    TerminalEventSink, TerminalInstance, TerminalKind, TerminalRequest, TerminalServices,
};

/// The panels the Dock's strip switches between, in strip order.
///
/// `UI-SPEC` §16.1 is the one rule this list exists to obey: the Dock holds the *streams* — the
/// things that have to be readable next to the table — and the centre view holds the lists.
/// `UI-SPEC` §16.2's drawing also names a third tab, `Metrics`; §16.5 gives that panel its whole
/// geometry. It is not in this list because the sampling handle reaches the Inspector rather than
/// the Dock, and moving it is a decision about the shell's wiring rather than about this strip.
/// See the delivery note: the gap is reported, not designed around.
const TABS: [&str; 2] = ["Logs", "Terminal"];

/// Most log streams the Dock will hold open at once.
///
/// `UI-SPEC` §16.3: "同一时刻最多 2 个活跃日志流；超过要提示『关掉一个』". Two is a product
/// decision, not a performance one — comparing two Pods' logs means keeping the first one
/// streaming while the second connects, and three is a habit nobody has. Every one of them is a
/// live watch on the API server and every one of them keeps its ring buffer, its reconnect timer
/// and its own filter, so the cap is what keeps the Dock's cost bounded at two instead of at
/// however many Pods a reader clicked through.
const MAX_ACTIVE_LOG_STREAMS: usize = 2;

/// Tabs the strip can hold: one per open log stream, plus the Terminal.
///
/// The array is sized for the worst case and the strip is built from what is actually open, so a
/// Dock with one stream still has exactly the two tabs it has always had.
const MAX_TABS: usize = MAX_ACTIVE_LOG_STREAMS + 1;

/// Which tab the strip has selected.
///
/// A `Logs` tab names *which* stream, because §16.3's cap means there can be more than one and a
/// strip that could not say which is showing would be a strip of identical words. The two are
/// named rather than numbered because `0` and `1` in a panel that hosts a terminal, two log
/// streams and a split are the kind of numbers that quietly swap meanings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockTab {
    /// The log stream at this index: `0` is the one the body shows, `1` is the other one.
    Logs(usize),
    /// The terminal tab, which counts its own sessions inside the body.
    Terminal,
}

impl DockTab {
    /// Index of the tab's focus handle and of its position in the strip.
    fn slot(self) -> usize {
        match self {
            Self::Logs(index) => index,
            Self::Terminal => MAX_ACTIVE_LOG_STREAMS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Logs(_) => TABS[LOGS_TAB],
            Self::Terminal => TABS[TERMINAL_TAB],
        }
    }

    fn is_logs(self) -> bool {
        matches!(self, Self::Logs(_))
    }
}

/// Index of the Logs tab. The two panels are addressed by name everywhere below, because `0` and
/// `1` in a panel that hosts a terminal, a log stream and a split are the kind of numbers that
/// quietly swap meanings.
const LOGS_TAB: usize = 0;
/// Index of the Terminal tab.
const TERMINAL_TAB: usize = 1;

/// The Dock's tab pill. The strip is 40px and the pill is 28px, so 6px of chrome is left above
/// and below: an active tab that fills its strip is a band, and two bands stacked read as one
/// taller band rather than as a tab in a strip. `docs/mockup/secondary.html` draws exactly this.
const DOCK_TAB_HEIGHT: Pixels = design::size::TAB_PILL;

/// Height of a control that sits *in* one of the Dock's 28px bands, rather than being one.
///
/// The strip already answers this, and the answer is `design::size::ICON_BUTTON`: a 22px tab pill
/// and three 24px icon buttons in a 28px strip, with strip left above and below each. The two
/// toolbars took the other answer and it does not fit. A band is 28px *including* the 1px rule
/// that separates it from the body, so its interior is 27px, and a 28px control centred in 27px
/// is measured at `origin.y 212.5 / bottom 240.5` inside a band at `212 / 240` — half a pixel over
/// the rule at the bottom and half a pixel over the status bar's own hairline at the top. That is
/// the 1px misregistration §8's zero-roughness list is about, and it is measured rather than
/// argued: the assertion in `the_log_body_empty_state_offers_a_control` is what caught it.
///
/// The height also has to be *stated* on every button, which is the other half of why this alias
/// exists. gpui-kit's `Button` reads `Size::Size(px)` as the **horizontal padding** when the button
/// carries a label and takes its height from the line box of the text inside it, so
/// `with_size(Size::Size(design::size::CONTROL))` on `Open Logs` measured **20px** — 20px of box
/// around an 18px line, with the descenders of `p` and `g` a pixel off the fill. A component that
/// silently answers a different number than the one it was given is exactly the thing a named
/// constant is for, so this one is named once and used by every control on the row.
const BAND_CONTROL: Pixels = design::size::ICON_BUTTON;

/// Restarts the terminal session the Dock is showing.
///
/// `no_register`: the only trigger is the `Restart` control in the session's own
/// state panel, and that panel is implemented in `k8s-app`, which cannot name a
/// Dock method. An action is the one channel that crosses the crate boundary
/// without either side reaching into the other, and leaving it out of the
/// keymap keeps the Dock from advertising a chord it does not own.
#[derive(Clone, PartialEq, Default, Debug, gpui_kit::Action)]
#[action(namespace = k8s_dock, no_register)]
pub struct RestartTerminalSession;

/// Names the two tab groups and their panels, so a screen reader announces where focus is.
const DOCK_TAB_LIST_LABEL: &str = "Dock views";
const TERMINAL_SESSION_TAB_LIST_LABEL: &str = "Terminal sessions";
/// Spoken state for the visible "follow paused" note in the Logs toolbar.
const FOLLOW_PAUSED_DESCRIPTION: &str = "Follow paused. New log lines keep arriving but the view stays where you scrolled to. \
     Select Follow to jump back to the newest line.";
/// Widest the note's label may grow before it truncates.
///
/// `Follow paused · jump to latest` is five words in a 28px band, and unbounded it took 500px of a
/// 1920px row — enough to squeeze the filter field, the control a reader reaches for on purpose,
/// out of the toolbar entirely. The label still carries both halves of the state §16.3 asks for;
/// it carries them in whatever room the row has left, and the full sentence stays in the tooltip
/// and the accessible name.
const FOLLOW_PAUSED_LABEL_MAX_WIDTH: Pixels = px(140.);

/// The visible half of that note, and the one control in the row. It names the state and the way
/// out of it in five words, because the reader who scrolled away is reading lines, not chrome.
const FOLLOW_PAUSED_LABEL: &str = "Follow paused · jump to latest";
/// Label of the divider between the two terminal panes.
const TERMINAL_SPLIT_LABEL: &str = "Resize terminal split";
/// Label of the Dock header control that hides the Dock.
const DOCK_CLOSE_LABEL: &str = "Close dock";
/// Label of the Dock header control that folds the body away and leaves the strip.
const DOCK_COLLAPSE_LABEL: &str = "Collapse dock";
/// The same control once the body is already folded away.
const DOCK_EXPAND_LABEL: &str = "Expand dock";
/// Widest the object name on a tab may grow before it truncates.
///
/// A tab strip has to survive a collapsed Dock, and a cluster-qualified name is the longest thing
/// on it. Sizing the name to the strip would push `⌃` and `×` off the end of the row, so the
/// name is the thing that gives way: it is an identity the reader already recognises, not the
/// content of the tab.
///
/// 160px is the *name's* budget and it is the one that matters, because the name is the half a
/// reader cannot reconstruct. It used to be the whole row's budget, which is a different number
/// wearing the same one: a scope plus a name never fits in 140px, so the row's cap was reached
/// before the name had drawn and the name was what got cut — `kind-k8s-gpui-3n/kube-system/co…`
/// on this product's own cluster. See `tab_detail_parts`.
///
/// 192px is sized to the shape a generated name actually has. `<prefix>-<5>-<5>` is 26 characters
/// of `caption` and about 185px measured on screen, so the product's own Pods draw whole and a
/// CronJob's does not. The name that does not is the one that wants §2.3's middle ellipsis, which
/// needs shaped measurement; reported rather than guessed at with a character count.
fn tab_name_max_width() -> Pixels {
    design::size::ROW * 6.
}
/// Widest the scope beside a name may draw before it truncates.
///
/// The scope is the smaller half on purpose: it is a fact the title bar prints 60px above as two
/// `▾` targets, and the name is not printed anywhere else. Two log tabs at `128 + 2 + 160` plus
/// their own chrome, a Terminal tab and three 24px controls is 822px of a 960px
/// `design::size::WINDOW_MIN` window, and the `⋯ ⌃ ×` row at the end of the strip is the last thing
/// that may be pushed off it — the strip is `flex_none` all the way down, so nothing on it can
/// shrink.
fn tab_scope_max_width() -> Pixels {
    design::size::ROW * 4.
}
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
///
/// The title carries the whole of this state. It used to carry a second line as well — "Select a
/// Pod, then open Logs to stream it." — and §4.13's table is explicit that the "really none" row
/// has no 说明: the one action is `Open Logs`, and a sentence that says "select a Pod, then open
/// Logs" beside a button labelled `Open Logs` is the same instruction twice, with the button the
/// one the reader can act on. The loop it used to paper over is already closed: the control runs
/// `k8s_shell::OpenLogs`, which reports which step is missing when there is nothing to stream, so
/// pressing it with no selection answers the question rather than doing nothing.
const LOGS_NO_TARGET_TITLE: &str = "No log target";
/// Heading for the state where a target *is* selected and the cluster it would stream from is not.
/// It has to be its own sentence: the one above says there is nothing to stream, and this one has
/// to say the thing that is missing is the connection.
const LOGS_NO_SOURCE_TITLE: &str = "No log source";
/// This one keeps its 说明. It is the row §4.13 carves out — "只有当原因必须说清时才加一行" —
/// because the Pod is selected and the request went out, so "no target" would be a lie and a
/// silent body would leave a reader waiting for a stream that was never sent.
const LOGS_NO_SOURCE_HINT: &str =
    "A log target is selected, but no log source is connected. Connect a cluster, then retry.";

/// The state the strip shows when a third log stream has been asked for.
///
/// `UI-SPEC` §16.3 puts the cap's answer in five words: "关掉一个", close one. The sentence names
/// the cap, names the way out of it, and does not name a control the reader is not looking at.
/// The two open streams go in the toast, where there is room for them.
const LOG_STREAM_LIMIT_TITLE: &str = "Two log streams already open · close one first";
/// The full sentence behind that word, for the tooltip and the accessible name.
const LOG_STREAM_LIMIT_DESCRIPTION: &str = "The Dock holds two log streams at a time, and both are still connected. Close one of them, \
     then open the third.";

/// Name of the panel the active tab shows, with the session count for the Terminal tab.
fn tab_panel_label(active_tab: DockTab, sessions: usize) -> SharedString {
    match active_tab {
        DockTab::Terminal => terminal_tab_label(sessions),
        DockTab::Logs(_) => SharedString::from(TABS[LOGS_TAB]),
    }
}

/// The tab's name, the object behind it, and the state its dot reports.
///
/// The dot is a colour, and a colour is not a name: a screen reader that announced only the
/// label would say the same thing for a stream that is delivering and one that died ten minutes
/// ago. The state word is the dot's text equivalent, and it is the same word for every surface
/// that draws the state.
fn tab_aria_label(
    label: &SharedString,
    detail: Option<&SharedString>,
    severity: Option<Severity>,
) -> SharedString {
    let detail = detail.map_or(String::new(), |detail| format!(" {detail}"));
    format!("{label}{detail}. {}", tab_status_label(severity)).into()
}

/// The word behind a tab's dot, and what a tab with no dot has instead.
///
/// The silent case is named rather than left out: a tab with nothing behind it and a tab whose
/// stream is fine are different facts, and announcing neither leaves a reader guessing which one
/// they are looking at.
fn tab_status_label(severity: Option<Severity>) -> &'static str {
    match severity {
        Some(Severity::Error) => "Stream stopped",
        Some(Severity::Warning) => "Reconnecting",
        // A grey mark means the stream ended without a failure to act on — a container that ran to
        // completion. "Needs attention" is a demand, and there is nothing to do about a Job that
        // finished; the word that matches the mark is the word that keeps it honest.
        Some(Severity::Muted) => "Stream finished",
        Some(_) => "Needs attention",
        None => "Nothing open",
    }
}

/// The dot a tab carries: a mark when there is a state to report, and a reserved empty slot when
/// there is not.
///
/// The slot is always there. A tab that grows a dot when its stream breaks would push its label
/// sideways at the moment the reader is least able to absorb a move, and the same reserved-slot
/// rule the session chips already follow is the reason it does not.
fn tab_status_dot(severity: Option<Severity>, cx: &App) -> AnyElement {
    h_flex()
        .flex_none()
        .w(design::size::STATUS_DOT)
        .h(design::size::STATUS_DOT)
        .items_center()
        .when_some(severity, |this, severity| {
            this.child(
                div()
                    .w(design::size::STATUS_DOT)
                    .h(design::size::STATUS_DOT)
                    .rounded_full()
                    .bg(design::icon::status(cx, severity)),
            )
        })
        .into_any_element()
}

/// Largest history the buffer keeps. The Tail menu, Load More History, and the cap notice all read
/// the top step of the ladder in `TailLines`, so they cannot disagree with what is retained.
fn history_cap() -> i64 {
    TailLines::cap().value()
}

/// One log row's height at the reader's configured data font size.
///
/// `design::text::MONO_SM_LINE_HEIGHT` is the default, and a `const` cannot read a runtime setting,
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
/// Largest number of buffer lines one filter pass scores. A burst of 100,000 lines then costs
/// one bounded pass per frame instead of a full rescan of the buffer for every batch.
const FILTER_SCORING_BUDGET: usize = 1024;
/// Frames one filter pass may chain by asking for the next one. Twelve budgets cover the whole
/// ring, so a filter over retained history always finishes on its own. A live stream grows the
/// buffer faster than any budget can score it, so the pass stops asking past this budget and
/// waits for the next batch instead of spinning frames that never catch up.
const FILTER_PASS_FRAME_BUDGET: u32 = 12;
/// Fixed chrome above the log body: the always-present tab strip, the per-panel toolbar, and the
/// status banner row. The Dock cannot shrink below it, because those rows do not scroll.
///
/// The three bands are `DOCK_TABS`, `DOCK_TOOLBAR` and `DOCK_TOOLBAR` and not the shell's
/// `TAB_BAR` / `TOOLBAR` / `ROW`: `UI-SPEC` §16.2 draws the Dock at 28px for its chrome, and
/// `TOOLBAR` is the 40px *title* band, so reading the shell token here made every Dock 12px taller
/// than the spec's drawing on each of two bands. The third band is the one that decides
/// `DOCK_MIN`: §11.3 derives 142 as 28 + 28 + 28 + three default log lines, so a banner 4px
/// taller than the toolbar it sits under means the Dock's own minimum height no longer buys the
/// three lines the minimum was derived from — a reader who drags to the bottom of the drag gets two
/// and a half. `Pixels` arithmetic is not `const`, so the sum is a function.
fn dock_chrome_height() -> Pixels {
    design::size::DOCK_TABS + design::size::DOCK_TOOLBAR + design::size::DOCK_TOOLBAR
}

/// The height of the status banner above a populated log body.
///
/// The third of the Dock's three chrome bands, and the only one whose height is not one of the
/// two 28px chrome rows: it holds a status plate, and a plate is its own height — `space::SM` of
/// padding above and below, a `LABEL_LINE_HEIGHT` line, and the hairline on each edge. `Pixels`
/// arithmetic is not `const`, so the sum is a function.
fn log_banner_height() -> Pixels {
    space::SM * 2. + design::text::LABEL_LINE_HEIGHT + design::border::LINE * 2.
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
/// Sends panel notices to the Shell toast handler.
type NoticeHandler = Box<dyn Fn(String, Severity, &mut App)>;

/// The request the cluster is asked for, in the options one stream streams with.
///
/// A free function taking the stream rather than a panel method, because the reconnect path holds
/// a mutable borrow of one stream and an immutable borrow of the panel at the same time, and a
/// method on the panel could not be called there.
fn log_options_for(stream: &LogStream) -> LogOptions {
    LogOptions {
        container: stream.container.as_ref().map(ToString::to_string),
        follow: true,
        tail_lines: Some(stream.history_lines),
        timestamps: stream.timestamps,
        ..Default::default()
    }
}

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

/// The role a log level wears, per `UI-SPEC` §16.3 — and specifically the ink its **word**
/// wears.
///
/// This is the *word* half of the pair, and the reason the pair exists at all: a 6px mark and a
/// 13px word are the same hue at two lightnesses held to two floors, so asking one colour to do
/// both buys the mark's legibility at the word's expense. `design::role::status_word_for` is the
/// word half for everything the product spells a state with; the two quiet levels are the
/// exception and stay two steps of the ink scale (`INFO` secondary, `DEBUG` tertiary).
///
/// ERROR is danger, WARN is warning, INFO is secondary and DEBUG is tertiary. The two quiet
/// levels are two steps of the same ink scale rather than two of four status channels, so a pane
/// full of `INFO` is a pane with no colour in it at all — which is the point.
fn log_level_role(severity: Severity, cx: &App) -> Hsla {
    match severity {
        Severity::Error => design::role::status_word_for(Severity::Error, cx),
        Severity::Warning => design::role::status_word_for(Severity::Warning, cx),
        Severity::Neutral | Severity::Info => design::role::fg_secondary(cx),
        Severity::Muted | Severity::Success => design::role::fg_tertiary(cx),
    }
}

/// The ink a log level's **mark** wears, in the same lane and beside the word above.
///
/// [`design::icon::status`] for the two levels that are a state, and `fg.tertiary` for the
/// two that are not — and that split is the whole reason this is a function rather than a
/// delegation to [`design::icon::status`]. `status_for(Neutral)` is `fg_primary` and
/// `status_for(Info)` is the info channel, and the log parser reports `INFO` and `NOTICE` as
/// `Neutral`: delegating would paint a hundred `fg_primary` marks down the left of the busiest
/// data surface in the product and then either a hundred blue ones or a hundred red ones, both
/// of which is the "status colour as decoration" and "five coloured badges a screen" failure the
/// design removes rather than tunes. A level mark says "this line needs a look", and only two of
/// the four words do.
fn log_level_mark(severity: Severity, cx: &App) -> Hsla {
    match severity {
        Severity::Error => design::icon::status(cx, Severity::Error),
        Severity::Warning => design::icon::status(cx, Severity::Warning),
        Severity::Neutral | Severity::Info | Severity::Muted | Severity::Success => {
            design::role::fg_tertiary(cx)
        }
    }
}

/// Tabular figures, taken from the reader's configured data font.
///
/// `design::text` carries no feature token, and the product's own data face already turns `tnum`
/// on, so the numbers the Dock prints get it from the same setting the log rows are measured with
/// rather than from a second, private list of feature tags that could disagree with it.
fn tabular_features(cx: &App) -> FontFeatures {
    crate::settings::data_typography(cx).features
}

/// The ink a log timestamp wears.
///
/// `UI-SPEC` §16.3 asks for `fg.disabled`, and that role has not been contrast-solved yet: it is
/// the quietest ink in the scale and a timestamp is the first thing a reader looks for in a log.
/// `fg_tertiary` is one step up and reads at both appearances, so the token is a report to the
/// design layer rather than a decision this panel gets to make. When `fg_disabled` is solved,
/// this is the one line that changes.
/// The ink a log row's timestamp is drawn in.
///
/// `UI-SPEC` §16.3 says `fg.disabled`, and §16.3 is the frozen decision — so this is a §7 report,
/// not a silent swap, and the number is here so the next reader does not "fix" it and ship a
/// timestamp nobody can read:
///
/// | on `surface.inset` | Dark `#050607` | Light `#F1F2F4` |
/// |---|---|---|
/// | `fg.disabled` | **2.31:1** | **1.78:1** |
/// | `fg.tertiary` (this) | 4.06:1 | 3.14:1 |
/// | `fg.secondary` | 7.68:1 | 5.64:1 |
///
/// The log body now sits on `role::surface_content` (`#111216` Dark, `#FFFFFF` Light) rather
/// than on `surface.inset`, so the row this table measured on is one step further from the ink:
/// 4.04:1 in Dark and 4.17:1 in Light. Both are above §1.4's 3:1 floor and the decision is
/// unchanged — the numbers here are what the swap *costs*, and what it buys is a log pane that
/// shares the table's plane instead of sitting below it.
///
/// §1.4 puts a floor of 3:1 on `fg.tertiary` and deliberately gives `fg.disabled` none, because
/// `fg.disabled` is specified for disabled text and the `·` separator and not for content. The
/// log plane is the app's *darkest* surface in Dark and its second-lightest in Light, so it is the
/// worst case for a faint ink in both, and a timestamp is content a reader is scanning for. 2.31:1
/// is below the floor the spec sets on the role one step up; 1.78:1 is below anything.
///
/// `fg.tertiary` is the nearest satisfiable role and it clears §1.4's 3:1 floor on this surface in
/// both appearances. §7: reported in the delivery note, decision unchanged here.
///
/// One thing it does cost: `fg.tertiary` is also what `DEBUG` is drawn in, so a timestamp and a
/// debug level are the same ink. They are not the same kind of thing — one is a column and one is
/// a value — but they are the same quiet, which is the point of both.
fn log_timestamp_role(cx: &App) -> Hsla {
    design::role::fg_tertiary(cx)
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
        format!(
            "{} · {}",
            TABS[TERMINAL_TAB],
            design::format::count(sessions)
        )
        .into()
    } else {
        SharedString::from(TABS[TERMINAL_TAB])
    }
}

fn timestamp_column_width(columns: usize, typography: &DataTypography) -> Pixels {
    log_columns(typography, columns)
}

/// Width of the level cell a log row reserves.
///
/// The cell is the widest level word the parser can produce, not the one this line carries, so
/// every message in the buffer starts in the same column and a column of `ERROR` lines reads as
/// a column rather than as a ragged edge. `UI-SPEC` §16.3 draws the level as text: a 6px dot is
/// a colour and nothing else, and the level is the one thing a reader scans this pane for.
///
/// The lane is *wider* than the four canonical words need, and deliberately so: it holds a 6px
/// mark, a `space::SM` gap and the word, and its width is fixed by the widest token the parser
/// can emit rather than by the widest word the panel prints. The alternative — sizing the lane
/// from [`LOG_LEVEL_COLUMNS`] — makes a row's measured width depend on which of the two
/// vocabularies a particular line happened to carry, and `log_row_width` is the number the
/// no-wrap list scrolls against, so that is a number that would change with the data.
/// [`LOG_LEVEL_COLUMNS`] is therefore the word's floor inside the lane and not the lane itself.
fn level_column_width(typography: &DataTypography) -> Pixels {
    log_columns(typography, LOG_SEVERITY_COLUMNS)
}

/// Floor for the word cell inside the level lane: the two narrowest canonical words, `INFO` and
/// `WARN`. The two wide ones, `ERROR` and `DEBUG`, are a character longer and take the lane's
/// remaining room, so the word is never truncated and the mark never moves.
fn level_word_floor(typography: &DataTypography) -> Pixels {
    log_columns(typography, LOG_LEVEL_COLUMNS)
}

/// Timestamp cell width for one row. The reserve keeps every message in the same column, and a
/// longer token still gets its own width so the row can show it in full.
fn log_timestamp_width(line: &LogLine, reserve: usize, typography: &DataTypography) -> Pixels {
    match line.timestamp_columns() {
        0 if reserve == 0 => px(0.),
        columns => timestamp_column_width(columns.max(reserve), typography),
    }
}

/// Columns a row spends on the timestamp and the level before the message.
///
/// These do not depend on how wide the Dock is. A row used to drop its level label in a narrow
/// Dock and keep a dot instead, which made the level of every line on screen depend on the width
/// of the window — the same stream read two different ways in two windows. The level is the
/// data, so it is the one column that never gives way.
fn log_row_fixed_width(timestamp_width: Pixels, typography: &DataTypography) -> Pixels {
    space::SM + timestamp_width + space::SM + level_column_width(typography) + space::SM + space::SM
}

/// Width a no-wrap row reserves. It must match the cells `log_row` draws, or the uniform list
/// scrolls a row that is wider than its content.
fn log_row_width(line: &LogLine, timestamp_reserve: usize, typography: &DataTypography) -> Pixels {
    let columns = line.display_columns();
    // Every row draws the copy control, so every row reserves it. A short line could be
    // selected with the keyboard, so hiding the control by line width left it uncopyable.
    log_row_fixed_width(
        log_timestamp_width(line, timestamp_reserve, typography),
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

/// The sentence a Dock control explains itself with, on hover.
///
/// The Dock's tooltips are one sentence long, so a plain text tooltip is the whole component: the
/// reader is told what the control does and nothing else, and the keycap version below is the only
/// one that carries a second element.
fn command_tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

/// Whether a keystroke is the one that presses whatever holds the keyboard.
///
/// The Dock's own controls answer to it, because the wrapper that holds the Dock's focus handle
/// has no button of its own to press: Enter or Space here is the same as a click on the control
/// the handle belongs to.
fn presses(keystroke: &Keystroke) -> bool {
    matches!(keystroke.key.as_str(), "enter" | "return" | "space")
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
fn dock_close_chord(contexts: &[KeyContext], cx: &App) -> Option<String> {
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
///
/// While a session holds the keyboard there is no chord to offer, and the tooltip says so by
/// drawing the label alone.
fn dock_control_tooltip(
    label: impl Into<SharedString>,
    chord: Option<String>,
) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let label = label.into();
    let key = chord
        .as_deref()
        .and_then(|chord| Keystroke::parse(chord).ok())
        .map(Kbd::new);
    move |window, cx| {
        Tooltip::new(label.clone())
            .key_binding(key.clone())
            .build(window, cx)
    }
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
fn is_log_copy_chord(keystroke: &Keystroke) -> bool {
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

/// One log stream the Dock is holding, and everything that belongs to it alone.
///
/// `UI-SPEC` §16.3 caps the Dock at two *active* log streams, so a stream is a thing with its own
/// identity rather than a value the panel overwrites. Everything that would be wrong to share
/// between two Pods lives here: the request, the ring buffer, the subscription that is still
/// pulling lines, the reconnect timer, the filter, the find bar, the follow state, the caret, and
/// the scroll position. What stays on the panel is what is genuinely the Dock's: the two text
/// fields (one control, mirrored into whichever stream is on screen) and the log factory.
struct LogStream {
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
    phase: LogPhase,
    buffer: LogBuffer,
    buffer_bytes: usize,
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

    /// The `⌘F` find bar: open, its query, the rows it matched, and which one is active.
    ///
    /// Find and filter are two different questions and they get two different fields. The filter
    /// answers "show me fewer lines"; find answers "where is this line in the ten thousand I have".
    /// Merging them means a find that finds nothing also empties the pane, and the reader loses
    /// the context they were reading to find the thing in.
    find_open: bool,
    find_query: String,
    /// Buffer indices of the matching lines, in order.
    find_hits: Vec<usize>,
    /// Row positions of the same lines, which is what the rows know themselves by.
    find_rows: Vec<usize>,
    /// Index into `find_hits` of the line the reader is on.
    find_active: usize,
    /// Keyboard caret and range selection over the visible log rows. `None` until the list
    /// takes focus.
    log_selection: Option<LogSelection>,
    /// Lines the ring buffer dropped since the current target was opened.
    dropped_lines: u64,
    /// Wall-clock milliseconds of the last line the stream delivered.
    ///
    /// A pane that has stopped saying anything looks exactly like a pane that is quiet, and the
    /// difference is the whole question when a log stream dies. The age of the last line is what
    /// tells them apart, so the Dock keeps it and reports it in the stopped state.
    last_line_at_ms: Option<i64>,
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

    subscription: Option<Box<dyn LogSubscription>>,
    stream_task: Option<Task<()>>,
    reconnect_task: Option<Task<()>>,
    /// Bumped on every connection. It is what tells a late line from a current one, and it is
    /// per stream because a parked stream keeps streaming while the live one reconnects.
    epoch: u64,
    lines_received: u64,
    attempts: u32,
}

impl LogStream {
    /// The container this stream reads, in the words the control that changes it uses.
    ///
    /// A stream with no container named is a Pod whose containers the list never reported, and
    /// the label says so rather than printing an empty control.
    fn container_label(&self) -> SharedString {
        self.container.clone().unwrap_or_else(|| "all".into())
    }

    /// A stream with nothing behind it yet: the state the Logs tab is in before a target is
    /// chosen, and the state a fresh slot is handed when the reader closes one.
    fn empty() -> Self {
        let list_state = ListState::new(0, ListAlignment::Top, design::size::DOCK_MAX);
        let list_follow_report: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        install_list_scroll_report(&list_state, &list_follow_report);
        Self {
            request: None,
            container: None,
            history_lines: TailLines::FiveHundred.value(),
            timestamps: false,
            wrap: true,
            follow: true,
            following: true,
            phase: LogPhase::Idle,
            buffer: LogBuffer::new(),
            buffer_bytes: 0,
            log_filter: String::new(),
            filter_cursor: 0,
            filter_frames_left: FILTER_PASS_FRAME_BUDGET,
            filter_pass_deferred: false,
            log_level: LogLevelScope::All,
            find_open: false,
            find_query: String::new(),
            find_hits: Vec::new(),
            find_rows: Vec::new(),
            find_active: 0,
            log_selection: None,
            dropped_lines: 0,
            last_line_at_ms: None,
            visible_log_indices: Vec::new(),
            longest_visible_log_row: 0,
            synced: 0,
            list_state,
            list_follow_report,
            scroll_handle: UniformListScrollHandle::new(),
            subscription: None,
            stream_task: None,
            reconnect_task: None,
            epoch: 0,
            lines_received: 0,
            attempts: 0,
        }
    }

    /// The identity a reader recognises a stream by: the object it is pointed at, container
    /// included, and nothing about how it is displayed.
    ///
    /// `LogRequest` has no `PartialEq` on purpose — the containers list is part of what the
    /// cluster sent, and two requests for the same Pod with a different list are still the same
    /// stream. Namespace and name are the object; the container chooses which of its streams.
    fn identity(&self) -> Option<(String, String, Option<String>)> {
        let request = self.request.as_ref()?;
        Some((
            request.namespace.as_deref().unwrap_or("default").to_owned(),
            request.name.to_string(),
            self.container.as_ref().map(ToString::to_string),
        ))
    }

    /// Releases the connection. Dropping the subscription is what stops the API server sending,
    /// so a closed stream is a closed stream rather than a hidden one.
    fn disconnect(&mut self) {
        if let Some(mut subscription) = self.subscription.take() {
            subscription.cancel();
        }
        self.stream_task = None;
        self.reconnect_task = None;
    }
}

pub struct DockPanel {
    active_tab: DockTab,
    tab_focus_handles: [FocusHandle; MAX_TABS],
    focus_handle: FocusHandle,
    /// Focusable close control in the Dock header. The Dock has no other way to say "hide me",
    /// and the toggle is unbound while a session owns the keyboard.
    close_focus: FocusHandle,
    /// Focusable collapse control beside the close control. It is a separate stop from `×`
    /// because it is a separate action: one hides the Dock, the other leaves the strip.
    collapse_focus: FocusHandle,
    /// Whether the strip's overflow menu is open, so the trigger can stay visibly pressed while
    /// its popup is up.
    ///
    /// gpui-kit's dropdown takes focus away from its trigger the moment the menu opens, so the
    /// trigger's own hover and press states cannot explain the relationship between the two. The
    /// flag is written by the popover's `on_open_change` and read by the trigger's fill, which is
    /// what "a Button that owns a dropdown must remain visibly pressed or open until the popup
    /// closes" asks for.
    ///
    /// One flag serves *both* triggers, not because they are one control but because only one of
    /// them can be open: the Terminal tab mounts the same menu twice — in the strip's overflow
    /// slot and in its own band — and a popover that is up is up over both. Two flags would let a
    /// stale one keep a trigger pressed after its menu closed, which is the exact failure the flag
    /// exists to prevent.
    overflow_menu_open: Cell<bool>,
    /// Whether the reader has folded the Dock's body away, leaving only the tab strip.
    ///
    /// This is a request, not the final word. `UI-SPEC` §11.3 folds the body away on its own
    /// below `design::size::DOCK_COLLAPSE_BELOW`, so [`Self::body_collapsed`] is the answer and
    /// this is only the half the reader controls.
    body_collapse_requested: bool,
    /// Whether `Window::context_stack` can be asked yet.
    ///
    /// That accessor walks the rendered frame's focus tree and asserts the tree
    /// exists, so it is not callable from a render that has not painted. A reader
    /// cannot hover this button before there is a frame to hover in, so the hint
    /// has nothing to say until then; it starts blank and becomes correct on the
    /// first key event, which is the first thing that can only happen after a
    /// paint.
    context_stack_ready: Cell<bool>,

    /// The log streams, one slot each, always [`MAX_ACTIVE_LOG_STREAMS`] of them.
    ///
    /// A slot is a position and a stream is a thing: slot 0 is whichever Pod was opened first and
    /// keeps that tab, position and identity for as long as it is open, and selecting it is a
    /// change of which slot the body reads rather than a move of the stream itself. That is what
    /// makes a tab's dot, its name and its position all describe the same Pod after the reader has
    /// flipped between the two.
    ///
    /// A slot with nothing behind it is not idle, it is empty: an empty slot draws a `Logs` tab
    /// with no dot and no close control, and the first `Open Logs` fills it.
    streams: Vec<LogStream>,
    /// Which slot the log body is showing. Always a valid index: the two are set together.
    active_log: usize,

    log_filter_input: Entity<TextInput>,
    /// True while the panel and the filter field are handing a value to each other, so
    /// neither of them re-enters the update that made it. The field reports every value it
    /// takes, and the panel writes values the field does not hold on its own.
    log_filter_writing: Rc<Cell<bool>>,
    find_input: Entity<TextInput>,
    /// True while the find field is handing a value to the panel, so neither re-enters the
    /// update that made it.
    find_writing: Rc<Cell<bool>>,
    /// Tracks the Dock's own bounds, so the compact breakpoint follows the panel instead of
    /// the window. The Dock is `w_full()`, so the window width is only a first-frame guess.
    width_handle: ScrollHandle,
    /// Tracks the log body, so a Dock squeezed below a readable height says so instead of
    /// showing half a row of text.
    log_body_handle: ScrollHandle,

    /// The refusal `UI-SPEC` §16.3 asks for when a third log stream is asked for, and the toast
    /// it was sent with. It lives on the panel rather than inside a stream because it is about
    /// the Dock's capacity, not about any one stream's state.
    log_cap_notice: Option<(SharedString, String)>,

    factory: Option<LogFactory>,

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
    next_forward_id: u64,
    next_forward_runtime_id: u64,

    /// Focus handle for the `Open Logs` control in the log body's empty state, so the recovery
    /// path is on the same keyboard path as the log list it replaces.
    open_logs_focus: FocusHandle,

    notice: Option<NoticeHandler>,
    pending_notice: Option<(String, Severity, String)>,
}

impl DockPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // The field reports the value the reader typed, so the filter is driven from here
        // rather than from a notify: the field mirrors a value it adopted and does not
        // re-notify for it, so an observer only ever saw the values the Dock wrote itself.
        let panel = cx.weak_entity();
        let find_writing = Rc::new(Cell::new(false));
        let find_field_writing = Rc::clone(&find_writing);
        let find_panel = panel.clone();
        let find_input = cx.new(move |cx| {
            let panel = find_panel.clone();
            let writing = find_field_writing;
            TextInput::new("Find in logs…", cx, move |text, cx| {
                if writing.replace(true) {
                    return;
                }
                panel
                    .update(cx, |panel, cx| panel.set_find_query(text, cx))
                    .ok();
                writing.set(false);
            })
            .with_accessibility(
                "Find in Logs",
                "Type text to highlight the log lines that contain it. Press Enter for the next \
                 match, Shift with Enter for the previous one, and Escape to close.",
                "Close find",
            )
            .with_width(design::size::ROW * 5.)
            // The band owns Escape for the find bar, and the field has to let go of it to let
            // the band have it. With the hint on, the field answers Escape first — it sits on the
            // key path ahead of its own band — and the band never sees the second press, so the
            // bar could be cleared but never closed and the keyboard could not get back to the
            // rows. One press that closes it is also the better rule than a press that clears and
            // a press that closes.
            .without_escape_hint()
        });
        let log_filter_writing = Rc::new(Cell::new(false));
        let writing = Rc::clone(&log_filter_writing);
        let log_filter_input = cx.new(move |cx| {
            let panel = panel.clone();
            let writing = Rc::clone(&writing);
            TextInput::new("Filter log lines…", cx, move |text, cx| {
                if writing.replace(true) {
                    return;
                }
                panel
                    .update(cx, |panel, cx| panel.set_log_filter(text, cx))
                    .ok();
                writing.set(false);
            })
            .with_accessibility(
                "Filter logs",
                "Type text to match log lines. Press Escape to clear the filter.",
                "Clear log filter",
            )
        });
        Self {
            active_tab: DockTab::Logs(0),
            tab_focus_handles: std::array::from_fn(|index| {
                cx.focus_handle().tab_stop(true).tab_index(index as isize)
            }),
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(2isize),
            close_focus: cx.focus_handle().tab_stop(true).tab_index(2isize),
            collapse_focus: cx.focus_handle().tab_stop(true).tab_index(2isize),
            overflow_menu_open: Cell::new(false),
            body_collapse_requested: false,
            context_stack_ready: Cell::new(false),
            streams: std::iter::repeat_with(LogStream::empty)
                .take(MAX_ACTIVE_LOG_STREAMS)
                .collect(),
            active_log: 0,
            log_filter_input,
            log_filter_writing,
            find_input,
            find_writing,
            width_handle: ScrollHandle::new(),
            log_body_handle: ScrollHandle::new(),
            log_cap_notice: None,
            factory: None,
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
            next_forward_id: 0,
            next_forward_runtime_id: 0,
            open_logs_focus: cx
                .focus_handle()
                .tab_stop(false)
                .tab_index(OPEN_LOGS_TAB_INDEX),
            notice: None,
            pending_notice: None,
        }
    }

    /// The stream the log body is showing.
    ///
    /// Every read of log state goes through here, so "which stream is this" is answered in one
    /// place. `active_log` is the only thing that decides it, and the strip is what sets it.
    fn log(&self) -> &LogStream {
        &self.streams[self.active_log]
    }

    fn log_mut(&mut self) -> &mut LogStream {
        let slot = self.active_log;
        &mut self.streams[slot]
    }

    /// The stream in `slot`, mutably. The slot is known to exist by every caller that has already
    /// counted the streams, and the panic is the alternative to a silent no-op on a reconnect timer
    /// that would then never fire.
    fn log_mut_at(&mut self, slot: usize) -> &mut LogStream {
        self.streams.get_mut(slot).expect("a slot inside the cap")
    }

    /// How many log streams are connected. `UI-SPEC` §16.3's cap is about this number.
    fn open_log_streams(&self) -> usize {
        self.slots_with_streams().len()
    }

    /// Slots that hold a stream the reader asked for, in slot order.
    fn slots_with_streams(&self) -> Vec<usize> {
        (0..MAX_ACTIVE_LOG_STREAMS)
            .filter(|slot| self.log_has_stream(DockTab::Logs(*slot)))
            .collect()
    }

    /// The first slot with no stream behind it, or `None` when the Dock is at the cap.
    fn free_log_slot(&self) -> Option<usize> {
        (0..MAX_ACTIVE_LOG_STREAMS).find(|slot| !self.log_has_stream(DockTab::Logs(*slot)))
    }

    /// Slot index of a stream, by identity. `None` when the Dock is not holding it.
    fn log_slot_of(&self, identity: &(String, String, Option<String>)) -> Option<usize> {
        self.slots_with_streams()
            .into_iter()
            .find(|slot| self.log_slot(*slot).and_then(|s| s.identity()).as_ref() == Some(identity))
    }

    /// A short name for a stream, for the strip and for the refusal that names what is open.
    fn log_slot_label(&self, slot: usize) -> String {
        let Some(stream) = self.log_slot(slot) else {
            return "no log target".to_owned();
        };
        match &stream.request {
            Some(request) => {
                let container = stream
                    .container
                    .as_ref()
                    .map(|container| format!(":{container}"))
                    .unwrap_or_default();
                format!(
                    "{}/{}{container}",
                    request.namespace.as_deref().unwrap_or("default"),
                    request.name
                )
            }
            None => "no log target".to_owned(),
        }
    }

    /// The tabs the strip holds, in strip order: one per open log stream, then the Terminal.
    ///
    /// A Dock with no stream still has a `Logs` tab. §16.2 draws the strip as the thing that
    /// survives a collapsed body, and a strip with no way to reach Logs is not that.
    fn tabs(&self) -> Vec<DockTab> {
        let mut tabs = self.slots_with_streams();
        if tabs.is_empty() {
            tabs.push(0);
        }
        tabs.into_iter()
            .map(DockTab::Logs)
            .chain([DockTab::Terminal])
            .collect()
    }

    fn sync_focus_handles(&mut self) {
        for (index, handle) in self.tab_focus_handles.iter_mut().enumerate() {
            *handle = handle
                .clone()
                .tab_stop(self.active_tab.slot() == index)
                .tab_index(index as isize);
        }
        // The close control follows the tab strip, so it is the last stop in the header group.
        self.close_focus = self.close_focus.clone().tab_stop(true).tab_index(2isize);
        // The collapse control sits just before it and survives the body's own collapse, so it
        // takes its stop whenever the strip is on screen — which is always.
        self.collapse_focus = self.collapse_focus.clone().tab_stop(true).tab_index(2isize);
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
            .tab_stop(self.active_tab.is_logs() || self.terminals.is_empty())
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
        self.active_tab.is_logs() && !(self.log().request.is_some() && self.factory.is_some())
    }

    fn request_uniform_follow(&mut self) {
        if !self.log().follow || !self.log().following {
            return;
        }
        if self.log().wrap {
            self.log_mut().list_state.scroll_to_end();
        } else {
            self.log_mut().scroll_handle.scroll_to_bottom();
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

    /// Whether the Dock is showing its strip and nothing else.
    ///
    /// Two things decide it, and both have to. The reader can fold the body away from the
    /// `⌃` control, and `UI-SPEC` §11.3 folds it away on its own below
    /// `design::size::DOCK_COLLAPSE_BELOW` because a table is worth more than a log. The strip
    /// is never part of the answer: it is the only way back, so it stays at
    /// `design::size::DOCK_TABS` either way.
    pub fn body_collapsed(&self, window: &Window) -> bool {
        self.body_collapse_requested
            || f32::from(window.viewport_size().height) < design::size::DOCK_COLLAPSE_BELOW
    }

    /// The height the Dock needs to show only its tab strip.
    ///
    /// The shell owns the Dock's height and this is the value it should read while the body is
    /// collapsed. It is a function rather than the token so the strip and the answer cannot be
    /// two different facts.
    pub fn collapsed_height() -> Pixels {
        design::size::DOCK_TABS
    }

    /// Folds the body away, or brings it back, without going through the control.
    ///
    /// The shell owns the Dock's height and it is the only thing that knows *why* the Dock is on
    /// screen: it starts the window with the strip and nothing under it — `UI-SPEC` §16.2 keeps the
    /// 28px strip resident precisely so the Dock can be open with a collapsed body, and §11.3's
    /// height break is the reader's, not the launcher's — and every entry point that means "I
    /// asked for the Dock" means the body too. Neither of those is a click, so neither of them can
    /// go through `Self::toggle_body_collapse`, which is a `Window` reader and stays private.
    ///
    /// It sets the request and nothing else. [`Self::body_collapsed`] is still the answer, so a
    /// shell that unfolds a Dock in a 700px-tall window gets the same folded strip a reader would
    /// get there, rather than a second rule that the `⌃` control can contradict.
    pub fn set_body_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.body_collapse_requested == collapsed {
            return;
        }
        self.body_collapse_requested = collapsed;
        cx.notify();
    }

    /// Folds the body away, or brings it back.
    ///
    /// Bringing it back clears the reader's own request but cannot overrule the window: below
    /// `design::size::DOCK_COLLAPSE_BELOW` the body stays folded however many times the control
    /// is pressed, and the control says `Expand Dock` while it does, so the reader is not invited
    /// to press a button that will not move.
    fn toggle_body_collapse(&mut self, window: &Window, cx: &mut Context<Self>) {
        if f32::from(window.viewport_size().height) < design::size::DOCK_COLLAPSE_BELOW {
            return;
        }
        self.body_collapse_requested = !self.body_collapse_requested;
        cx.notify();
    }

    fn log_row_context(&self, cx: &Context<Self>) -> LogRowContext {
        LogRowContext {
            selection: self.log().log_selection,
            panel: cx.entity().downgrade(),
            find: self.find_highlight(),
        }
    }

    /// The find highlight for this frame, or `None` when the find bar is closed or has no query.
    fn find_highlight(&self) -> Option<FindHighlight> {
        if !self.log().find_open || self.log().find_query.is_empty() {
            return None;
        }
        let active = *self.log().find_hits.get(self.log().find_active)?;
        Some(FindHighlight {
            rows: Rc::new(self.log().find_rows.clone()),
            active,
        })
    }

    fn reset_uniform_scroll(&mut self) {
        self.log()
            .scroll_handle
            .0
            .borrow_mut()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
        self.log_mut()
            .scroll_handle
            .0
            .borrow_mut()
            .deferred_scroll_to_item = None;
        self.request_uniform_follow();
    }

    /// Replaces the log stream with static lines for previews and tests.
    pub fn set_lines(&mut self, lines: Vec<String>, cx: &mut Context<Self>) {
        self.cancel_stream();
        self.log_mut().log_filter.clear();
        self.write_log_filter_input("", cx);
        self.log_mut().buffer.clear();
        self.log_mut().buffer_bytes = 0;
        self.log_mut().dropped_lines = 0;
        self.log_mut().filter_cursor = 0;
        self.log_mut().log_selection = None;
        self.append_log_lines(lines);
        self.log_mut().follow = true;
        self.log_mut().following = true;
        self.rebuild_log_view(log_row_height(cx));
        self.reset_uniform_scroll();
        self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
        self.log_mut().phase = LogPhase::Streaming;
        // The lines replace the stream in the body, so the body is the body whichever slot that is.
        self.active_log = 0;
        self.active_tab = DockTab::Logs(0);
        self.terminal_maximized = false;
        self.sync_focus_handles();
        cx.notify();
    }

    /// Sets the log source. None disables streaming.
    ///
    /// Every open stream reconnects against the new source, not only the one in the body: a
    /// cluster connection arrived after the reader had two tabs open, and a source that is only
    /// applied to the tab they happen to be looking at leaves the other tab on a dead handle while
    /// its buffer looks live.
    pub fn set_log_factory(&mut self, factory: Option<LogFactory>, cx: &mut Context<Self>) {
        self.factory = factory;
        let slots = self.slots_with_streams();
        if slots.is_empty() {
            cx.notify();
            return;
        }
        for slot in slots {
            self.start_stream_at(slot, cx);
        }
    }

    /// Routes panel notices to Shell toasts.
    pub fn set_notice_handler(&mut self, handler: impl Fn(String, Severity, &mut App) + 'static) {
        self.notice = Some(Box::new(handler));
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// The 28px strip's own focus handle — the one that stays reachable while the
    /// body is collapsed.
    ///
    /// `UI-SPEC` §16.2 keeps the strip resident precisely so the Dock can be open
    /// with a folded body, which means the strip is the only way *into* a
    /// collapsed Dock. A caller walking the tab order therefore has to ask for
    /// this one as well as [`Self::focus_handle`]: which of the two is a tab stop
    /// depends on whether the body is on screen.
    pub fn collapse_focus_handle(&self) -> FocusHandle {
        self.collapse_focus.clone()
    }

    /// Live, paused, or reconnecting state of the log stream, for surfaces outside the Dock such
    /// as the status bar, which stays on screen while the Dock is collapsed. `None` when no log
    /// target is open.
    ///
    /// The bar names the transport state and nothing more. The Dock owns the failure: it names the
    /// class, the next step, and the reason, so a failed stream is not read out again here.
    pub fn log_status_label(&self) -> Option<&'static str> {
        self.should_show_log_status()
            .then(|| self.log().phase.label())
    }

    /// Opens a log target, and holds on to the one the reader already had.
    ///
    /// §16.3 caps the Dock at `MAX_ACTIVE_LOG_STREAMS` *active* streams, so this is three
    /// answers and the reader has to be able to tell them apart:
    ///
    /// - The target is already open. Select its tab. Restarting a stream the reader is reading to
    ///   ask for it again would throw away their scroll position and the lines they had scrolled
    ///   back through, and the only thing the request asked for was to be *shown*.
    /// - There is a free slot. Park the stream in the body into the other slot — still connected,
    ///   still filling, still reporting on its own tab — and start the new one here.
    /// - Both slots are taken. Refuse, and say which two are open.
    ///
    /// The refusal is not a toast alone. §16.2 keeps the tab strip on screen when the body is
    /// folded to nothing, so a third stream can be asked for with no log body anywhere on the
    /// display; a message that only exists in the body, or only in a toast that has already faded,
    /// is a message the reader never sees. `Self::log_cap_notice` puts it on the strip, which is
    /// the one Dock surface that is always painted.
    pub fn open_logs(&mut self, request: LogRequest, cx: &mut Context<Self>) {
        // `spec.containers[0]` is the container a single-container Pod has and the one a
        // multi-container Pod most likely means. It is a *guess* on a five-container Pod, so the
        // toolbar names it and the menu changes it — the guess is never the only thing on screen.
        let container = request.containers.first().cloned();
        let namespace = request.namespace.as_deref().unwrap_or("default").to_owned();
        let identity = (
            namespace,
            request.name.to_string(),
            container.as_ref().map(ToString::to_string),
        );
        if let Some(slot) = self.log_slot_of(&identity) {
            self.terminal_maximized = false;
            self.show_logs_slot(slot, cx);
            return;
        }
        // The stream already open does not move and does not stop: it keeps its slot, its tab, its
        // connection and its lines, and the reader flips back to it with one click. That is what
        // makes §16.3's "two" a number of connections rather than a number of tabs.
        let Some(slot) = self.free_log_slot() else {
            self.refuse_log_stream(cx);
            return;
        };
        self.active_log = slot;
        let stream = &mut self.streams[slot];
        stream.disconnect();
        stream.container = container;
        stream.request = Some(request);
        stream.log_filter.clear();
        stream.buffer.clear();
        stream.buffer_bytes = 0;
        stream.visible_log_indices.clear();
        stream.longest_visible_log_row = 0;
        stream.synced = 0;
        stream.filter_cursor = 0;
        stream.filter_frames_left = FILTER_PASS_FRAME_BUDGET;
        stream.filter_pass_deferred = false;
        stream.find_open = false;
        stream.find_query.clear();
        stream.find_hits.clear();
        stream.find_rows.clear();
        stream.find_active = 0;
        stream.log_selection = None;
        stream.dropped_lines = 0;
        stream.last_line_at_ms = None;
        stream.list_state.reset(0);
        stream.list_state.set_follow_mode(FollowMode::Tail);
        stream.attempts = 0;
        stream.follow = true;
        stream.following = true;
        stream.phase = LogPhase::Idle;
        self.write_log_filter_input("", cx);
        self.write_find_input("", cx);
        self.active_tab = DockTab::Logs(slot);
        self.terminal_maximized = false;
        self.log_cap_notice = None;
        self.reset_uniform_scroll();
        self.sync_focus_handles();
        eprintln!(
            "k8s-gpui: log target opened: {}, tail {}, timestamps {}, follow {}, wrap {}, {} of {} streams",
            log_target_label(self.log().request.as_ref(), self.log().container.as_deref()),
            self.log().history_lines,
            self.log().timestamps,
            self.log().follow,
            self.log().wrap,
            self.open_log_streams(),
            MAX_ACTIVE_LOG_STREAMS,
        );
        self.start_stream(cx);
    }
    fn show_logs_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        let changed = self.active_tab != DockTab::Logs(slot) || self.terminal_maximized;
        self.active_log = slot.min(self.streams.len().saturating_sub(1));
        self.active_tab = DockTab::Logs(slot);
        self.log_cap_notice = None;
        self.terminal_maximized = false;
        // One filter field, two streams: the field is the Dock's, and it is told which stream's
        // value it is showing. Without this a reader who filtered one Pod's logs and switched to
        // the other would find a filter they cannot see.
        //
        // The field's own report is held for the write, so the match set has to be asked for
        // explicitly: a stream that arrives with a filter already on it has no match set until
        // something scores it, and the list would answer "no matching lines" to a filter the
        // reader can see in the field and did not touch.
        let filter = self.log().log_filter.clone();
        self.write_log_filter_input(&filter, cx);
        if !filter.is_empty() {
            self.restart_filter(log_row_height(cx));
        }
        let find = self.log().find_query.clone();
        self.write_find_input(&find, cx);
        self.sync_focus_handles();
        if changed {
            cx.notify();
        }
    }

    /// Tells the reader the Dock is already holding as many log streams as it will.
    ///
    /// Two surfaces, because the reader can reach the mistake from two directions: the strip,
    /// which is on screen even with the body folded to nothing, and a toast, which is what the
    /// rest of the app answers an impossible request with. The strip text is the short one — a
    /// 28px band has room for a state, not a sentence — and the toast carries the names.
    fn refuse_log_stream(&mut self, cx: &mut Context<Self>) {
        let open: Vec<String> = self
            .slots_with_streams()
            .into_iter()
            .map(|slot| self.log_slot_label(slot))
            .collect();
        let title = LOG_STREAM_LIMIT_TITLE;
        let detail = format!("Open now: {}.", open.join(" · "));
        self.log_cap_notice = Some((SharedString::from(title), detail.clone()));
        // The toast alone does not redraw the Dock: `notify_with_detail` hands the message to the
        // shell's handler and nothing else, so without this the refusal would sit in the state and
        // never reach the strip until the next unrelated repaint.
        cx.notify();
        self.notify_with_detail(title, Severity::Warning, &detail, cx);
        eprintln!("k8s-gpui: log stream refused: {MAX_ACTIVE_LOG_STREAMS} already open, {open:?}");
    }

    // Terminal sessions and port forwards.

    /// Sets terminal and port forward services.
    ///
    /// The Dock treats a change of *context* as a change of cluster and drops what belonged to the
    /// old one. It used to keep them: the reader switched cluster with three shells open and the
    /// Dock kept all three, each chip naming the cluster it was actually on, in a window whose
    /// title bar, status bar and every table now said something else. Every shell is a live
    /// connection and every forward is a live tunnel into the cluster it was started against, so
    /// "it survived the switch" is not a feature — it is three ways to run a command against the
    /// wrong cluster by typing into a chip that looks like it belongs to this window.
    ///
    /// A change of *namespace* is not a change of cluster and keeps everything. A shell pinned to
    /// `team-a` is still in `team-a` after the reader looks at `team-b`, and its chip already says
    /// so; that is the reader comparing two namespaces, which is the reason the session exists.
    pub fn set_terminal_services(
        &mut self,
        services: Option<TerminalServices>,
        cx: &mut Context<Self>,
    ) {
        let context = services
            .as_ref()
            .and_then(|services| services.context.clone());
        if self.terminal_context() != context.as_deref() {
            self.close_sessions_off_cluster(context.as_deref(), cx);
        }
        self.terminal_services = services;
        cx.notify();
    }

    /// Drops everything the Dock is holding against a cluster other than `context`.
    ///
    /// `context` is the cluster being switched *to*. Reading it off `self.terminal_services`
    /// instead would test every session against the cluster it is leaving, which keeps exactly the
    /// sessions that should have gone.
    ///
    /// The log streams go with the sessions. A Pod name is not unique across clusters, so a stream
    /// that reopened against the new one would carry on filling the same tab with a different
    /// Pod's lines under the same `namespace/pod` label — the silent wrong answer the whole tab is
    /// named to prevent. It used to reconnect on the new factory and hope the reader noticed.
    fn close_sessions_off_cluster(&mut self, context: Option<&str>, cx: &mut Context<Self>) {
        let terminals = self.terminals.len();
        let forwards = self.forwards.len();
        self.terminals
            .retain(|entry| Self::session_is_on(entry, context));
        self.terminal_focus_handles.truncate(self.terminals.len());
        self.forwards
            .retain(|entry| Self::forward_is_on(entry, context));
        let closed = (terminals - self.terminals.len()) + (forwards - self.forwards.len());
        let slots = self.slots_with_streams();
        let streams = slots.len();
        for slot in slots {
            self.close_log_slot(slot, cx);
        }
        if closed == 0 && streams == 0 {
            return;
        }
        if self.terminal_split {
            self.close_terminal_split();
        }
        self.terminal_maximized = false;
        self.active_terminal = self
            .active_terminal
            .min(self.terminals.len().saturating_sub(1));
        self.sync_terminal_focus_handles();
        self.sync_focus_handles();
        eprintln!(
            "k8s-gpui: cluster changed to {context:?}: {closed} sessions and forwards and \
             {streams} log streams closed"
        );
        // Both kinds are named. A log tab that emptied itself with nothing said about it is the
        // same silent wrong answer one step later: the reader comes back to a closed pane and has
        // to work out whether they closed it or something else did.
        let mut parts = Vec::new();
        if closed > 0 {
            parts.push(format!(
                "{closed} session{}",
                if closed == 1 { "" } else { "s" }
            ));
        }
        if streams > 0 {
            parts.push(format!(
                "{streams} log stream{}",
                if streams == 1 { "" } else { "s" }
            ));
        }
        self.notify(
            &format!(
                "{} closed. {} belonged to the previous cluster.",
                parts.join(" and "),
                if closed + streams == 1 { "It" } else { "They" },
            ),
            Severity::Warning,
            cx,
        );
    }

    /// True when a session was started against the cluster the Dock is showing now.
    fn session_is_on(entry: &TerminalEntry, context: Option<&str>) -> bool {
        entry.request.context.as_deref() == context
    }

    /// The same for a port forward, which is a live tunnel into the same cluster.
    fn forward_is_on(entry: &ForwardEntry, context: Option<&str>) -> bool {
        entry.request.context.as_deref() == context
    }

    pub fn show_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.active_tab = DockTab::Terminal;
        self.sync_focus_handles();
        cx.notify();
    }

    fn terminal_available(&self) -> bool {
        self.terminal_services.is_some()
    }

    /// Whether the Dock holds the factories a *session* needs — a shell or a port forward.
    ///
    /// One answer for both because both are handed the same `TerminalServices` by the shell, and
    /// `panels/forwards.rs` needs it for one reason: its "nothing here yet" state has to be able
    /// to say what a reader can do about it. With no cluster there is no Service and no Pod to
    /// right-click, so the sentence that names that gesture is an instruction against a menu that
    /// is not on screen — the same defect the Dock's own `No log target` state had, one panel
    /// over. With one, the sentence is right.
    pub fn sessions_available(&self) -> bool {
        self.terminal_available()
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

    /// Selects a tab, moving the stream it names into the body, and puts the caret on the tab.
    ///
    /// The caret is part of the answer rather than an extra: the strip is a tab group, so a reader
    /// who moved the selection with the pointer and then pressed an arrow has to be somewhere the
    /// arrow can move from. `show_logs_slot` already notifies, so the second one here is the
    /// Terminal's.
    fn select(&mut self, tab: DockTab, window: &mut Window, cx: &mut Context<Self>) {
        match tab {
            DockTab::Logs(slot) => self.show_logs_slot(slot, cx),
            DockTab::Terminal => {
                self.active_tab = DockTab::Terminal;
                self.terminal_maximized = false;
                self.log_cap_notice = None;
                self.sync_focus_handles();
                cx.notify();
            }
        }
        window.focus(&self.tab_focus_handles[tab.slot()], cx);
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
            && self.active_tab == DockTab::Terminal
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

    /// Removes every session and forward, whatever cluster they are on.
    ///
    /// [`Self::set_terminal_services`] already closes what belonged to the cluster being left, so a
    /// cluster switch does not need this. It stays because it is the one call that drops the
    /// Dock's sessions on *losing* a cluster entirely, where there is no new one to compare
    /// against, and because it is the seam the shell owns for "there is no cluster any more".
    pub fn close_cluster_sessions(&mut self, cx: &mut Context<Self>) {
        if self.terminals.is_empty() && self.forwards.is_empty() {
            return;
        }
        self.terminals.clear();
        self.terminal_focus_handles.clear();
        self.forwards.clear();
        self.close_terminal_split();
        self.terminal_maximized = false;
        self.active_terminal = 0;
        self.sync_focus_handles();
        cx.notify();
    }

    pub fn phase(&self) -> &LogPhase {
        &self.log().phase
    }

    pub fn request(&self) -> Option<&LogRequest> {
        self.log().request.as_ref()
    }

    pub fn is_following(&self) -> bool {
        self.log().following
    }

    pub fn history_lines(&self) -> i64 {
        self.log().history_lines
    }

    pub fn selected_container(&self) -> Option<&SharedString> {
        self.log().container.as_ref()
    }

    /// Restarts a failed log stream.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.log_mut().attempts = 0;
        self.start_stream(cx);
    }

    fn start_stream(&mut self, cx: &mut Context<Self>) {
        self.start_stream_at(self.active_log, cx);
    }

    /// Connects the stream in `slot`, and only that one.
    ///
    /// A parked stream reconnects on its own timer, so the reconnect path has to name its stream
    /// rather than reach for "the one in the body" — otherwise a stream that fell over while the
    /// reader was reading the other one would be revived into the wrong buffer.
    fn start_stream_at(&mut self, slot: usize, cx: &mut Context<Self>) {
        let Some(stream) = self.log_slot_mut(slot) else {
            return;
        };
        stream.disconnect();
        let Some(request) = stream.request.clone() else {
            return;
        };
        let Some(factory) = self.factory.clone() else {
            self.log_mut_at(slot).phase = LogPhase::Unavailable(
                "No context connection. Select a context, then try again.".to_owned(),
            );
            self.trace_stream_at(slot, "unavailable");
            cx.notify();
            return;
        };
        let Some(stream) = self.log_slot_mut(slot) else {
            return;
        };
        stream.epoch = stream.epoch.wrapping_add(1);
        let epoch = stream.epoch;
        let options = log_options_for(stream);
        let (sink, mut receiver) = tokio::sync::mpsc::channel(LOG_EVENT_BUFFER);
        if let Some(stream) = self.log_slot_mut(slot) {
            stream.subscription = Some(factory(request, options, sink));
            stream.lines_received = 0;
            stream.phase = LogPhase::Connecting;
        }
        self.trace_stream_at(slot, "start");

        self.log_mut_at(slot).stream_task = Some(cx.spawn(async move |this, cx| {
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
                    .update(cx, |panel, cx| panel.deliver(slot, epoch, batch, cx))
                    .is_err()
                {
                    return;
                }
                if ended {
                    return;
                }
            }
        }));

        // The watchdog is armed on *every* connection, not only on a reconnect.
        //
        // It used to be gated on `attempts > 0`, which meant the one case nobody could escape was
        // the first one: open a Pod's logs, the request goes out, the API server accepts it and
        // then nothing ever arrives, and the pane spins on "Waiting for the first log line…"
        // forever with no `Reconnect` and no failure. `UI-SPEC` §9.3 requires a network timeout to
        // be visible inside ten seconds rather than after sixty, and a timeout that only exists
        // on the second attempt is not a timeout.
        let timeout_epoch = epoch;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECONNECT_WAIT).await;
            this.update(cx, |panel, cx| {
                let stalled = panel.log_slot(slot).is_some_and(|stream| {
                    stream.epoch == timeout_epoch
                        && stream.lines_received == 0
                        && matches!(stream.phase, LogPhase::Connecting)
                });
                if stalled {
                    panel.on_stream_ended_at(
                        slot,
                        "The log stream stopped before it sent data.".to_owned(),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn cancel_stream(&mut self) {
        if let Some(stream) = self.log_slot_mut(self.active_log) {
            stream.disconnect();
        }
    }

    /// Routes a batch to the stream that asked for it.
    ///
    /// The epoch identifies the sender and both streams are connected at once, so a batch that
    /// belongs to the parked stream has to reach the parked stream's buffer. It does *not* reach
    /// the list, the filter pass or the caret: those describe the pane the reader is looking at,
    /// and a parked stream's arriving lines are not on screen. The tab's dot is the one surface
    /// that has to know, and it reads the stream's own state.
    fn deliver(&mut self, slot: usize, epoch: u64, batch: Vec<LogEvent>, cx: &mut Context<Self>) {
        if slot != self.active_log {
            let Some(stream) = self.log_slot_mut(slot) else {
                return;
            };
            if stream.epoch != epoch {
                eprintln!(
                    "k8s-gpui: log stream dropped a stale batch: epoch {epoch}, current {}",
                    stream.epoch
                );
                return;
            }
            Self::absorb_batch(stream, batch);
            cx.notify();
            return;
        }
        self.on_log_batch(epoch, batch, cx);
    }

    /// The part of a batch every stream runs: the lines into the ring, the counters, the phase.
    ///
    /// Shared by both paths so a parked stream's line count and phase cannot drift from the
    /// visible one's, and so there is one place that decides a stream has started delivering.
    fn absorb_batch(
        stream: &mut LogStream,
        batch: Vec<LogEvent>,
    ) -> (usize, usize, bool, Option<String>) {
        let mut raws = Vec::new();
        let mut ended = None;
        for event in batch {
            match event.bounded() {
                LogEvent::Line(raw) => raws.push(raw),
                LogEvent::Ended(reason) => ended = Some(reason),
            }
        }
        if raws.is_empty() {
            return (0, 0, false, ended);
        }
        stream.last_line_at_ms = Some(Self::now_ms());
        let (appended, dropped, rebuilt) = Self::absorb_lines(stream, raws, LOG_BUFFER_MAX_BYTES);
        stream.lines_received += appended as u64;
        stream.dropped_lines = stream.dropped_lines.saturating_add(dropped as u64);
        stream.attempts = 0;
        if matches!(
            stream.phase,
            LogPhase::Connecting | LogPhase::Reconnecting { .. }
        ) {
            stream.phase = LogPhase::Streaming;
        }
        (appended, dropped, rebuilt, ended)
    }

    fn on_log_batch(&mut self, epoch: u64, batch: Vec<LogEvent>, cx: &mut Context<Self>) {
        if epoch != self.log().epoch {
            // A batch from a stream the Dock already replaced. The epoch is the only thing that
            // separates a late line from a current one, so say how many arrived late.
            eprintln!(
                "k8s-gpui: log stream dropped a stale batch: epoch {epoch}, current {}",
                self.log().epoch
            );
            return;
        }
        let was_connecting = matches!(
            self.log().phase,
            LogPhase::Connecting | LogPhase::Reconnecting { .. }
        );
        let (appended, dropped, rebuilt, ended) = Self::absorb_batch(self.log_mut(), batch);
        if appended > 0 {
            let row_height = log_row_height(cx);
            if rebuilt {
                // The byte limit rebuilt the buffer, so every stored index moved.
                self.restart_filter(row_height);
            } else {
                self.sync_list(appended, dropped, row_height);
            }
            if was_connecting && matches!(self.log().phase, LogPhase::Streaming) {
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

    /// Appends to the active stream, keeping both caps: `RING_CAPACITY` lines and `max_bytes`.
    ///
    /// The byte cap is the second bound, not a second copy of the first: a thousand lines of
    /// kilobyte-sized stack traces is a megabyte of `SharedString` in a list the reader is
    /// scrolling, and the line cap alone does not see that. It is a free function on the stream so
    /// the parked path can run the identical arithmetic without the panel's list state.
    fn append_log_lines_with_limit(
        &mut self,
        raws: Vec<String>,
        max_bytes: usize,
    ) -> (usize, usize, bool) {
        Self::absorb_lines(self.log_mut(), raws, max_bytes)
    }

    fn absorb_lines(
        stream: &mut LogStream,
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
        if stream.buffer_bytes.saturating_add(incoming_bytes) <= max_bytes {
            let overflow = stream
                .buffer
                .len()
                .saturating_add(appended)
                .saturating_sub(RING_CAPACITY);
            let evicted_bytes = (0..overflow)
                .filter_map(|index| stream.buffer.line(index))
                .map(|line| line.raw.len())
                .sum::<usize>();
            let (appended, dropped) = stream.buffer.push_many(raws);
            stream.buffer_bytes = stream
                .buffer_bytes
                .saturating_sub(evicted_bytes)
                .saturating_add(incoming_bytes);
            return (appended, dropped, false);
        }

        let mut combined = Vec::with_capacity(stream.buffer.len() + appended);
        for index in 0..stream.buffer.len() {
            if let Some(line) = stream.buffer.line(index) {
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
        stream.buffer = LogBuffer::new();
        stream.buffer.push_many(retained);
        stream.buffer_bytes = retained_bytes;
        (appended, 0, true)
    }

    fn sync_list(&mut self, appended: usize, dropped: usize, row_height: Pixels) {
        // An open find bar follows the stream, or the highlight is a snapshot of a pane the
        // reader is still watching move. It is one bounded pass over a capped buffer, and it
        // only runs while someone is actually searching.
        if self.log().find_open && (appended > 0 || dropped > 0) {
            self.rescore_find();
        }
        if self.log_view_is_filtered() {
            // Ring eviction moved every stored index, so the existing match set is rebased
            // before the pass scores the lines that arrived since the last one.
            self.rebase_filter(dropped);
            self.run_filter_pass(row_height);
            return;
        }
        if dropped > 0 {
            self.log_mut().list_state.splice(0..dropped, 0);
            self.log_mut().synced = self.log_mut().synced.saturating_sub(dropped);
        }
        if appended > 0 {
            let start = self.log().synced;
            self.log_mut().list_state.splice(start..start, appended);
            self.log_mut().synced = start + appended;
        }
        if self.log().log_filter.is_empty() {
            self.log_mut().longest_visible_log_row = self.log_mut().buffer.longest_message_index();
        }
        self.clamp_log_selection();
        if self.log().find_open {
            self.rebuild_find_rows();
        }
        self.request_uniform_follow();
    }

    /// Shifts the stored match set and the filter cursor after the ring dropped `dropped` lines
    /// from the front.
    fn rebase_filter(&mut self, dropped: usize) {
        if dropped == 0 {
            return;
        }
        debug_assert!(
            self.log()
                .visible_log_indices
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "the stored match set is sorted before the rebase"
        );
        // A stored index below the drop bound belongs to a line the ring discarded, so there is
        // nothing to point at any more. Removing it, instead of subtracting and clamping to zero,
        // is what keeps the set strictly increasing: the survivors shift by the same amount the
        // ring shifted the buffer, so they stay sorted and stay inside it.
        let cursor = self.log_mut().filter_cursor;
        self.log_mut()
            .visible_log_indices
            .retain(|index| *index >= dropped);
        for index in &mut self.log_mut().visible_log_indices {
            *index -= dropped;
        }
        self.log_mut().filter_cursor = cursor.saturating_sub(dropped);
        self.clamp_log_selection();
    }

    /// Scores at most one budget of buffered lines and merges the matches into the visible set.
    /// The cursor only moves forward, so a live stream costs the lines that arrived since the
    /// last pass instead of the whole buffer.
    fn score_filter_budget(&mut self) {
        let start = self.log().filter_cursor.min(self.log().buffer.len());
        let end = (start + FILTER_SCORING_BUDGET).min(self.log().buffer.len());
        if start >= end {
            return;
        }
        let mut matches = self
            .log()
            .buffer
            .matching_indices_in(start..end, &self.log().log_filter);
        matches.retain(|index| {
            self.log()
                .buffer
                .line(*index)
                .is_some_and(|line| self.log().log_level.accepts(line.severity))
        });
        self.log_mut().visible_log_indices.extend(matches);
        self.log_mut().filter_cursor = end;
        self.clamp_log_selection();
        let buffer = self.log().buffer.clone();
        let visible = self.log().visible_log_indices.clone();
        self.log_mut().longest_visible_log_row = longest_log_row(&buffer, &visible);
    }

    /// Continues the filter where it stopped. Returns true when another pass is still owed, which
    /// is the caller's cue to ask for the next frame.
    fn run_filter_pass(&mut self, row_height: Pixels) -> bool {
        if !self.log_view_is_filtered() || self.log().filter_cursor >= self.log().buffer.len() {
            return false;
        }
        self.score_filter_budget();
        self.refresh_filtered_list(row_height);
        self.log().filter_cursor < self.log().buffer.len()
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
            if self.log().filter_pass_deferred {
                self.log_mut().filter_pass_deferred = false;
                self.log_mut().filter_frames_left = FILTER_PASS_FRAME_BUDGET;
                self.trace_filter("complete");
            }
            return false;
        }
        if self.log().filter_frames_left == 0 {
            if !self.log().filter_pass_deferred {
                self.log_mut().filter_pass_deferred = true;
                self.trace_filter("deferred");
            }
            return false;
        }
        self.log_mut().filter_frames_left -= 1;
        cx.notify();
        true
    }

    /// Lifecycle line for the filter pass. Reports how much of the buffer is scored, never the
    /// query or the lines themselves: a filter query is user text and the lines are Pod output.
    fn trace_filter(&self, state: &str) {
        eprintln!(
            "k8s-gpui: log filter pass {state}: {}/{} lines scored, {} visible, filter {} chars, level {:?}",
            self.log().filter_cursor,
            self.log().buffer.len(),
            self.visible_log_count(),
            self.log().log_filter.chars().count(),
            self.log().log_level,
        );
    }

    /// Applies the current match set to the list. Only the row count changes, so this stays
    /// cheap enough to run once per batch.
    fn refresh_filtered_list(&mut self, row_height: Pixels) {
        self.log_mut().synced = self.log_mut().buffer.len();
        self.log()
            .list_state
            .reset_with_uniform_height(self.visible_log_count(), row_height);
        if !self.log().follow {
            self.log_mut().following = false;
        }
        self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
        if !self.log().following {
            self.log().list_state.pause_following_tail();
        }
        self.request_uniform_follow();
    }

    /// True when the free-text filter or the level scope hides part of the buffer.
    fn log_view_is_filtered(&self) -> bool {
        !self.log().log_filter.is_empty() || self.log().log_level != LogLevelScope::All
    }

    fn on_stream_ended(&mut self, reason: String, cx: &mut Context<Self>) {
        self.on_stream_ended_at(self.active_log, reason, cx);
    }

    /// Backs a stream off and schedules its own reconnection, naming the stream it belongs to.
    ///
    /// The backoff is per stream because the failure is: a second Pod whose log endpoint is not
    /// there has no business resetting the first Pod's attempt count, and a reader with two tabs
    /// open is watching two independent states on one strip.
    fn on_stream_ended_at(&mut self, slot: usize, reason: String, cx: &mut Context<Self>) {
        let Some(stream) = self.log_slot_mut(slot) else {
            return;
        };
        if let Some(mut subscription) = stream.subscription.take() {
            subscription.cancel();
        }
        // A reason the API server has already ruled on is not an interrupted connection. Giving it
        // the backoff ladder made a deleted Pod report "Reconnecting, attempt 3" for half a minute
        // — five identical requests against a server that has answered — and the reader had to
        // wait out a wait that could not end in an answer.
        let terminal = LogFailure::classify(&reason).is_terminal();
        if stream.lines_received > 0 {
            stream.attempts = 0;
        }
        stream.attempts += 1;
        if terminal || stream.attempts > MAX_RECONNECT_ATTEMPTS {
            stream.phase = LogPhase::Failed {
                reason: reason.clone(),
            };
            self.trace_reason_at(slot, "gave up", &reason);
            cx.notify();
            return;
        }
        let delay = reconnect_delay(stream.attempts);
        stream.phase = LogPhase::Reconnecting {
            attempt: stream.attempts,
            reason: reason.clone(),
        };
        let epoch = stream.epoch;
        self.trace_reason_at(
            slot,
            &format!("reconnect in {}ms", delay.as_millis()),
            &reason,
        );
        self.log_mut_at(slot).reconnect_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |panel, cx| {
                let waiting = panel.log_slot(slot).is_some_and(|stream| {
                    stream.epoch == epoch && matches!(stream.phase, LogPhase::Reconnecting { .. })
                });
                if waiting {
                    panel.start_stream_at(slot, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Lifecycle line for the log stream. Names the target, the epoch, and the counts, never the
    /// lines: a Pod log is unfiltered output and must not reach a terminal someone else can read.
    fn trace_stream(&self, state: &str) {
        self.trace_stream_at(self.active_log, state);
    }

    fn trace_stream_at(&self, slot: usize, state: &str) {
        let Some(stream) = self.log_slot(slot) else {
            return;
        };
        eprintln!(
            "k8s-gpui: log stream {state} on tab {slot}: {}, epoch {}, phase {}, tail {}, \
             buffered {} lines, {} lines received, {} dropped, {} bytes, timestamps {}, wrap {}, \
             follow {}",
            log_target_label(stream.request.as_ref(), stream.container.as_deref()),
            stream.epoch,
            stream.phase.label(),
            stream.history_lines,
            stream.buffer.len(),
            stream.lines_received,
            stream.dropped_lines,
            stream.buffer_bytes,
            stream.timestamps,
            stream.wrap,
            stream.follow,
        );
    }

    /// Lifecycle line for a stream failure, with the reason the cluster gave. The reason is the
    /// one string worth having in the log: it names what the request was rejected for.
    fn trace_reason_at(&self, slot: usize, state: &str, reason: &str) {
        let Some(stream) = self.log_slot(slot) else {
            return;
        };
        eprintln!(
            "k8s-gpui: log stream {state} on tab {slot}: {}, epoch {}, attempt {} of {}, \
             reason: {reason}",
            log_target_label(stream.request.as_ref(), stream.container.as_deref()),
            stream.epoch,
            stream.attempts,
            MAX_RECONNECT_ATTEMPTS,
        );
    }

    /// Restarts the filter from the top of the buffer. Typing replaces any pass that is still
    /// owed, so a burst of keystrokes cannot queue one pass per character.
    fn set_log_filter(&mut self, query: &str, cx: &mut Context<Self>) {
        self.write_log_filter_input(query, cx);
        if self.log().log_filter == query {
            return;
        }
        self.log_mut().log_filter = query.to_owned();
        self.restart_filter(log_row_height(cx));
        if self.log().find_open {
            self.rebuild_find_rows();
        }
        cx.notify();
    }

    /// Puts a value the panel decided on into the field. The field reports every value it
    /// takes, so the report is held for the duration, and a value that came from the field
    /// in the first place is left alone: the field is already inside its own update.
    fn write_log_filter_input(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.log_filter_writing.replace(true) {
            return;
        }
        self.log_filter_input
            .update(cx, |input, cx| input.set_text_pending(query, cx));
        self.log_filter_writing.set(false);
    }

    /// The find bar's query, rescored against the whole buffer.
    ///
    /// Find scores the raw line rather than the visible one, because its answer is "which of the
    /// ten thousand lines I retained contains this", and a reader who has filtered the list down
    /// to errors still has to be able to find a timestamp inside them.
    fn set_find_query(&mut self, query: &str, cx: &mut Context<Self>) {
        self.log_mut().find_query = query.to_owned();
        self.rescore_find();
        cx.notify();
    }

    /// Recomputes the hit set. It is one pass over the buffer on a keystroke, and the buffer is
    /// capped, so it is bounded work on the one path where the reader is waiting for an answer.
    fn rescore_find(&mut self) {
        self.log_mut().find_hits = if self.log_mut().find_query.is_empty() {
            Vec::new()
        } else {
            self.log().buffer.matching_indices(&self.log().find_query)
        };
        self.log_mut().find_active = 0;
        self.rebuild_find_rows();
    }

    /// Maps the buffer indices the find matched onto the rows the list draws.
    ///
    /// A filtered list is not the whole buffer, so a hit can be a line the reader cannot see. It
    /// is still a hit and it is still counted — the counter says "3 of 37", not "3 of 2" — but
    /// only the ones the list can actually draw are highlighted, because a hit count that walks
    /// past what Enter can reach is a count that lies about where Enter will go.
    fn rebuild_find_rows(&mut self) {
        let mut rows = Vec::with_capacity(self.log().find_hits.len());
        if self.log_view_is_filtered() {
            for index in &self.log().find_hits {
                if let Some(row) = self
                    .log()
                    .visible_log_indices
                    .iter()
                    .position(|row| row == index)
                {
                    rows.push(row);
                }
            }
        } else {
            rows.extend(self.log().find_hits.iter().copied());
        }
        self.log_mut().find_rows = rows;
    }

    /// Moves to the next or previous hit, wrapping at both ends.
    ///
    /// It wraps because a reader stepping through matches is looking for the one they have not
    /// seen yet, and a list that stops at the end silently says there are no more when there are.
    fn step_find(&mut self, forward: bool, row_height: Pixels, cx: &mut Context<Self>) {
        if self.log().find_hits.is_empty() {
            return;
        }
        let count = self.log().find_hits.len();
        self.log_mut().find_active = if forward {
            (self.log().find_active + 1) % count
        } else {
            (self.log().find_active + count - 1) % count
        };
        self.rebuild_find_rows();
        let Some(&index) = self.log().find_hits.get(self.log().find_active) else {
            return;
        };
        let target = if self.log_view_is_filtered() {
            self.log()
                .visible_log_indices
                .iter()
                .position(|row| *row == index)
        } else {
            Some(index)
        };
        if let Some(row) = target {
            // Scroll only. Moving the caret with it would put the active hit in the text
            // selection, and a selection paints over a find hit — so the hit the reader is on
            // would stop looking like a hit, and a reader who had a range selected before they
            // searched would silently lose it.
            self.scroll_log_row_into_view(row, row_height);
        }
        cx.notify();
    }

    /// Opens the find bar and puts the caret in it.
    fn open_find(&mut self, cx: &mut Context<Self>) {
        self.log_mut().find_open = true;
        self.rescore_find();
        let handle = self.find_input.read(cx).focus_handle(cx);
        if let Some(window) = cx
            .active_window()
            .or_else(|| cx.windows().into_iter().next())
        {
            cx.defer(move |cx| {
                let _ = window.update(cx, move |_, window, cx| window.focus(&handle, cx));
            });
        }
        cx.notify();
    }

    /// Closes the find bar, drops the highlight, and gives the keyboard back to the log list.
    ///
    /// Escape always has an effect, and this is the one place in the Dock where it can be
    /// ambiguous — clear the query, or leave find? It leaves. A reader who pressed Escape wants
    /// to be reading lines again, and the caret belongs where they are going.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.log().find_open {
            return;
        }
        self.log_mut().find_open = false;
        self.write_find_input("", cx);
        self.log_mut().find_query.clear();
        self.log_mut().find_hits.clear();
        self.log_mut().find_rows.clear();
        self.log_mut().find_active = 0;
        let previous = self.find_input.read(cx).focus_handle(cx);
        if previous.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }
        cx.notify();
    }

    /// Puts a value the panel decided on into the find field. The field reports every value it
    /// takes, so the report is held for the duration — the same one-way handoff the filter has.
    fn write_find_input(&mut self, query: &str, cx: &mut Context<Self>) {
        if self.find_writing.replace(true) {
            return;
        }
        self.find_input
            .update(cx, |input, cx| input.set_text_pending(query, cx));
        self.find_writing.set(false);
    }

    /// `⌘W` closes the view the Dock is showing, and the window stays.
    ///
    /// `UI-SPEC` §16.4. The Dock sits above the terminal the app also hosts, so a window-level
    /// close would take the reader's shell and their cluster with it; the Dock is the thing that
    /// knows what "the current tab" is, so it is the Dock that answers the chord. Everything else
    /// with a modifier — `⌘K`, `⌘C`, `⌘V` — is left to travel, because a terminal that swallows
    /// the app's clipboard would be a terminal nobody could paste into.
    fn close_current_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.active_tab == DockTab::Terminal {
            if self.active_terminal >= self.terminals.len() {
                return false;
            }
            let index = self.active_terminal;
            self.close_terminal(index, window, cx);
        } else if self.log_has_stream(self.active_tab) {
            // `⌘W` on a Logs tab closes that tab, and with §16.3's cap it is a tab the reader can
            // see and point at rather than a panel-wide reset: closing the one they are not looking
            // at is exactly how a reader makes room for a third.
            let slot = self.active_log;
            self.close_find(window, cx);
            self.close_log_slot(slot, cx);
        } else {
            self.close_log_target(window, cx);
        }
        cx.stop_propagation();
        cx.notify();
        true
    }

    /// Drops the log stream and everything that came with it, and puts the Logs tab back to the
    /// state it has before anything is asked for.
    ///
    /// This is what `⌘W` means on the Logs tab, and it is the Dock's own work rather than a
    /// command: the shell's `OpenLogs` opens a stream, so running it here would restart the very
    /// stream the reader asked to close, and would raise a toast about selecting a Pod when there
    /// was nothing to select. The state it leaves behind already has a control that opens a
    /// stream, so the loop closes without either side inventing an action.
    fn close_log_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_stream();
        self.log_mut().request = None;
        self.log_mut().container = None;
        self.log_mut().phase = LogPhase::Idle;
        self.log_mut().log_filter.clear();
        self.write_log_filter_input("", cx);
        self.close_find(window, cx);
        self.clear(cx);
    }

    fn set_log_level(&mut self, level: LogLevelScope, cx: &mut Context<Self>) {
        if self.log().log_level == level {
            return;
        }
        self.log_mut().log_level = level;
        self.restart_filter(log_row_height(cx));
        cx.notify();
    }

    /// Clears the stored match set and scores the first budget of the buffer. Any remaining work
    /// continues on the following frames.
    fn restart_filter(&mut self, row_height: Pixels) {
        self.log_mut().visible_log_indices.clear();
        self.log_mut().filter_cursor = 0;
        self.log_mut().log_selection = None;
        // A new query owes a new pass, so it also gets a full frame budget. Without the reset
        // the first keystroke of the next filter would inherit the previous pass's exhaustion.
        self.log_mut().filter_frames_left = FILTER_PASS_FRAME_BUDGET;
        self.log_mut().filter_pass_deferred = false;
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
            self.log().visible_log_indices.len()
        } else {
            self.log().buffer.len()
        }
    }

    /// Resets the view when nothing is filtered: every buffered line is visible, so the list
    /// takes the buffer length and the stored match set is not needed.
    fn rebuild_log_view(&mut self, row_height: Pixels) {
        self.log_mut().visible_log_indices.clear();
        self.log_mut().filter_cursor = 0;
        self.log_mut().longest_visible_log_row = self.log_mut().buffer.longest_message_index();
        self.log_mut().synced = self.log_mut().buffer.len();
        self.log()
            .list_state
            .reset_with_uniform_height(self.visible_log_count(), row_height);
        if !self.log().follow {
            self.log_mut().following = false;
        }
        self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
        if !self.log().following {
            self.log().list_state.pause_following_tail();
        }
        self.request_uniform_follow();
    }

    /// Whether the uniform list, the scroll owner in no-wrap mode, sits at the newest line.
    fn uniform_list_at_bottom(&self) -> bool {
        let state = self.log().scroll_handle.0.borrow();
        let at_bottom = state.deferred_scroll_to_item.is_some()
            || uniform_list_at_bottom(
                state.base_handle.offset().y,
                state.base_handle.max_offset().y,
            );
        drop(state);
        at_bottom
    }

    fn set_follow(&mut self, follow: bool, cx: &mut Context<Self>) {
        self.log_mut().follow = follow;
        self.log_mut().following = follow;
        self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
        if follow {
            self.request_uniform_follow();
        } else {
            self.log().list_state.pause_following_tail();
            self.log_mut()
                .scroll_handle
                .0
                .borrow_mut()
                .deferred_scroll_to_item = None;
        }
        cx.notify();
    }

    /// The way back from a paused view: scroll to the newest line and follow the tail again.
    ///
    /// `UI-SPEC` §16.3 makes this the answer to a paused follow, and it is the same answer the
    /// `End` key gives, so the two cannot leave the view in different states — which is how a
    /// "jump to latest" control ends up jumping without resuming.
    fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.log_mut().follow = true;
        self.scroll_log_to_end();
        self.log_mut().following = true;
        cx.notify();
    }

    /// Wall-clock milliseconds, for the one place the Dock reads the wall clock: how long ago the
    /// stream last said anything.
    fn now_ms() -> i64 {
        jiff::Timestamp::now().as_millisecond()
    }

    /// How long it has been since the stream last delivered a line, in the words a log pane uses.
    ///
    /// Two units, then it rounds: a reader watching a frozen pane wants to know whether it froze a
    /// moment ago or a quarter of an hour ago, and `2h` says that where `2h 14m 03s` does not.
    /// `panels::forwards` formats a phase age the same way, which is the point — one scale, one
    /// set of words — but the formatter is private to that panel, so this is a second copy. The
    /// shared home for it is `design::format`, which this change cannot reach.
    fn elapsed_text(age: Duration) -> String {
        let seconds = age.as_secs();
        if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3600 {
            format!("{}m {}s", seconds / 60, seconds % 60)
        } else {
            format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
        }
    }

    /// The note a stopped or reconnecting stream carries: how long since it last said anything.
    ///
    /// It is the difference between a pane that has died and a pane that has gone quiet, and those
    /// two look identical on screen. `None` while the stream is delivering, because a reader
    /// watching lines arrive does not need to be told that lines are arriving.
    fn last_line_note(&self) -> Option<SharedString> {
        if !matches!(
            self.log().phase,
            LogPhase::Failed { .. } | LogPhase::Reconnecting { .. }
        ) {
            return None;
        }
        let at = self.log().last_line_at_ms?;
        let age = Duration::from_millis((Self::now_ms() - at).max(0) as u64);
        Some(format!("Last line {} ago", Self::elapsed_text(age)).into())
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.log_mut().buffer.clear();
        self.log_mut().buffer_bytes = 0;
        self.log_mut().dropped_lines = 0;
        // The buffer is empty, so "no line for four minutes" is no longer a fact about the
        // stream. Leaving the stamp would have a stopped stream claim a line that is gone.
        self.log_mut().last_line_at_ms = None;
        self.log_mut().filter_cursor = 0;
        self.log_mut().log_selection = None;
        self.rebuild_log_view(log_row_height(cx));
        self.reset_uniform_scroll();
        cx.notify();
    }

    fn set_tail(&mut self, tail: TailLines, cx: &mut Context<Self>) {
        if self.log().history_lines == tail.value() {
            return;
        }
        self.log_mut().history_lines = tail.value();
        self.restart(cx);
    }

    /// Increases the requested history by one step of the shared ladder.
    fn load_earlier(&mut self, cx: &mut Context<Self>) {
        let Some(next) = TailLines::next_after(self.log().history_lines) else {
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
                format_count(self.log().history_lines as usize)
            ),
            Severity::Info,
            cx,
        );
    }

    fn set_container(&mut self, container: SharedString, cx: &mut Context<Self>) {
        if self.log().container.as_ref() == Some(&container) {
            return;
        }
        self.log_mut().container = Some(container);
        self.restart(cx);
    }

    fn set_timestamps(&mut self, timestamps: bool, cx: &mut Context<Self>) {
        if self.log().timestamps == timestamps {
            return;
        }
        self.log_mut().timestamps = timestamps;
        self.restart(cx);
    }

    /// Changing the source or request options restarts the stream and clears old lines.
    fn restart(&mut self, cx: &mut Context<Self>) {
        self.log_mut().buffer.clear();
        self.log_mut().buffer_bytes = 0;
        self.log_mut().dropped_lines = 0;
        self.log_mut().filter_cursor = 0;
        self.log_mut().log_selection = None;
        self.log_mut().visible_log_indices.clear();
        self.log_mut().longest_visible_log_row = 0;
        self.log_mut().synced = 0;
        self.log_mut().list_state.reset(0);
        self.log_mut().attempts = 0;
        self.log_mut().following = self.log_mut().follow;
        self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
        if !self.log().follow {
            self.log().list_state.pause_following_tail();
        }
        self.reset_uniform_scroll();
        self.start_stream(cx);
    }

    fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        self.log_mut().wrap = !self.log_mut().wrap;
        self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
        if !self.log().following {
            self.log().list_state.pause_following_tail();
        }
        self.request_uniform_follow();
        cx.notify();
    }

    /// Downloads buffered lines to the download directory.
    /// Uses the configuration or temporary directory when the download directory is unavailable.
    fn download(&mut self, cx: &mut Context<Self>) {
        if self.log().buffer.is_empty() {
            self.notify(
                "No log lines are available to download. Wait for log output, then select Download Logs.",
                Severity::Warning,
                cx,
            );
            return;
        }
        let lines = self.log().buffer.len();
        let text = self.log().buffer.text();
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
            .log()
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

    /// The stream state is worth reporting on every Dock tab: the log stream keeps running while
    /// the Terminal tab is open, and a maximised terminal hides the tab bar altogether.
    fn should_show_log_status(&self) -> bool {
        self.log().request.is_some() && !matches!(self.log().phase, LogPhase::Idle)
    }

    /// The header chip is a Dock-local summary of a state that needs acting on, so it is drawn for
    /// a paused view and for the phases that are not delivering — and for nothing else.
    ///
    /// It is drawn on either tab. The stream keeps running while the Terminal tab is open and
    /// behind a maximised pane, so `Reconnecting` on that tab has no other surface to report
    /// itself on; the tab's own dot is the one thing that would be left, and a dot next to the
    /// word `Terminal` says nothing about a log stream.
    ///
    /// A stream that is connecting, streaming or still waiting for a Pod to be pointed at is not
    /// one of those states. `UI-SPEC` §3.2's inversion says a healthy thing is not a thing that
    /// gets marked, and the chip used to mark exactly that: a `✓ Live` in `status.success`, on the
    /// one surface where the reader is deciding whether to trust what they are reading.
    fn header_status_is_useful(&self) -> bool {
        matches!(
            self.log().phase,
            LogPhase::Reconnecting { .. } | LogPhase::Failed { .. } | LogPhase::Unavailable(_)
        ) && !self.log_body_reports_failure()
    }

    /// True when the log body is the surface that reports this state, because it has nothing else
    /// to show. The banner then stays away instead of repeating the empty state one row above it.
    fn log_body_carries_state(&self) -> bool {
        self.log().buffer.is_empty() && self.log_failure_notice().is_some()
    }

    /// True when the log body already names this failure, in the empty state or in the banner. The
    /// header chip then steps aside: a state the Dock names in one row is not named in the next.
    fn log_body_reports_failure(&self) -> bool {
        self.active_tab.is_logs() && self.log_failure_notice().is_some()
    }

    /// The log failure the Dock has to report, in one record. A failed stream names its class, so
    /// a Pod that is still Pending is not reported as a Pod that is gone.
    fn log_failure_notice(&self) -> Option<LogFailureNotice> {
        match &self.log().phase {
            LogPhase::Reconnecting { attempt, reason } => Some(LogFailureNotice {
                title: "Reconnecting",
                guidance: format!(
                    "Log connection lost. Reconnect attempt {attempt} is in progress…"
                )
                .into(),
                severity: Severity::Warning,
                icon: design::glyph::action::reload(),
                detail: Some(reason.clone()),
                retry: false,
            }),
            LogPhase::Failed { reason } => {
                let failure = self.log().phase.failure()?;
                Some(LogFailureNotice {
                    title: failure.word(),
                    guidance: failure.guidance().into(),
                    severity: failure.severity(),
                    // A finished container is not an alert. It drew a warning triangle for a Job
                    // that ran to completion, so the one state with nothing wrong in it was the
                    // loudest thing in the row, and it wore the same glyph as a denied request.
                    // The mark follows the severity, which is what every other state already does.
                    icon: design::health_icon(failure.severity()),
                    detail: Some(reason.clone()),
                    retry: true,
                })
            }
            LogPhase::Unavailable(reason) => Some(LogFailureNotice {
                title: "No log source",
                guidance: "Select a context, then open Logs.".into(),
                severity: Severity::Muted,
                icon: IconName::TriangleAlert,
                detail: Some(reason.clone()),
                retry: false,
            }),
            _ => None,
        }
    }

    /// The Dock header: the two views the Dock can show, and the controls beside them.
    ///
    /// The strip is drawn here rather than handed to `TabBar`, and the reason is the surface it
    /// paints. `TabBar` brings its own band with it — `tokens.tab_bar` behind the tabs and a 1px
    /// `theme.border` under them — so a strip that already owns a `surface_chrome` background ends
    /// up two-tone with a rule down the middle of itself, and the component's tab fills the whole
    /// 28px height, so the active tab reads as a bar rather than as a 22px pill in a strip. The
    /// mockup is unambiguous about the intended shape: 28px of chrome, 22px of tab, 3px of strip
    /// above and below, a 4px corner, and a fill on the active tab only.
    ///
    /// What the component *did* own was the keyboard, and it never did: nothing in it moves focus
    /// when the selection changes, so the arrow keys, Home and End are handled here, and the focus
    /// handle behind each tab is the one the handler moves.
    ///
    /// This strip is the Dock's one permanent row. `UI-SPEC` §16.2 keeps it at
    /// `design::size::DOCK_TABS` even when the body has collapsed to nothing, because a Dock whose
    /// body is gone and whose tabs went with it cannot be switched back out of.
    fn render_tabs(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        self.sync_focus_handles();
        // The cap refusal outranks the transport chip: it is the answer to the request the reader
        // just made, and §16.3 asks for it by name. It is also the only Dock message that has to
        // survive a body folded to nothing, which is why it is on this row and not in the body.
        let status = self.log_cap_notice.as_ref().map(|_| {
            self.render_status_chip_with(
                LOG_STREAM_LIMIT_TITLE,
                Severity::Warning,
                LOG_STREAM_LIMIT_DESCRIPTION,
                cx,
            )
        });
        let status = status.or_else(|| {
            self.header_status_is_useful()
                .then(|| self.render_status_chip(cx))
        });
        let drawn = self.tabs();
        let mut tabs = h_flex()
            .id("dock-tab-bar")
            .flex_none()
            .h(design::size::DOCK_TABS)
            .items_center()
            .gap(space::XXS);
        for (index, tab) in drawn.iter().copied().enumerate() {
            let selected = tab == self.active_tab;
            // The Terminal tab counts its sessions once there is more than one, so the strip says
            // how many shells are open rather than repeating a constant word.
            let label: SharedString = match tab {
                DockTab::Terminal => terminal_tab_label(self.terminals.len()),
                DockTab::Logs(_) => SharedString::from(tab.label()),
            };
            // The strip names the object a tab is pointed at, so a reader with the strip collapsed
            // can still tell which tab holds the Pod they are looking at. It is the quietest thing
            // on the row, and it truncates: the name identifies the tab, it is not its content.
            // Which half truncates is the whole argument — see `tab_detail_parts`.
            //
            // The whole path is the accessible name and the drawn half is the strip, and both come
            // from one place: `tab_detail` is `tab_detail_parts` rejoined, so there is no way for
            // the two to say different things about the same Pod.
            let detail = self.tab_detail_parts(tab);
            let detail_label = self.tab_detail(tab);
            let severity = self.tab_status(tab);
            let focus = self.tab_focus_handles[tab.slot()].clone();
            let surface = design::role::surface_chrome(cx);
            let panel = cx.entity().downgrade();
            tabs = tabs.child(
                h_flex()
                    .id(("dock-tab", index))
                    .debug_selector(move || format!("dock-tab-{index}"))
                    .accessibility_id(format!("dock-tab-{index}"))
                    .role(Role::Tab)
                    .aria_label(tab_aria_label(&label, detail_label.as_ref(), severity))
                    .aria_selected(selected)
                    .aria_keyshortcuts("Enter Space ArrowLeft ArrowRight Home End")
                    .flex_none()
                    .h(DOCK_TAB_HEIGHT)
                    // The pill's own padding, symmetric at `space::SM`, so its contents sit on one
                    // horizontal spine with the strip's inset, the status chip and the trailing
                    // cluster. It used to be `space::XS` on the leading edge and `space::SM` on the
                    // trailing one, and the only reason for the asymmetry was the 2px rail that used
                    // to stand in the leading padding: the left half was narrower so the rail was not
                    // left floating in the middle of the pill. With the rail gone the two halves are
                    // the same number, and the strip's leading edge is one inset again.
                    .px(space::SM)
                    // `space::XS`, like the other two tab strips. It was `space::ICON` — the mark's
                    // own width used as a gap — which is how a 6px dot ended up 6px from its word
                    // while the Inspector's 14px icon sits 4px from its label in the same window.
                    // Two of the three already agree on `xs`; this is the third.
                    .gap(space::XS)
                    .items_center()
                    .rounded(design::radius::SM)
                    // No cursor. `UI-SPEC` §9.3: "表格行、侧栏行、工具栏按钮**都不改光标**
                    // （不要 `cursor: pointer`）。只有输入框是 I-beam". This was the only file in the
                    // crate that set one, so the Dock's tabs wore a hand while every table row,
                    // sidebar row and button around them wore the arrow — put the Dock beside
                    // anything else and the pointer is the first thing that reads as a different
                    // application.
                    .track_focus(&focus)
                    .tab_index(tab.slot() as isize)
                    .tab_stop(selected)
                    // Keyboard focus gets §3.2's ring: a 1px accent edge all the way round the pill,
                    // which a pill with a background of its own has room for. It is reserved at rest
                    // so focusing a tab never resizes it and never moves the name beside it.
                    .border_1()
                    .border_color(design::role::border_subtle(cx).alpha(0.))
                    .focus_visible(|this| this.border_color(design::focus::border(cx)))
                    .when(selected, |this| {
                        this.bg(design::role::accent_wash(cx))
                            .text_color(design::role::fg_primary(cx))
                            // The open tab still answers the pointer, which is the one state the
                            // other two strips have and this one did not: the wash is the accent
                            // over the pill's *own* plane, read with the two `design::state` steps
                            // `panels/inspector.rs` gives its selected tab. Nothing here invents a
                            // third number — hover is `state::hover_on`, press is `state::press_on`,
                            // both resolved against the same `surface_chrome` the wash sits on, so
                            // the pill reads as one object under the pointer rather than as a
                            // pointer resting on nothing.
                            .hover(|this| {
                                this.bg(design::state::hover_on(surface, design::role::accent(cx)))
                            })
                            .active(|this| {
                                this.bg(design::state::press_on(surface, design::role::accent(cx)))
                            })
                    })
                    .when(!selected, |this| {
                        this.text_color(design::role::fg_tertiary(cx))
                            // Wash only. The unselected hover used to raise the ink to
                            // `fg_secondary` as well, which was dead: the label, the scope and the
                            // name each state their own colour two lines below, because
                            // gpui-kit's `Label` re-applies `theme().foreground` over an ancestor's.
                            // The wash is the whole state, and it is the whole state in the other
                            // two tab strips too.
                            .hover(|this| this.bg(design::state::hover(cx, surface)))
                            // Pressed, resolved against the same `surface_chrome` the hover wash
                            // is, which is what `shell/panels.rs`'s unselected centre tab does.
                            // It was missing here, so a held pointer on an unselected Dock tab
                            // looked identical to a pointer resting on it — three states out of the
                            // four the state matrix asks for, on the one control that switches what
                            // the body below it shows.
                            .active(|this| this.bg(design::state::press(cx, surface)))
                    })
                    // *Selected*, read through the item's own surface and nothing else.
                    //
                    // The pill's whole selected treatment is three things, and all three live on
                    // the pill:
                    //
                    // - plane: the strip's own chrome, unchanged. The pill has no plane of its own,
                    //   so a selected tab and the strip behind it are the same surface plus a tint.
                    // - fill: `role::accent_wash` — 12% of the accent over the chrome — on the whole
                    //   pill, at `design::radius::SM`, so the tint follows the rounded silhouette.
                    // - ink: the label at `role::fg_primary` and `design::text::MEDIUM`, against the
                    //   unselected pill's `role::fg_tertiary` at `design::text::REGULAR`.
                    //
                    // The 2px accent rail that used to stand in the leading padding is gone, and the
                    // guide is explicit about why: "Show a selected navigation item, list row, or tab
                    // through the item's own surface: a selected fill, stronger foreground, or heavier
                    // weight. Do not add a leading-edge bar or one-sided border as the selection
                    // marker. It is a web template habit… it breaks the item's rounded silhouette and
                    // adds a second, competing edge to a column that already aligns on its text."
                    // It was also the only part of the treatment that survived greyscale, which was the
                    // argument for keeping it; the argument no longer holds, because the fill and the
                    // weight difference are two channels rather than one and both are on the item.
                    //
                    // Nothing is reserved for the marker now, so selecting a tab moves nothing. The
                    // one slot the pill keeps is the status dot below, and it is reserved for every
                    // tab whether or not it has a state: a mark only once there is something to
                    // report, under the semantic inversion. `UI-SPEC` §16.2.
                    .child(tab_status_dot(severity, cx))
                    // The label's ink is stated, not inherited. gpui-kit's `Label` renders
                    // `.text_color(cx.theme().foreground)` before it takes the caller's style, so
                    // the `fg.tertiary` the pill sets two children up is dead: an unselected tab
                    // and the selected one drew the same `#E8EAED` in Dark and the same `#101114`
                    // in Light, measured, and the strip's only tell was the wash and the rail.
                    // `UI-SPEC` §3's `selected` and §4.3's `active fg.primary · inactive
                    // fg.tertiary` both ask for the quieter ink, and the open-view bar 800px above
                    // this row does have it — a strip without it reads as two selected tabs.
                    //
                    // The weight is stated for the same reason, and it is the channel that replaces
                    // the rail: a `13/500` word beside a `13/400` one separates in greyscale, where
                    // a 12%-accent wash over a 1.02:1 chrome step does not.
                    .child(
                        label_text(label)
                            .font_weight(if selected {
                                design::text::MEDIUM
                            } else {
                                design::text::REGULAR
                            })
                            .text_color(if selected {
                                design::role::fg_primary(cx)
                            } else {
                                design::role::fg_tertiary(cx)
                            }),
                    )
                    .when_some(detail, |this, (scope, object)| {
                        // The scope gives way, the name does not. `UI-SPEC` §2.3 asks for the
                        // middle of an object name to be the part that shortens, because a k8s hash
                        // is at its tail, and the scope is a fact the title bar prints 60px above.
                        // One label with `.truncate()` cut the name instead, which is the half a
                        // reader cannot reconstruct: on this product's own cluster the tab read
                        // `kind-k8s-gpui-3n/kube-system/co…`.
                        //
                        // Two labels, so the flexbox does the measuring. A character-count estimate
                        // would put a `caption` line's error into a 140px row, which is the thing
                        // `UI-SPEC` §8's zero-roughness list is about.
                        let scope_row = div()
                            .id(("dock-tab-scope", index))
                            .debug_selector(move || format!("dock-tab-scope-{index}"))
                            .flex_shrink(1.)
                            .min_w(px(0.))
                            .max_w(tab_scope_max_width())
                            .overflow_hidden()
                            .child(
                                label_small(scope)
                                    .min_w(px(0.))
                                    .text_color(design::role::fg_tertiary(cx))
                                    .truncate(),
                            );
                        this.child(
                            h_flex()
                                .id(("dock-tab-detail", index))
                                .debug_selector(move || format!("dock-tab-detail-{index}"))
                                .flex_none()
                                .min_w(px(0.))
                                .items_center()
                                .gap(space::XXS)
                                .child(scope_row)
                                .children(object.into_iter().map(|part| {
                                    div()
                                        .id(("dock-tab-object", index))
                                        .debug_selector(move || format!("dock-tab-object-{index}"))
                                        .flex_none()
                                        .min_w(px(0.))
                                        .max_w(tab_name_max_width())
                                        .overflow_hidden()
                                        .child(
                                            label_small(part)
                                                .min_w(px(0.))
                                                .text_color(design::role::fg_tertiary(cx))
                                                .truncate(),
                                        )
                                })),
                        )
                    })
                    // The second stream's own way out. `⌘W` closes the tab the reader is on, and a
                    // reader who wants to close the *other* one without reading it first should not
                    // have to select it and look at it to do that. Painted on the tab the pointer is
                    // on and on the selected tab, reserved on every tab that has a stream, so no tab
                    // changes width as the pointer crosses the row.
                    .when(self.log_has_stream(tab), |this| {
                        let DockTab::Logs(slot) = tab else {
                            return this;
                        };
                        this.child(self.render_log_tab_close(slot, selected, cx))
                    })
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        if event.modifiers.shift || event.modifiers.control {
                            return;
                        }
                        panel
                            .update(cx, |panel, cx| panel.select(tab, window, cx))
                            .ok();
                        cx.stop_propagation();
                    }),
            );
        }
        h_flex()
            .id("dock-tabs")
            .debug_selector(|| "dock-tabs-row".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::DOCK_TABS)
            .px(space::SM)
            .items_center()
            .tab_group()
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            // No rule above the strip. The boundary between the centre workspace and the Dock is
            // drawn by the shell's Dock splitter — the interactive boundary, with its own 1px line
            // at rest, a 20px hit area and four states — and the guide's rule is that one owner
            // draws a boundary. A second hairline here would sit ten pixels under the splitter's own
            // line and read as two rules with a gap between them, which is the "every element gets
            // a border" failure the design removes seventy percent of its strokes to avoid. The
            // plane change below is what makes the edge legible; the line on it is not this row's
            // to draw.
            //
            // No rule under the strip either: the band below it is the toolbar, which is the same
            // chrome, and a 1px line between two surfaces of the same lightness separates nothing —
            // it just makes the Dock look like it has one more border than it does. The boundary
            // that carries information is the one at the bottom of the chrome band, and the toolbar
            // draws it.
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.context_stack_ready.set(true);
                if event.keystroke.modifiers.control
                    || event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.platform
                {
                    return;
                }
                let tabs = this.tabs();
                let count = tabs.len();
                let index = tabs
                    .iter()
                    .position(|tab| *tab == this.active_tab)
                    .unwrap_or(0);
                let target = match event.keystroke.key.as_str() {
                    "enter" | "return" | "space" => index,
                    "left" | "up" => (index + count - 1) % count,
                    "right" | "down" => (index + 1) % count,
                    "home" => 0,
                    "end" => count - 1,
                    _ => return,
                };
                let Some(tab) = tabs.get(target).copied() else {
                    return;
                };
                this.select(tab, window, cx);
                cx.stop_propagation();
            }))
            .child(
                h_flex()
                    .id("dock-tab-list")
                    .role(Role::TabList)
                    .aria_label(DOCK_TAB_LIST_LABEL)
                    .h_full()
                    .flex_none()
                    // Without a floor the strip's own tabs are the first thing the row squeezes when
                    // a status chip takes room, and the strip is the only way back into the Dock.
                    .min_w(px(0.))
                    .overflow_hidden()
                    .items_center()
                    .child(tabs),
            )
            .child(div().flex_1().min_w(px(0.)))
            .when_some(status, |this, status| this.child(status))
            // One cluster, so the three trailing controls share a frame: the same 8px inset the
            // strip gives its own leading edge, one `space::XS` between neighbours, and the close
            // one step further out.
            //
            // The gap was `space::XXS`, which is 2px. These are three separate 24px hit targets,
            // not three pills: a pointer aimed between `⋯` and `⌃` had a 2px seam to find, and
            // `design::size::HIT_MIN` — the width a target may not be smaller than — was being
            // spent on boxes whose edges were 2px apart, so in practice the pair was one 50px
            // target. One `space::XS` step is the smallest gap in the ladder that separates two
            // adjacent boxes rather than abutting them, and it is the same step the close already
            // takes for itself, so the cluster now reads 4 / 8 instead of 2 / 6.
            //
            // The extra room before `×` is separation, not decoration: folding the body and hiding
            // the Dock are different gestures on different objects, and the guide asks for "a
            // destructive decision can gain separation". `render_close_control` owns the step.
            //
            // Nothing in the group is made quieter than its neighbours to carry that weight.
            // `design::icon::incidental` would put the close at `fg_tertiary` directly under two
            // `fg_secondary` glyphs, and `UI-SPEC`'s first rule for this sweep is that a glyph in
            // the tertiary tier beside secondary text reads as DISABLED — the close is a live
            // control, so space says it is the last one and ink does not.
            .child(
                h_flex()
                    .id("dock-strip-controls")
                    .debug_selector(|| "dock-strip-controls".to_owned())
                    .flex_none()
                    .items_center()
                    .gap(space::XS)
                    .child(self.render_overflow_menu(cx))
                    .child(self.render_collapse_control(window, cx))
                    .child(self.render_close_control(window, cx)),
            )
            .into_any_element()
    }

    /// The severity a tab's dot wears.
    ///
    /// The Terminal tab has no opinion of its own: a session that ends is reported by that
    /// session's chip, and a dot on the tab would be a second place for the same verdict to be
    /// wrong.
    fn tab_status(&self, tab: DockTab) -> Option<Severity> {
        let DockTab::Logs(slot) = tab else {
            return None;
        };
        let phase = self.log_slot_phase(slot);
        match phase {
            // A stream that is delivering has nothing to report, and `UI-SPEC` §3.2's inversion
            // means a healthy thing is not a thing that gets marked.
            LogPhase::Streaming | LogPhase::Idle | LogPhase::Connecting => None,
            // Reconnecting is different from connecting: the first second of every stream is
            // ordinary, and a stream that has already broken once is not.
            LogPhase::Reconnecting { .. } => Some(Severity::Warning),
            LogPhase::Failed { .. } => Some(
                // A container that finished is not a stopped stream, so it does not wear the
                // tab's error dot. The tab's job is to make the reader notice a dead stream, and
                // a red dot for a Job that completed trains them to ignore the red one.
                self.log_slot(slot)
                    .and_then(|stream| stream.phase.failure())
                    .map_or(Severity::Error, |failure| failure.severity()),
            ),
            // No log source is a state the reader has to act on, and the dot is how it says so
            // while the reader is on the Terminal tab and the log body is not on screen to say
            // it in words.
            LogPhase::Unavailable(_) => Some(Severity::Warning),
        }
    }

    /// The object a tab is pointed at, or `None` while the tab has no object behind it.
    ///
    /// Both tabs name the object the same way — cluster, namespace, name, container — because a
    /// strip that reads `default/web-0` beside `kind-dev/team-a/web-0:app` is telling the reader
    /// two different facts about the same Pod, and only one of them is the whole one. This is the
    /// whole path: it is what the accessible name, the tooltip and the tab's own aria label say.
    /// The strip draws the last two segments of it — see [`Self::tab_detail_parts`].
    fn tab_detail(&self, tab: DockTab) -> Option<SharedString> {
        let (scope, object) = self.tab_detail_parts(tab)?;
        let mut whole = scope.to_string();
        for part in object {
            whole.push_str(&part);
        }
        Some(whole.into())
    }

    /// The scope half and the object half of [`Self::tab_detail`], split at the last `/`.
    ///
    /// The two are drawn as two labels so the flexbox does the measuring. No character-count
    /// estimate is involved, which is the only way this stays right on a proportional face at a
    /// reader's configured font.
    ///
    /// The split is at the last `/` and the *cluster* is dropped from the drawn half, which is the
    /// part of the string that was making the strip unreadable. It used to be drawn whole under one
    /// cap with a tail truncation, and the tail is the object: on a 1920px window
    /// `kind-k8s-gpui-3n/kube-system/coredns-559f6c778d-jwghp` came out as
    /// `kind-k8s-gpui-3n/kube-system/co…`, so a reader with two log tabs open could not tell which
    /// Pod either of them held. §2.3 asks for the *middle* of a name to shorten because a k8s hash
    /// is at its tail, and the cluster is a fact the title bar prints 60px above as its own `▾`.
    ///
    /// The cluster is not in the tooltip or the accessible name either — that is [`Self::tab_detail`]
    /// — so a reader who needs to know which cluster a stale tab belongs to still has it. What
    /// stays on the strip is the namespace, which is the half that is *not* otherwise on screen:
    /// with `All namespaces` selected, two tabs on Pods from different namespaces are the case
    /// where it is the only thing telling them apart.
    fn tab_detail_parts(&self, tab: DockTab) -> Option<(SharedString, Vec<SharedString>)> {
        let whole: SharedString = match tab {
            DockTab::Terminal => self
                .terminals
                .get(self.active_terminal)
                .map(|entry| terminal_entry_title(entry, self.terminal_context()))?,
            DockTab::Logs(slot) => {
                let stream = self.log_slot(slot)?;
                let request = stream.request.as_ref()?;
                let context = self.terminal_context();
                let cluster = context_label(context);
                let namespace = namespace_label(request.namespace.as_deref());
                let container = stream
                    .container
                    .as_ref()
                    .map(|container| format!(":{container}"))
                    .unwrap_or_default();
                format!("{cluster}/{namespace}/{}{container}", request.name).into()
            }
        };
        // Split into segments and keep the last two: the namespace is the scope half, everything
        // after it is the name. A title with no `/` in it is all name, and a title with one is all
        // scope — a local shell's whole identity is the namespace it was started in.
        let mut segments: Vec<&str> = whole.split('/').collect();
        let Some(name) = segments.pop() else {
            return Some((SharedString::default(), vec![whole]));
        };
        let scope = segments
            .pop()
            .map_or_else(String::new, |namespace| format!("{namespace}/"));
        Some((SharedString::from(scope), vec![SharedString::from(name)]))
    }

    /// The stream in `slot`, or `None` for a slot outside the cap.
    fn log_slot(&self, slot: usize) -> Option<&LogStream> {
        self.streams.get(slot)
    }

    /// The same, mutably.
    fn log_slot_mut(&mut self, slot: usize) -> Option<&mut LogStream> {
        self.streams.get_mut(slot)
    }

    fn log_slot_phase(&self, slot: usize) -> LogPhase {
        self.log_slot(slot)
            .map_or(LogPhase::Idle, |stream| stream.phase.clone())
    }

    /// True when the tab names a stream that is actually open. The first `Logs` tab exists with
    /// nothing behind it so the strip always has a way to reach the panel, and an empty slot draws
    /// no dot and no close control for the same reason a Terminal tab draws neither.
    fn log_has_stream(&self, tab: DockTab) -> bool {
        matches!(tab, DockTab::Logs(slot) if self.log_slot(slot).is_some_and(|s| s.request.is_some()))
    }

    /// The `×` that closes one log stream's tab, and only that stream.
    ///
    /// It is inside the tab rather than beside it because a Dock-level `×` closes the whole Dock and
    /// a reader who wants to drop the second Pod's stream should not lose the Dock to do it. The
    /// hit area is `design::size::HIT_MIN` wide and the full pill tall, so it is findable with a
    /// pointer, and the press stops before the tab's own press so one click cannot both select and
    /// close.
    ///
    /// It is hover-revealed, which the guide allows only for "a hover-revealed icon… a shortcut to
    /// a command that remains reachable elsewhere". It is: the tab carries the same close chord the
    /// Dock-level `×` runs, so selecting the tab and pressing it closes that stream. What was
    /// missing is the *name* — the control declared no role and no accessible label, so the one
    /// button inside the tab that is not the tab announced as body text. It names the stream and
    /// the result, and the tooltip carries the same words.
    fn render_log_tab_close(
        &self,
        slot: usize,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.entity().downgrade();
        let target = self.log_slot_label(slot);
        let name = format!("Close the log stream for {target}");
        div()
            .id(("dock-tab-close", slot))
            .debug_selector(move || format!("dock-tab-close-{slot}"))
            .accessibility_id(format!("dock-tab-close-{slot}"))
            .role(Role::Button)
            .aria_label(name.clone())
            .flex_none()
            .w(design::size::HIT_MIN)
            .h_full()
            .flex()
            .justify_center()
            .items_center()
            // §9.3 again: the strip does not change the cursor.
            // Reserved on every tab and painted on the one the pointer is on or the reader is on,
            // the same rule the log row's copy control follows. The slot is always in the flow so
            // selecting a tab never moves the name next to it.
            .opacity(if selected { 1.0 } else { 0.0 })
            .hover(|this| this.opacity(1.0))
            .tooltip(command_tooltip(format!(
                "Close the log stream for {target}."
            )))
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                panel
                    .update(cx, |panel, cx| panel.close_log_slot(slot, cx))
                    .ok();
                cx.stop_propagation();
            })
            .child(
                Icon::new(IconName::X)
                    .with_size(Size::Size(design::icon::IN_TOOLBAR))
                    .text_color(design::icon::resting(cx)),
            )
            .into_any_element()
    }

    /// Closes the log stream in `slot` and gives its tab to whatever is left.
    ///
    /// Closing the last one does not leave the strip empty: it goes back to the single `Logs` tab
    /// with nothing behind it, which is the state §16.2 draws and the one whose toolbar carries
    /// the `Open Logs` control.
    fn close_log_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        if !self.log_has_stream(DockTab::Logs(slot)) {
            return;
        }
        let label = self.log_slot_label(slot);
        if let Some(stream) = self.log_slot_mut(slot) {
            stream.disconnect();
            *stream = LogStream::empty();
        }
        self.log_cap_notice = None;
        // Closing a stream the reader is not looking at changes nothing they can see: the body stays
        // on the stream it was on and the field keeps the filter that stream is showing. Closing the
        // one in the body hands the body to whatever is left, or to the empty first slot when there
        // is nothing left — the state §16.2 draws, with the `Open Logs` control back in the band.
        //
        // Only a reader who was on a log tab is moved off it. Closing a *background* log stream
        // used to switch the strip to the Logs tab, so tidying up a cluster switch dropped a reader
        // out of the shell they were working in to look at an empty log pane.
        if self.active_tab.is_logs() {
            self.active_log = self.slots_with_streams().first().copied().unwrap_or(0);
            self.active_tab = DockTab::Logs(self.active_log);
            self.write_log_filter_input("", cx);
            self.write_find_input("", cx);
        }
        self.sync_focus_handles();
        eprintln!(
            "k8s-gpui: log stream closed: {label}, {} streams open",
            self.open_log_streams()
        );
        cx.notify();
    }

    /// The context every session in the Dock is labelled with, so the tab strip and the session
    /// chips cannot disagree about which cluster a session belongs to.
    fn terminal_context(&self) -> Option<&str> {
        self.terminal_services
            .as_ref()
            .and_then(|services| services.context.as_deref())
    }

    /// Collapse the Dock's body without closing it.
    ///
    /// `UI-SPEC` §16.2 draws `⌃` beside `×` and the two are not the same gesture: `×` hides the
    /// Dock, `⌃` leaves the strip on screen with nothing under it. They are told apart by shape as
    /// well as by name, so one is a chevron and the other a cross.
    ///
    /// The chevron is the collapsed state's whole answer: a folded Dock is a strip with no body,
    /// and the one control that changes that says so with its direction — down when the body is
    /// gone and there is something below to bring back, up while there is something to fold. The
    /// tooltip and the accessible name carry the same word, so the gesture is legible to a reader
    /// who cannot see the icon.
    fn render_collapse_control(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let collapsed = self.body_collapsed(window);
        let label = if collapsed {
            DOCK_EXPAND_LABEL
        } else {
            DOCK_COLLAPSE_LABEL
        };
        div()
            .id("dock-collapse")
            .debug_selector(|| "dock-collapse".to_owned())
            .flex_none()
            .h(design::size::ICON_BUTTON)
            .items_center()
            .track_focus(&self.collapse_focus)
            // The wrapper is the stop: it holds the handle the strip moves focus with, draws the
            // ring, and answers the key that presses the control, because the button below keeps
            // its handle to itself. See `render_close_control`. The 1px is reserved at rest so
            // focusing it never resizes it and never moves the two controls beside it.
            .border_l(design::border::FOCUS_RAIL)
            .border_color(design::role::border_subtle(cx).alpha(0.))
            .focus_visible(|this| this.border_color(design::focus::border(cx)))
            .tooltip(dock_control_tooltip(label, None))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if presses(&event.keystroke) {
                    cx.stop_propagation();
                    this.toggle_body_collapse(window, cx);
                }
            }))
            .child(
                Button::new("dock-collapse-button")
                    // No glyph size is stated on any `Button` icon in this file. gpui-kit's
                    // `Button` applies `content_style`'s `icon_size`, or `box * 0.75` when it is
                    // not given one, *after* the caller's own size — so the size on the `Icon`
                    // is overwritten and never reaches the screen, and stating one here is a
                    // claim about a number that is not drawn.
                    //
                    // The ink is stated for the opposite reason and is not optional: the size is
                    // overwritten, but the *colour* is not read at all. `Button` puts its own
                    // `.text_color()` on the root and the `Icon` resolves a missing one to the
                    // theme foreground, so a ghost button with no ink on its `Icon` is a control
                    // painted a step above every other control in the window. See
                    // `render_overflow_menu`.
                    .icon(
                        Icon::new(if collapsed {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronUp
                        })
                        .text_color(design::icon::resting(cx)),
                    )
                    .ghost()
                    .with_size(Size::Size(design::size::ICON_BUTTON))
                    .tab_index(2isize)
                    .tab_stop(false)
                    .accessibility_label(label)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.toggle_body_collapse(window, cx)),
                    ),
            )
            .into_any_element()
    }

    /// Hands whichever field is open the keystrokes the app's keymap binds no answer for.
    ///
    /// The app installs its own key bindings and binds no editing key, so a field's own context
    /// answers nothing, and the band is its DOM ancestor: the band sees the keystroke first and
    /// passes on the one it owns.
    ///
    /// There is one handler for both fields, not one each, because they share the band and
    /// `stop_propagation` is not scoped to a handler: with two, whichever was registered first
    /// silently owned Escape, and the second one could only be reached by guessing. The rule the
    /// reader learns is one rule — Escape leaves the field that has the keyboard — and one handler
    /// is what keeps it one rule.
    fn on_toolbar_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let escape = event.keystroke.key == "escape";
        if self.log().find_open {
            let row_height = log_row_height(cx);
            match event.keystroke.key.as_str() {
                // The field gave Escape up when it was built, so this is the only Escape there
                // is: one press closes the bar, drops the highlight, and gives the keyboard back
                // to the rows. `UI-SPEC` §9.4 requires Escape to always do something, and a find
                // bar whose Escape only empties the field has spent the key on half the answer.
                "escape" => {
                    self.close_find(window, cx);
                    cx.stop_propagation();
                }
                "enter" | "return" => {
                    self.step_find(!event.keystroke.modifiers.shift, row_height, cx);
                    cx.stop_propagation();
                }
                // Every other key belongs to the field's own editing context. Intercepting it here
                // and forwarding it would have been the obvious thing to write, and it silently
                // drops the character: the field's forwarding helper answers navigation keys with
                // actions, and a letter is not one of them.
                _ => {}
            }
            return;
        }
        if !escape {
            return;
        }
        // Escape clears the filter and stays there: a reader who is filtering is not asking to be
        // dismissed out of the Dock.
        let keystroke = event.keystroke.clone();
        self.log_filter_input.update(cx, |input, cx| {
            input.handle_keystroke(&keystroke, window, cx)
        });
        cx.stop_propagation();
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
            .h(design::size::ICON_BUTTON)
            .items_center()
            // One step out from the two controls beside it. The strip's group gives its members a
            // 2px gap, and hiding the Dock is not the same kind of gesture as the other two — it
            // takes the whole region off screen rather than changing what the region shows — so it
            // gets `space::XS` on top of that and reads as the last control rather than as the
            // third one. The padding it used to carry was 8px on *both* sides, which is what made
            // this box wider than its two neighbours and left the row's trailing edge ragged.
            .ml(space::XS)
            .track_focus(&self.close_focus)
            // The wrapper is the stop: it holds the handle the header moves focus with, draws the
            // ring, and answers the key that presses the control, because the button below keeps
            // its handle to itself and cannot be focused from here. The tooltip lives on the
            // wrapper for the same reason, and because it is the one hint that carries a keycap.
            // The 1px is reserved at rest, so focusing this never resizes it.
            .border_l(design::border::FOCUS_RAIL)
            .border_color(design::role::border_subtle(cx).alpha(0.))
            .focus_visible(|this| this.border_color(design::focus::border(cx)))
            .tooltip(dock_control_tooltip(DOCK_CLOSE_LABEL, chord))
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, window, cx| {
                if presses(&event.keystroke) {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(crate::shell::ToggleDock), cx);
                }
            }))
            .child(
                Button::new("dock-close-button")
                    // Stated, for the reason `render_overflow_menu` gives: a ghost `Button` never
                    // hands its own `.text_color()` to the `Icon` inside it, so without this the
                    // one control that hides the whole Dock was painted a step above the two
                    // beside it and a step above the Dock's own `×` on the tab pill.
                    .icon(Icon::new(IconName::X).text_color(design::icon::resting(cx)))
                    .ghost()
                    // An icon button is 24px (`UI-SPEC` §4.6), not the 28px
                    // control height. The strip is 28px and the wrapper above
                    // reserves a 1px border for its focus ring, so a 28px button
                    // is 30px of control in a 28px row and overflows it.
                    .with_size(Size::Size(design::size::ICON_BUTTON))
                    .tab_index(2isize)
                    .tab_stop(false)
                    .accessibility_label(DOCK_CLOSE_LABEL)
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(crate::shell::ToggleDock), cx);
                    })),
            )
            .into_any_element()
    }

    /// The Dock-local state word. A failure shows its class name and nothing else: the reason and
    /// the next step belong to the log body, which is the one surface that reports the whole
    /// failure, so the chip carries no disclosure of its own.
    ///
    /// A stream that is delivering says nothing here. It used to say `✓ Live` in `status.success`,
    /// which is the one thing `UI-SPEC` §3.2's semantic inversion forbids: a healthy thing is not
    /// a thing that gets marked, and the tab's dot already reads "nothing to report" for exactly
    /// this state. Two surfaces, one fact, two answers — the dot grey and the chip green — is how a
    /// reader learns to distrust both. The chip is a mark for the two states that need acting on.
    fn render_status_chip(&self, cx: &Context<Self>) -> AnyElement {
        let (label, severity) = self.log().phase.failure().map_or_else(
            || (self.log().phase.label(), self.log().phase.severity()),
            |failure| (failure.word(), failure.severity()),
        );
        self.render_status_chip_with(label, severity, label, cx)
    }

    /// The strip's one state word, with a full sentence behind it.
    ///
    /// `status_message` draws gpui-kit's chip, which is a full-height bordered pill; in a 28px
    /// strip that is a box with only two sides drawn. The word and the dot are all this row needs,
    /// and they are what the rest of the Dock's states are drawn with. The long form is the
    /// accessible name and the tooltip, because a 28px band has room for a state and not a story —
    /// and because the two messages that have to be read in full (a stream that stopped, and a
    /// third stream that was refused) are the two a reader is most likely to miss.
    fn render_status_chip_with(
        &self,
        label: &str,
        severity: Severity,
        description: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id("dock-log-status")
            .debug_selector(|| "dock-log-status".to_owned())
            .flex_none()
            .h(DOCK_TAB_HEIGHT)
            .px(space::SM)
            .gap(space::XS)
            .items_center()
            .rounded(design::radius::SM)
            // The chip is `flex_none` beside a `flex_1` spacer, so an unbounded label is the one
            // thing on this row that can push `⋯` `⌃` `×` off the end of it, or squeeze the tabs
            // into their own clip. The longest label is the cap refusal — a sentence, because the
            // cap is the one message the reader has to read before their next request does nothing —
            // so it is the label that gives way, and the sentence stays in the tooltip and the
            // accessible name. 256px holds that sentence whole at 11px, which is the width it has to
            // have to be worth printing it in a 28px row at all.
            .max_w(design::size::ROW * 8.)
            .min_w(px(0.))
            .overflow_hidden()
            .role(Role::Status)
            .aria_label(description)
            // The mark and the word wear the same severity's two inks. They used to be
            // `status_for` and a flat `fg.secondary`, so a *failed* stream wore a red dot beside a
            // grey word — two answers to one state, and the word is the one a reader actually
            // reads. `status_word_for` is the same hue one step along, which is what the role layer
            // means by "a 6px dot and a 12px label do not read at the same contrast".
            .child(
                div()
                    .flex_none()
                    .w(design::size::STATUS_DOT)
                    .h(design::size::STATUS_DOT)
                    .rounded_full()
                    .bg(design::role::status_for(severity, cx)),
            )
            .child(
                label_small(label)
                    .min_w(px(0.))
                    .text_color(design::role::status_word_for(severity, cx))
                    .truncate(),
            )
            .tooltip(command_tooltip(description.to_owned()))
            .into_any_element()
    }

    /// The Logs panel's own row, at `design::size::DOCK_TOOLBAR`.
    ///
    /// `UI-SPEC` §16.2 draws it at the *bottom* of the panel — `[Follow ⏸] [level: all ▾] [Filter… 18,402 lines]`
    /// under the lines — and so does `docs/mockup/secondary.html` screen 4. It sat above the body
    /// until now, which put a 28px band of chrome between the reader and the first line of the
    /// thing they opened the Dock to read, and left the log rows ending against a bare edge with
    /// nothing framing them. The arrangement inside the band is unchanged and is the point rather
    /// than the inventory: the two controls that change what the view *is* sit on the left where
    /// the eye lands, and the two numbers that say what the view *holds* sit on the right where a
    /// count belongs. Everything else — the container, the history ladder, the display options — is
    /// a control the reader reaches for on purpose, so it is grouped at the far end rather than
    /// interleaved with the two above.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let follow = self.log().following;
        // The stream is still running while the view stopped following it, so the state needs a
        // visible label: an unselected Follow button is not enough.
        let follow_paused = self.log().follow && !self.log().following;
        let can_control = matches!(
            self.log().phase,
            LogPhase::Connecting | LogPhase::Streaming | LogPhase::Reconnecting { .. }
        );
        let visible = self.visible_log_count();
        let retained = self.log().buffer.len();
        let filtered = self.log_view_is_filtered();
        // One number on screen. The retained count is the one `UI-SPEC` §16.2 names, and while a
        // filter is active the count a reader wants is how much of it survived, so the two share a
        // slot instead of printing the total twice on the same row.
        let count = if filtered {
            (
                format!(
                    "{} of {} lines",
                    format_count(visible),
                    format_count(retained)
                ),
                format!("Log filter count: {visible} of {retained}."),
            )
        } else {
            (
                format!("{} lines", format_count(retained)),
                format!("Log buffer holds {retained} lines."),
            )
        };
        let filter_count = filtered.then(|| {
            (
                format!("{} / {}", format_count(visible), format_count(retained)),
                format!("Log filter count: {visible} of {retained}."),
            )
        });
        let dropped_note = (self.log().dropped_lines > 0).then(|| {
            let dropped = self.log().dropped_lines;
            (
                format!("{} dropped", format_count(dropped as usize)),
                format!(
                    "Log dropped count: {dropped} older lines. The buffer keeps the newest {} lines.",
                    format_count(RING_CAPACITY)
                ),
            )
        });
        let view_actions = h_flex()
            .id("dock-log-view-actions")
            .flex_none()
            .gap(space::XS)
            .items_center()
            // `Follow` is the row's one streaming control, and it is drawn as the switch
            // `UI-SPEC` §16.3 calls for: `accent.wash` while it is on. It used to be `.outline()`,
            // which is a 1px border — a box that reads in dark and disappears in light, on the one
            // control whose state decides whether the lines under it are current. The wash is the
            // same fill the selected Dock tab uses, so "on" looks the same in both places.
            //
            // The pause button that used to sit beside it is gone. Its handler was
            // `paused = true; set_follow(false)` — the Follow button's own off path, under a second
            // name, in the second most prominent slot on the row, and its tooltip had to explain
            // that "New lines continue to buffer", which is also what Follow-off does. Two controls
            // with one behaviour is a question the reader has to learn; `UI-SPEC` §16.2 names four
            // controls here and this was not one of them.
            .child(self.render_follow_switch(follow, can_control, cx))
            .child(self.render_log_level_menu(cx))
            .when_some(self.render_container_menu(cx), |this, menu| {
                this.child(menu)
            });
        h_flex()
            .id("dock-log-toolbar")
            .debug_selector(|| "dock-log-toolbar".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::DOCK_TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .tab_group()
            .on_key_down(cx.listener(Self::on_toolbar_key_down))
            .text_size(design::text::BODY)
            .line_height(design::text::BODY_LINE_HEIGHT)
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            // The band is below the body, so the boundary it rules is its top edge. The rule belongs
            // to whichever side draws it and both sides draw the same 1px `border.subtle`, so
            // crossing between the tabs cannot change how thick the line above the band is.
            .border_t_1()
            .border_color(design::role::border_subtle(cx))
            .restrict_scroll_to_axis()
            // The paused state leads the row, and it is the one item on it that is a statement
            // rather than a control. `UI-SPEC` §16.3 wants it clickable: a log pane that keeps
            // yanking the viewport while a reader is three lines up is the most irritating thing
            // it can do, and the way back has to be one keystroke or one click away.
            .when(follow_paused, |this| {
                this.child(self.render_follow_paused_note(cx))
            })
            .child(view_actions)
            // The find bar takes the field's slot rather than a row of its own. `UI-SPEC` §16.2
            // fixes the toolbar at one band, and a second band for find would be 28px the Dock
            // does not have at its own minimum. The two fields are never on screen together:
            // find is a question about the lines, filter is a question about which lines exist,
            // and a reader asks one at a time.
            .when(self.log().find_open, |this| {
                this.child(self.render_find_bar(cx))
            })
            .when(!self.log().find_open, |this| {
                // The field is the one control on this row that has to survive a crowded one: it
                // is where a reader goes on purpose, every session, and a flex child with no floor
                // is the first thing a neighbour takes the space from. `flex_1` with a minimum is
                // that floor, and it makes the field the row's slack absorber rather than its
                // casualty.
                this.child(
                    div()
                        .id("dock-log-filter-slot")
                        .flex_1()
                        .min_w(space::XL)
                        .max_w(design::size::CONTROL * 8.)
                        .child(self.log_filter_input.clone()),
                )
            })
            .when(!self.log().find_open, |this| {
                this.when_some(filter_count, |this, (count, aria_label)| {
                    this.child(
                        h_flex()
                            .id("dock-log-filter-status")
                            .flex_none()
                            .role(Role::Status)
                            .aria_label(aria_label)
                            .child(label_small(count).text_color(design::role::fg_tertiary(cx))),
                    )
                })
            })
            .child(div().flex_1().min_w(space::SM))
            .when_some(dropped_note, |this, (count, aria_label)| {
                this.child(
                    h_flex()
                        .id("dock-log-dropped-status")
                        .debug_selector(|| "dock-log-dropped-status".to_owned())
                        .flex_none()
                        .role(Role::Status)
                        .aria_label(aria_label)
                        .child(label_small(count).text_color(design::role::fg_tertiary(cx))),
                )
            })
            .child(
                h_flex()
                    .id("dock-log-line-count")
                    .debug_selector(|| "dock-log-line-count".to_owned())
                    .flex_none()
                    .role(Role::Status)
                    .aria_label(count.1)
                    // A count is a number, and a proportional face gives `1` a narrower advance
                    // than `8`, so a line count that grows from four digits to five staircases
                    // across the row it sits on.
                    .font_features(tabular_features(cx))
                    .child(label_small(count.0).text_color(design::role::fg_tertiary(cx))),
            )
            .into_any_element()
    }

    /// The `⌘F` bar: the field, where the reader is among the matches, and the way out.
    ///
    /// The counter is the whole reason a find bar exists rather than a filter — it tells a reader
    /// with nine matches in ten thousand lines that there are nine and that they are on the
    /// first. `0 of 0` is the one case that needs saying in words, because "0 of 0" is a number
    /// and the reader needs to know the query is the reason.
    fn render_find_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let total = self.log().find_hits.len();
        let active = if total == 0 {
            0
        } else {
            self.log().find_active + 1
        };
        let counter = if total == 0 {
            "No matches".to_owned()
        } else {
            format!("{} of {}", format_count(active), format_count(total))
        };
        let announced = if total == 0 {
            format!("Find: no log line contains {}.", self.log().find_query)
        } else {
            format!("Find: match {active} of {total}.")
        };
        h_flex()
            .id("dock-log-find")
            .debug_selector(|| "dock-log-find".to_owned())
            .flex_none()
            .gap(space::XS)
            .items_center()
            .child(self.find_input.clone())
            .child(
                h_flex()
                    .id("dock-log-find-count")
                    .debug_selector(|| "dock-log-find-count".to_owned())
                    .flex_none()
                    .h(BAND_CONTROL)
                    .px(space::XS)
                    .items_center()
                    .rounded(design::radius::SM)
                    .role(Role::Status)
                    .aria_label(announced)
                    .font_features(tabular_features(cx))
                    .child(label_small(counter).text_color(if total == 0 {
                        design::role::fg_tertiary(cx)
                    } else {
                        design::role::fg_secondary(cx)
                    })),
            )
            .child(
                Button::new("dock-find-close")
                    .icon(Icon::new(IconName::X))
                    .ghost()
                    .with_size(Size::Size(BAND_CONTROL))
                    .tab_index(12isize)
                    .tooltip("Close find")
                    .accessibility_label("Close find")
                    .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
            )
            .into_any_element()
    }

    /// `Follow`, drawn here rather than handed to `Button`.
    ///
    /// It is the row's only switch and the Dock's other selected thing, and it was the odd one out:
    /// a component button's own horizontal padding put more space on one side of the word than the
    /// other, so the label was not centred in the wash that says it is on. The tab pill is drawn in
    /// this file for the same reason, and the two now read as the same control.
    ///
    /// On is `accent.wash` — `UI-SPEC` §3.2's answer for a selected thing, and the only one that
    /// works in both appearances: the 1px outline this used to wear reads in dark and vanishes in
    /// light, on the one control whose state decides whether the lines under it are current. It is
    /// the same fill the selected Dock tab takes, so "on" looks the same in both places.
    fn render_follow_switch(
        &self,
        follow: bool,
        can_control: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.entity().downgrade();
        let surface = design::role::surface_chrome(cx);
        let ink = if !can_control {
            design::state::disabled(cx, surface)
        } else if follow {
            design::role::fg_primary(cx)
        } else {
            design::role::fg_secondary(cx)
        };
        let mut switch = h_flex()
            .id("dock-follow")
            .debug_selector(|| "dock-follow".to_owned())
            .flex_none()
            .h(BAND_CONTROL)
            .px(space::SM)
            .items_center()
            .justify_center()
            .rounded(design::radius::SM)
            // §9.3: no cursor, and no `Arrow` either. The pair was a way of saying "this is
            // clickable" with the pointer, which is the web habit the rule names; a disabled switch
            // is already told apart by its ink, its tooltip and its missing press response.
            .text_color(ink)
            // The focus ring is the only thing this border is for. It used to double as the
            // selected marker — a 2px accent bar standing in the left padding — which put two
            // signals on one state and is the marker the guide rules out by name: "Do not add a
            // leading-edge bar or one-sided border as the selection marker." The wash below already
            // says "on", and it says it across the whole control rather than down one edge.
            //
            // It is reserved on all four sides at rest, so focusing the switch never resizes it and
            // never moves the word — and four 1px sides are the same 2px of border the one 2px side
            // was reserving, so the control's box does not change.
            .border_1()
            .border_color(design::role::border_subtle(cx).alpha(0.))
            .focus_visible(|this| this.border_color(design::focus::border(cx)))
            // The ink goes on the label, not on this row. gpui-kit's `Label` renders
            // `.text_color(cx.theme().foreground)` before it applies the caller's style, so a
            // `text_color` on an ancestor never reaches the text: this switch computed `ink` for
            // three states and drew `fg.primary` in all of them, which means Follow looked the
            // same on and off — the wash and the rail were the whole tell — and a disabled switch
            // was drawn at full opacity with no `state::disabled` on it at all. `UI-SPEC` §3 lists
            // `rest` and `disabled` as two of the six states every control has, and a control
            // whose disabled state is invisible is a reader pressing something that will not
            // answer.
            .child(label_text("Follow").text_color(ink));
        if can_control {
            if follow {
                switch = switch
                    .bg(design::role::accent_wash(cx))
                    // A selected control still acknowledges the pointer. Without this, the one
                    // row control whose state is a fill loses that fill the moment the pointer
                    // arrives and reads as if it turned itself off. Hover is `state::hover_on`,
                    // press is `state::press_on`, the same two steps the tab pills state.
                    .hover(|this| {
                        this.bg(design::state::hover_on(
                            design::role::accent_wash(cx),
                            design::role::fg_primary(cx),
                        ))
                    });
            } else {
                switch = switch.hover(|this| this.bg(design::state::hover(cx, surface)));
            }
            switch.interactivity().tooltip(command_tooltip(
                "Keep the newest lines in view. Follow pauses when you scroll up and resumes at \
                 the bottom.",
            ));
            switch = switch.on_mouse_down(MouseButton::Left, move |_, _, cx| {
                panel
                    .update(cx, |panel, cx| {
                        let next = !panel.log().following;
                        panel.set_follow(next, cx);
                    })
                    .ok();
                cx.stop_propagation();
            });
        }
        switch.into_any_element()
    }

    /// The row that says the view has stopped following the stream, and the way back.
    ///
    /// It is a control rather than a caption because the state it reports has exactly one action
    /// that resolves it, and a reader who has scrolled away should not have to work out which of
    /// the controls on this row is the one that undoes it. `warning` is the only status colour in
    /// a healthy log pane and it is spent here, because this is the only state in which the pane
    /// is not showing the reader what it was asked to show.
    fn render_follow_paused_note(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut note = div()
            .id("dock-log-follow-paused")
            .debug_selector(|| "dock-log-follow-paused".to_owned())
            .flex_none()
            .h(BAND_CONTROL)
            .px(space::XS)
            .gap(space::XS)
            .items_center()
            .rounded(design::radius::SM)
            .bg(design::role::warning_wash(cx))
            .role(Role::Button)
            .aria_label(FOLLOW_PAUSED_DESCRIPTION)
            // §9.3: no cursor.
            .hover(|this| this.bg(design::role::warning(cx).alpha(0.18)))
            .active(|this| this.bg(design::role::warning(cx).alpha(0.24)))
            .on_click(cx.listener(|this, _, _, cx| this.jump_to_latest(cx)))
            .child(
                div()
                    .flex_none()
                    .w(design::size::STATUS_DOT)
                    .h(design::size::STATUS_DOT)
                    .rounded_full()
                    .bg(design::role::warning(cx)),
            )
            // The note is the one item on this row that is a temporary state, so it is the one
            // item that gives way. Unbounded, its five words took 500px of a 28px band and the
            // filter field — the control a reader reaches for on purpose, every session — was
            // squeezed to nothing behind it. `UI-SPEC` §16.3 still wants both halves of the state
            // readable, so the label truncates rather than disappearing, and the full sentence
            // stays in the accessible name and the tooltip.
            .child(
                label_small(FOLLOW_PAUSED_LABEL)
                    .min_w(px(0.))
                    .max_w(FOLLOW_PAUSED_LABEL_MAX_WIDTH)
                    .text_color(design::role::fg_primary(cx))
                    .truncate(),
            );
        note.interactivity()
            .tooltip(command_tooltip(FOLLOW_PAUSED_DESCRIPTION));
        note.into_any_element()
    }

    /// Which container the stream is tailed from, when the Pod has more than one.
    ///
    /// A container that is the Pod's only one is not a choice, so it draws nothing: a reader
    /// looking at one container's logs is not being asked anything and should not be shown a menu
    /// with one item in it.
    ///
    /// It sits beside the level scope rather than in the strip's `⋯` menu because it is the same
    /// kind of question — which slice of what the Pod is saying reaches the pane — and it used to
    /// sit under "Show timestamps" and "Wrap long lines", where a reader with the wrong answer had
    /// no reason to think the menu held it. Two places to look for one fact is how a silent wrong
    /// container becomes a half-minute of reading someone else's log.
    fn render_container_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let panel = cx.entity().downgrade();
        let selected = self.log().container.clone();
        let containers = self
            .log()
            .request
            .as_ref()
            .map(|request| request.containers.clone())
            .unwrap_or_default();
        if containers.len() < 2 {
            return None;
        }
        Some(
            labelled(
                Button::new("dock-log-container")
                    .debug_selector(|| "dock-log-container".to_owned())
                    .ghost()
                    // The height is stated rather than derived, for the reason the level trigger
                    // states it: a labelled `Button` takes its box from the text inside it, and two
                    // neighbouring controls at different heights is the row's only misalignment.
                    .with_size(Size::Size(design::size::CONTROL))
                    .h(BAND_CONTROL)
                    .tab_index(10isize)
                    .dropdown_caret(true)
                    .tooltip("Which container this stream reads. Changing it restarts the stream."),
                format!("Container: {}", self.log().container_label()),
            )
            // The two words differ on purpose: the visible value is the container's short name,
            // and the announced one repeats it as a field of the log stream, so a reader hearing
            // only the trigger knows what the caret is about to change.
            .accessibility_label(format!("Log container: {}", self.log().container_label()))
            // A labelled Button is what `labelled` builds, so the menu wraps after it.
            .dropdown_menu(move |menu, _, _| {
                containers.clone().into_iter().fold(menu, |menu, name| {
                    let toggled = selected.as_ref() == Some(&name);
                    let panel = panel.clone();
                    let name = name.clone();
                    menu.item(
                        menu_item(name.clone())
                            .checked(toggled)
                            .on_click(move |_, _, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_container(name.clone(), cx))
                                    .ok();
                            }),
                    )
                })
            })
            .into_any_element(),
        )
    }

    /// Severity scope for the log list. Severity is parsed from the first token of a line, so
    /// this is the only way to ask for warnings or errors without typing their labels.
    fn render_log_level_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let level = self.log().log_level;
        let has_lines = !self.log().buffer.is_empty();
        // gpui-kit owns the popup surface and the trigger's dismissal, so the
        // panel only describes the rows.
        labelled(
            Button::new("dock-log-level")
                .debug_selector(|| "dock-log-level".to_owned())
                .ghost()
                // The height is stated, not derived: a labelled `Button` takes its box
                // from the text.
                .with_size(Size::Size(design::size::CONTROL))
                .h(BAND_CONTROL)
                .tab_index(11isize)
                .dropdown_caret(true)
                .tooltip(level.description())
                .disabled(!has_lines),
            format!("Level: {}", level.short_label()),
        )
        // The two words differ on purpose: the visible value is the scope's short name and the
        // announced one spells the field out, so a reader hearing only the trigger knows what the
        // caret is about to change.
        .accessibility_label(format!("Log level: {}", level.label()))
        // A labelled Button is what `labelled` builds, so the menu wraps after it.
        .dropdown_menu(move |menu, _, _| {
            let panel = panel.clone();
            if !has_lines {
                return menu;
            }
            LogLevelScope::ALL.into_iter().fold(menu, |menu, option| {
                let panel = panel.clone();
                menu.item(menu_item(option.label()).checked(option == level).on_click(
                    move |_, _, cx| {
                        panel
                            .update(cx, |panel, cx| panel.set_log_level(option, cx))
                            .ok();
                    },
                ))
            })
        })
        .into_any_element()
    }

    /// The strip's overflow menu: everything about the active panel that is not one of the four
    /// controls `UI-SPEC` §16.2 names for its toolbar.
    ///
    /// §16.2 draws this `⋯` on the strip, beside `⌃` and `×`, and draws the toolbar as
    /// `Follow · level · Filter… · lines`. Six controls in a 28px band was the other reading, and
    /// it does not fit: at the 960px `WINDOW_MIN` the history ladder, `Load More History` and the
    /// options menu were pushed off the end of a horizontally scrolling row with nothing on screen
    /// to say they were there. The strip is where a panel's settings belong when the panel's
    /// *view* is four controls wide, and it is the only row in the Dock with room.
    ///
    /// It keeps the cluster's frame: the same 24px box as `⌃` and `×`, the same leading gap, and —
    /// because it owns a menu — a pressed fill that stays while the menu is up. gpui-kit moves
    /// focus into the menu on open, so the trigger's own hover and press states have nothing left
    /// to say; the open flag is what says it.
    fn render_overflow_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.active_tab == DockTab::Terminal {
            return self.render_terminal_menu("dock-terminal-actions-strip", cx);
        }
        let panel = cx.entity().downgrade();
        let timestamps = self.log().timestamps;
        let wrap = self.log().wrap;
        let has_source = self.log().request.is_some() && self.factory.is_some();
        let has_lines = !self.log().buffer.is_empty();
        let history_lines = self.log().history_lines;
        let cap = history_cap();
        // The press wash, read from the same pair of roles the two icon buttons beside it use, so
        // an open menu and a held pointer are the same colour.
        let chrome = design::role::surface_chrome(cx);
        let open_wash = design::state::press_on(chrome, design::role::fg_primary(cx));
        let menu_open = self.overflow_menu_open.get();
        let trigger_panel = panel.clone();
        Button::new("dock-overflow")
            .debug_selector(|| "dock-overflow".to_owned())
            // The glyph's ink is stated, because an `Icon` resolves a missing colour to the
            // theme's foreground rather than to the button's: `Button` paints `.text_color()` on
            // its own root, and the `Icon` never reads it. Measured on the strip, the three
            // trailing controls came out at the theme foreground — `#FAFAFA`-ish, ~1.65x the
            // contrast of every other chrome control in the window, including the Dock's own
            // `×` on the tab pill 60px away, which states `icon::resting`. Three marks a full
            // step above the selected tab's label is what made the cluster read as one bright
            // blob rather than as three quiet controls.
            //
            // Open is a state, so open is the ink step, exactly as `shell/panels.rs` states it
            // on the bell: the trigger has to say it owns the menu that is up, and hover cannot
            // say it once focus has moved into the menu.
            .icon(Icon::new(IconName::Ellipsis).text_color(if menu_open {
                design::icon::active(cx)
            } else {
                design::icon::resting(cx)
            }))
            .ghost()
            // 24x24, the same icon-button box as `⌃` and `×` beside it. It used to override the
            // height down to the 22px tab pill, so the one control in the strip that is not a
            // navigation control was 2px shorter than the two that are, and the row of three had
            // three different heights in it. The strip is 28px, so a 24px control still has 2px of
            // strip above and below it.
            .with_size(Size::Size(design::size::ICON_BUTTON))
            .tab_index(3isize)
            .tooltip("Log history and display options.")
            .accessibility_label("Log options")
            .when(menu_open, |this| this.bg(open_wash))
            .dropdown_menu(move |menu, _, _| {
                let tail_panel = panel.clone();
                let mut menu = menu
                    .item(menu_item("History fetched").disabled(true))
                    .separator();
                menu = TailLines::ALL.into_iter().fold(menu, |menu, option| {
                    let panel = tail_panel.clone();
                    menu.item(
                        menu_item(format!("Last {} lines", option.label()))
                            .checked(option.value() == history_lines)
                            .on_click(move |_, _, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_tail(option, cx))
                                    .ok();
                            }),
                    )
                });
                if has_source && history_lines < cap {
                    let panel = panel.clone();
                    menu = menu.item(menu_item("Load more history").on_click(move |_, _, cx| {
                        panel.update(cx, |panel, cx| panel.load_earlier(cx)).ok();
                    }));
                }
                let stamp_panel = panel.clone();
                menu = menu.separator().item(
                    menu_item("Show timestamps")
                        .checked(timestamps)
                        .on_click(move |_, _, cx| {
                            stamp_panel
                                .update(cx, |panel, cx| {
                                    let next = !panel.log().timestamps;
                                    panel.set_timestamps(next, cx);
                                })
                                .ok();
                        }),
                );
                let wrap_panel = panel.clone();
                menu = menu.item(menu_item("Wrap long lines").checked(wrap).on_click(
                    move |_, _, cx| {
                        wrap_panel
                            .update(cx, |panel, cx| panel.toggle_wrap(cx))
                            .ok();
                    },
                ));
                if !has_lines {
                    return menu;
                }
                let clear_panel = panel.clone();
                let download_panel = panel.clone();
                menu.separator()
                    .item(
                        menu_item("Clear log buffer")
                            .icon(IconName::Eraser)
                            .on_click(move |_, _, cx| {
                                clear_panel.update(cx, |panel, cx| panel.clear(cx)).ok();
                            }),
                    )
                    .item(
                        menu_item("Download logs")
                            .icon(IconName::Download)
                            .on_click(move |_, _, cx| {
                                download_panel
                                    .update(cx, |panel, cx| panel.download(cx))
                                    .ok();
                            }),
                    )
            })
            // The trigger stays pressed while its menu is up. gpui-kit takes focus into the menu
            // on open, so the trigger's own hover and press states cannot carry that relationship,
            // and a menu that appears under an unmarked trigger reads as an unowned popup. The
            // update is on the panel rather than the flag so the strip repaints in the same frame.
            .on_open_change(move |open, _window, cx| {
                if let Some(panel) = trigger_panel.upgrade() {
                    panel.update(cx, |panel, cx| {
                        panel.overflow_menu_open.set(*open);
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }

    /// The control a stopped stream offers. Both surfaces that can report a failure use this one
    /// element, so the tab order holds a single one and its meaning is the same in either place.
    ///
    /// The word is `Reconnect` and not `Retry` because that is what it does: the request is
    /// rebuilt against the same target with the same options, and a reader who is told `Retry`
    /// cannot tell whether the same thing will happen differently or whether they should be
    /// changing something first.
    ///
    /// `secondary`, not `primary`. It was the Dock's last `Accent` control and the loudest thing
    /// in the pane, and it appeared in exactly the two places a recovery action should not be the
    /// main event: the empty state and the error row. Both of those are states the reader arrived
    /// at rather than chose, so a filled accent button there tells them the pane is about a
    /// decision when it is about a stream that stopped — and it spent the surface's whole accent
    /// budget on a button that is one click of several ways back. `secondary` keeps the push-button
    /// read (§4.6's 6% fill) and lets the *reason* stay the loudest thing in the row, which is the
    /// part the reader actually needs.
    fn render_log_retry(&self, cx: &Context<Self>) -> AnyElement {
        labelled(
            Button::new("dock-retry")
                .secondary()
                .with_size(Size::Size(design::size::CONTROL))
                // Stated for the same reason as the level trigger, and to the same
                // height: the two are the row's only push buttons and they are 28px
                // and 20px apart otherwise.
                .h(BAND_CONTROL)
                .tab_index(9isize)
                .tooltip("Reconnect the log stream")
                .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
            "Reconnect",
        )
        .into_any_element()
    }

    /// Shows a recoverable log stream status, unless the log body is already showing it. The
    /// banner and the empty state are one entry point with two shapes, so a failure is never
    /// stated twice in the same view.
    ///
    /// The band holds [`log_banner_height`] rather than `design::size::ROW`. Thirty-two was two
    /// pixels short of the status plate it holds, so on every failed stream with lines in the
    /// buffer — which is the state this row exists for, because an empty body reports the failure
    /// in its own empty state instead — the plate's bottom hairline and two pixels of its wash
    /// painted over the top of the first log row. The band and the thing inside it have to be one
    /// height, and the plate is the thing that is drawn.
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
                .h(log_banner_height())
                .px(space::SM)
                .gap(space::XS)
                .items_center()
                .bg(design::role::surface_chrome(cx))
                .child(status_message(
                    notice.severity,
                    notice.guidance,
                    notice.detail,
                    cx,
                ))
                // A stream that stopped has to say when it last spoke. A pane of frozen lines is
                // indistinguishable from a quiet one, and "it has been four minutes" is the only
                // thing in the row that says the difference.
                .when_some(self.last_line_note(), |this, note| {
                    this.child(
                        div()
                            .id("dock-log-last-line")
                            .debug_selector(|| "dock-log-last-line".to_owned())
                            .flex_none()
                            .child(label_small(note).text_color(design::role::fg_tertiary(cx))),
                    )
                })
                .child(div().flex_1())
                .when(retry, |this| this.child(self.render_log_retry(cx)))
                .into_any_element(),
        )
    }

    fn render_logs(&self, cx: &mut Context<Self>) -> AnyElement {
        let has_source = self.log().request.is_some() && self.factory.is_some();
        let body: AnyElement = if self.log().buffer.is_empty() {
            match self.log_failure_notice() {
                // With nothing to read, the body is the surface that reports the failure, so the
                // banner above it stays hidden instead of saying the same thing twice.
                Some(notice) => {
                    let retry = notice.retry.then(|| self.render_log_retry(cx));
                    empty_state_block(dock_empty_state(
                        notice.icon,
                        notice.title,
                        notice.guidance,
                        retry,
                        cx,
                    ))
                }
                None => match &self.log().phase {
                    LogPhase::Idle => self.logs_unavailable_state(cx),
                    // The waiting state names the thing it is waiting for. A pane that says
                    // "Loading logs" over an arbitrary number of seconds cannot tell the reader
                    // whether the cluster is slow or the request went to the wrong Pod, and the
                    // one fact the reader has — which object this stream is pointed at — is
                    // printed nowhere on the surface at that moment. It goes in the sentence
                    // rather than a third heading, because the sentence is the row that is allowed
                    // to change shape per state.
                    _ => {
                        let target = log_target_label(
                            self.log().request.as_ref(),
                            self.log().container.as_deref(),
                        );
                        empty_state_block(dock_empty_state(
                            IconName::LoaderCircle,
                            "Waiting for logs",
                            format!("Opening the stream for {target}"),
                            None,
                            cx,
                        ))
                    }
                },
            }
        } else if self.visible_log_count() == 0 {
            // "Nothing here" and "nothing here *because of what you asked for*" are different
            // answers and the reader cannot tell them apart from an empty pane. `UI-SPEC` §8 asks
            // the second one to name how many filters are in force, because a reader who set two
            // and is looking at one blank pane has no way to know which one emptied it.
            let filters = self.active_filter_names();
            let hint = match filters.len() {
                0 => format!(
                    "All {} buffered lines are hidden. Select a level to widen the scope.",
                    format_count(self.log().buffer.len())
                ),
                1 => format!(
                    "{} is hiding every line. Clear it to see the buffer.",
                    filters[0]
                ),
                _ => format!(
                    "{} are hiding every line. Clear them to see the buffer.",
                    filters.join(" and ")
                ),
            };
            empty_state_block(dock_empty_state(
                IconName::Funnel,
                "No matching log lines",
                hint,
                None,
                cx,
            ))
        } else if self.log_body_is_too_short() {
            // The Dock was squeezed below a readable body. Half a row of text is worse than a
            // sentence that says what to do.
            empty_state_block(dock_empty_state(
                IconName::TriangleAlert,
                "Dock too short for log lines",
                "Drag the Dock divider up to read the log.",
                None,
                cx,
            ))
        } else if self.log().wrap {
            self.render_wrapped_list(cx)
        } else {
            self.render_nowrap_list(cx)
        };
        v_flex()
            .size_full()
            .min_h(px(0.))
            // The banner stays on top. `UI-SPEC` §4.15 puts an in-place error at the top of the
            // thing it is about, and a failure notice is an error.
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
            // The Terminal tab always draws its band, so dropping the whole row when there is no
            // log source moved the content's edge by a full toolbar every time the reader crossed
            // between the two tabs. The band is isobaric: same height, same surface, same hairline,
            // one status line instead of the controls. `UI-SPEC` §16.2 puts it under the body.
            .child(if has_source {
                self.render_toolbar(cx)
            } else {
                self.render_log_toolbar_shell(cx)
            })
            .into_any_element()
    }

    /// The toolbar band the Logs tab shows when there is no log source to control.
    ///
    /// Same frame as [`Self::render_toolbar`]: `design::size::DOCK_TOOLBAR`, the chrome surface,
    /// and the same hairline on the same edge, so crossing to the Terminal tab and back does not
    /// move the content.
    ///
    /// It also carries the `Open Logs` control, which is where every other log command already
    /// lives - Follow, Pause, Load More History - and the only place it fits. At
    /// `design::size::DOCK_MIN` the body over this band is 86px, and a centred empty state with a
    /// 24px control in it needs 110. Inside the body the control and the hint are the two
    /// flexible rows, so both would shrink: a 14px button above a zero-height sentence.
    fn render_log_toolbar_shell(&self, cx: &Context<Self>) -> AnyElement {
        // The band's left slot reports a *transport* fact and nothing else, and only when the Dock
        // has one the reader does not already have. It used to name the scope as
        // `kind-k8s-gpui-3n/All Namespaces`, which is a third rendering of one fact: the title bar
        // prints the cluster and the namespace as two `▾` targets 60px apart, and the status bar
        // prints the same pair as one path 28px below this band. Three copies of the same string,
        // the nearest two 28px apart, is the reader having to check which one is live — and it is
        // also the reason the band felt like it had content when it had none. §4: one fact, one
        // place. The scope is the shell's to render, and the body's own state stays the body's.
        //
        // So the slot says something only when nothing is connected, and a source with no context
        // label is a label the Dock was never given — saying so there would be the Dock guessing.
        let status = if self.factory.is_none() {
            Some(SharedString::from("No cluster connected"))
        } else if self.terminal_context_label().is_none() {
            Some(SharedString::from("No log source connected"))
        } else {
            None
        };
        h_flex()
            .id("dock-log-toolbar-shell")
            .debug_selector(|| "dock-log-toolbar-shell".to_owned())
            .flex_none()
            .w_full()
            .h(design::size::DOCK_TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            .border_t_1()
            .border_color(design::role::border_subtle(cx))
            .when_some(status, |this, status| {
                this.child(
                    div().flex_1().min_w(px(0.)).overflow_hidden().child(
                        label_small(status)
                            .text_color(design::role::fg_tertiary(cx))
                            .truncate(),
                    ),
                )
            })
            .child(div().flex_1().min_w(px(0.)))
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
            // The wrapper is the stop: it holds the handle the Dock focuses when a log surface
            // has nothing to stream, and it draws the ring and answers the key that presses the
            // control, because the button below keeps its handle to itself.
            .track_focus(&self.open_logs_focus)
            .border_l(design::border::FOCUS_RAIL)
            .border_color(design::role::border_subtle(cx).alpha(0.))
            .focus_visible(|this| this.border_color(design::focus::border(cx)))
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, window, cx| {
                if presses(&event.keystroke) {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(crate::shell::OpenLogs), cx);
                }
            }))
            .child(labelled(
                Button::new("dock-open-logs")
                    // `secondary`, not `primary`. It was the only `primary` on the first screen
                    // and it spent 94% of the screen's accent — 7,013 of 7,448 measured
                    // `#4F8CFF` pixels — on a control that starts a repeatable action rather
                    // than committing to a one-off. `UI-SPEC` §2 rule 8 caps accent at two per
                    // screen and the two the first screen already spends are the ones §4.2 and
                    // §3 name; a third is a push button declaring itself to be the most important
                    // thing in a window whose actual job is a table of Pods.
                    //
                    // `secondary` over `ghost` for two reasons. It is the band's only labelled
                    // control, and `ghost` is `fg.secondary` on nothing, which on
                    // `surface.chrome` is a row of grey text the reader has to look for; and §4.6
                    // gives `secondary` a 6% fill, so it still reads as a push button. Both are
                    // push buttons, so both keep §2 rule 9: no hover, pressed only.
                    //
                    // It was never competing with another `primary`: `render_log_toolbar_shell`
                    // draws only when there is no log source, which is exactly when
                    // [`Self::render_log_retry`] is not on screen, so the two never shared a row.
                    .secondary()
                    // `BAND_CONTROL`, stated outright. It used to ask for `DOCK_TOOLBAR`, which
                    // the component turned into 5.6px of horizontal padding and a height of 20px:
                    // the glyphs of `Open Logs` sat a pixel off the fill, in the only 28px band in
                    // the Dock that carries a push button.
                    .with_size(Size::Size(BAND_CONTROL))
                    .h(BAND_CONTROL)
                    .tab_index(OPEN_LOGS_TAB_INDEX)
                    .tab_stop(false)
                    .tooltip(
                        "Stream the logs of the selected Pod. With nothing selected, this \
                         reports which step is missing.",
                    )
                    .on_click(cx.listener(|_, _, window, cx| {
                        window.dispatch_action(Box::new(crate::shell::OpenLogs), cx);
                    })),
                "Open Logs",
            ))
            .into_any_element()
    }

    /// The filters currently narrowing the log list, in the words the reader set them with.
    ///
    /// A pane can be empty for two reasons — nothing was ever streamed, or everything that was
    /// streamed was filtered out — and the second one is a consequence of something the reader
    /// just did. Naming the filters is what turns the empty state from a fact into an
    /// instruction.
    fn active_filter_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if !self.log().log_filter.is_empty() {
            names.push("The filter");
        }
        if self.log().log_level != LogLevelScope::All {
            names.push(self.log().log_level.label());
        }
        names
    }

    /// The empty state the Logs tab shows when nothing has been asked for yet.
    ///
    /// The heading is the whole of it. It used to be an icon, a title, a sentence naming a menu
    /// that is not on screen, and no control at all, on a surface whose sibling states
    /// ([`Self::terminal_empty_state`], the log failure state) both carry a real button; the
    /// sentence then became a second instruction beside the `Open Logs` button in the band below,
    /// which is the arrangement §4.13's "really none" row rules out. The band carries the control
    /// and the control carries the sentence.
    ///
    /// The title follows the state rather than being fixed. It was `No log target` in both cases,
    /// and one of the two cases *has* a log target: a Pod was selected, the request went out, and
    /// the cluster it went to is not connected. A heading that denies the thing the reader just did
    /// is worse than no heading, because it sends them back through the step that already worked.
    fn logs_unavailable_state(&self, cx: &App) -> AnyElement {
        let (title, hint) = if self.log().request.is_some() {
            (LOGS_NO_SOURCE_TITLE, LOGS_NO_SOURCE_HINT)
        } else {
            (LOGS_NO_TARGET_TITLE, "")
        };
        dock_empty_state(IconName::BookOpen, title, hint, None, cx)
    }

    /// True when the measured log body cannot hold three rows, which is what the minimum Dock
    /// height leaves for it. Before the first layout the height is zero, and the list renders
    /// normally.
    fn log_body_is_too_short(&self) -> bool {
        let height = self.log_body_handle.bounds().size.height;
        height > px(0.) && height + dock_chrome_height() < design::size::DOCK_MIN
    }

    fn render_wrapped_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let buffer = self.log().buffer.clone();
        let state = self.log().list_state.clone();
        let font = buffer_font(cx);
        let visible = self.log().visible_log_indices.clone();
        let filtered = self.log_view_is_filtered();
        let timestamp_reserve = buffer.timestamp_column_reserve();
        let row_context = self.log_row_context(cx);
        div()
            .id("dock-log-scroll")
            .debug_selector(|| "dock-log-scroll".to_owned())
            .role(Role::List)
            .aria_label("Log lines")
            .size_full()
            // The scrollbar layer is absolutely positioned against this box, so the box has to be
            // its containing block.
            .relative()
            .vertical_scrollbar(&self.log().list_state)
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

    fn render_nowrap_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let buffer = self.log().buffer.clone();
        let font = buffer_font(cx);
        let measure_row = self.log().longest_visible_log_row;
        let visible = self.log().visible_log_indices.clone();
        let filtered = self.log_view_is_filtered();
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
        .track_scroll(&self.log().scroll_handle)
        .size_full();

        div()
            .id("dock-log-nowrap-scroll")
            .debug_selector(|| "dock-log-nowrap-scroll".to_owned())
            .role(Role::List)
            .aria_label("Log lines")
            .size_full()
            // The scrollbar layer is absolutely positioned against this box, so the box has to be
            // its containing block.
            .relative()
            .scrollbar(&self.log().scroll_handle, ScrollbarAxis::Both)
            .restrict_scroll_to_axis()
            .child(list)
            .into_any_element()
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.active_tab.is_logs() {
            return self.render_terminal(window, cx);
        }
        self.render_logs(cx)
    }

    /// The session controls and the active terminal.
    ///
    /// The port forward list this used to draw between the toolbar and the body is gone. Three
    /// parts of the spec say the same thing about where a list belongs: `UI-SPEC` §16.1 makes
    /// "Dock = streams, centre = lists" the Dock's one rule, §12 classes port forwards with the
    /// terminal because they are sessions, and §14.6 rules out a resident panel outright. It was
    /// also a second copy: `shell::status_bar` already lists every forward from
    /// [`Self::forward_snapshots`] with `Start` / `Stop` / `Retry` and a `New Forward` footer, so
    /// the Dock's strip was two places to look for the same rows and two places to stop a forward
    /// from. The Dock keeps the model — [`Self::forward_snapshots`] and [`Self::forward_summary`]
    /// are what the status bar reads — and drops the view.
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
            .child(div().flex_1().min_h(px(0.)).overflow_hidden().child(body))
            .child(self.render_terminal_toolbar(window, cx))
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
            // The pane — *and the 16px inset inside it* — is on the terminal's own plane, not on
            // the Dock's content plane. The session view paints its own canvas inside the padding,
            // so without this the Dock showed a frame of the content plane around an
            // inset canvas: the one region in the Dock whose whole job is to be a
            // step below its neighbours was the only one whose frame was on the *neighbour's*
            // step, and the boundary rule below sat on content ink instead of on the plane it
            // bounds.
            //
            // `design::role::surface_inset` is the named role for this — the inset step —
            // rather than the session view's own theme key, so the pane's padding,
            // the rule that closes it and the canvas inside it cannot come from three places. In
            // both shipped themes `surface.inset` and `terminal.background` hold the same value
            // (`#050607` dark, `#F1F2F4` light), so this is the terminal's existing colour and not
            // a new one; the step is now *stated* rather than arrived at by two keys agreeing.
            .bg(design::role::surface_inset(cx))
            .border_b_1()
            .border_color(design::role::border_subtle(cx))
            // `UI-SPEC` §16.4: 16px, not 0. A terminal canvas flush against the edge of its own
            // pane reads as a rendering fault rather than as a window, and the cursor sitting on
            // the boundary is what makes it — there is nowhere for the eye to rest before the
            // first character. It is `space::LG` rather than a fresh number so the terminal's
            // inset is the same 16 the rest of the product pads a panel with.
            .p(space::LG)
            .child(self.terminals[index].instance.view.clone())
            .into_any_element()
    }

    /// Divider between the two terminal panes. It matches the shell dividers: a hit area wider
    /// than the line, a splitter role, a focusable rail, and a keyboard resize.
    ///
    /// "Matches" is stated because it used not to, and it is the whole of what the guide asks of a
    /// handle: "discoverable and quiet… never wider or brighter than the product's other handles".
    /// The shell's three splitters draw four states — a 1px structural line at rest, and on hover,
    /// keyboard focus and drag a 2px accent rail plus a wash that steps 12% → 18% → 24%. This one
    /// drew a 1px line, a 12% wash on hover, a 12% wash on focus, 18% on drag, and *never widened*.
    /// Two of those are the same defect the shell's own comment names: "Focus and hover used to be
    /// the same pair, so the row delivered three [states] out of the four the state matrix asks
    /// for", and the state a keyboard user most needs to find was the one indistinguishable from
    /// the pointer's.
    ///
    /// The visible line and the 20px hit area stay separate: at rest the divider is the 1px
    /// structural line, and the rest of the hit area paints nothing. The accent rail is what
    /// carries the interactive boundary, and it is drawn *over* the rest colour rather than
    /// replacing it, so the boundary does not disappear the moment the pointer leaves.
    fn render_terminal_divider(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let dragging = self.terminal_split_drag.is_some();
        let focused = self.terminal_split_focus.is_focused(window);
        let ratio = self.terminal_split_ratio;
        let highlight = design::focus::border(cx);
        let group = SharedString::from("terminal-split-divider-group");
        // Rest is fully transparent, so the 1px structural line is the whole resting state and the
        // 20px hit area paints nothing around it.
        let rest_wash = highlight.opacity(0.0);
        let hover_wash = highlight.opacity(0.12);
        let focus_wash = highlight.opacity(0.18);
        let drag_wash = highlight.opacity(0.24);
        let rest_line = design::role::border_subtle(cx);
        let body = self.dock_bounds();
        div()
            .id("terminal-split-divider")
            .debug_selector(|| "terminal-split-divider".to_owned())
            .group(group.clone())
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
            .focus_visible(move |this| this.bg(if dragging { drag_wash } else { focus_wash }))
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
                drag_wash
            } else if focused {
                focus_wash
            } else {
                rest_wash
            })
            .hover(move |this| this.bg(hover_wash))
            .active(move |this| this.bg(drag_wash))
            .w(design::border::HIT)
            .h_full()
            .child(
                div()
                    .w(if dragging || focused {
                        design::border::FOCUS_RAIL
                    } else {
                        design::border::LINE
                    })
                    .h_full()
                    .bg(if dragging || focused {
                        highlight
                    } else {
                        rest_line
                    })
                    // Hover is a paint-time state, so the rail's width is a group style on the line.
                    .group_hover(group, |s| s.w(design::border::FOCUS_RAIL).bg(highlight)),
            )
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
    fn dock_bounds(&self) -> Bounds<Pixels> {
        self.width_handle.bounds()
    }

    /// Share of the body a pointer position asks for. A pointer outside the body clamps.
    fn split_ratio_at(&self, x: Pixels, body: Bounds<Pixels>) -> f32 {
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

    /// The Terminal tab's "no session" state.
    ///
    /// §4.13's "说明 默认**没有**", and here it is not a preference:
    ///
    /// - With services, the sentence was the button's own label restated, and the one thing it
    ///   said that the label does not — that the session is a *local shell* scoped to the current
    ///   context — is the button's tooltip.
    /// - Without services there is no button, and the reason is already in the band directly
    ///   below: [`Self::render_terminal_toolbar`] prints `Select a context to open a terminal.`
    ///   when there is nothing to start. The body said the same sentence one row under it.
    ///
    /// It also used to open with `kind-k8s-gpui-3n/All Namespaces · `, a fourth rendering of the
    /// fact the title bar prints as two `▾` targets and the status bar prints as one path. §4: one
    /// fact, one place.
    ///
    /// The height is not decoration. `design::size::DOCK_MIN` leaves this tab 86px of body — the
    /// strip and the band, and *not* the failure banner that is in the Logs tab's chrome — so
    /// §4.13's block is 82px with the action and 106px with a 说明 on top of it. At the floor,
    /// which `⌘⌃Home` reaches in one keystroke, the sentence was pushing the bottom of the button
    /// under the band's rule.
    fn terminal_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let action = self.terminal_available().then(|| {
            div()
                .id("terminal-empty-add-target")
                .debug_selector(|| "terminal-add".to_owned())
                // The wrapper is the stop: closing the last session puts the reader here, so the
                // handle the Dock focuses has to be one the Dock can hold. See
                // `render_close_control` for why the button cannot be that handle.
                .track_focus(&self.terminal_add_focus)
                .border_l(design::border::FOCUS_RAIL)
                .border_color(design::role::border_subtle(cx).alpha(0.))
                .focus_visible(|this| this.border_color(design::focus::border(cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if presses(&event.keystroke) {
                        cx.stop_propagation();
                        this.new_local_terminal(window, cx);
                    }
                }))
                .child(labelled(
                    Button::new("terminal-empty-add")
                        // `secondary`, for the same reason `Open Logs` is, and because
                        // `table_view` already draws the line this follows: its empty states spend
                        // the accent on the control that *repairs* the table (`Retry`, and
                        // `Clear filters`, "the state where the reader came to do something and a
                        // single click undoes whatever stopped them") and leave `Refresh` — the
                        // "nothing here yet, here is the way to get something" state, which is
                        // what this is — on the resting surface. `forwards.rs`'s `+ New` is the
                        // third instance of the same answer, and `Reconnect` is the fourth: a
                        // recovery control that fills in accent says the recovery is the decision,
                        // and in both of its states it is not — the reader wants their lines back,
                        // not a commitment.
                        //
                        // Left as `primary` it was the third accent on the Terminal tab: the
                        // resource header's kind icon (§4.2) and this tab's own selection rail (§3)
                        // are already two, and `PROMPT.md` §2.1 rule 8 is a cap.
                        .secondary()
                        // `design::size::CONTROL`, not `BAND_CONTROL`: this one is in the body, not
                        // in a 28px band, and §4.13 gives an empty state's action the full control
                        // height. The two numbers being different is the point.
                        .with_size(Size::Size(design::size::CONTROL))
                        .h(design::size::CONTROL)
                        .tab_index(5isize)
                        .tab_stop(false)
                        .tooltip("Open a local shell with the current context.")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.new_local_terminal(window, cx)),
                        ),
                    "New terminal",
                ))
                .into_any_element()
        });
        div()
            .id("terminal-empty-state")
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .child(empty_state_block(dock_empty_state(
                IconName::SquareTerminal,
                "No terminal session",
                "",
                action,
                cx,
            )))
            .into_any_element()
    }

    fn render_terminal_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
            .h(design::size::DOCK_TOOLBAR)
            .px(space::SM)
            .gap(space::SM)
            .items_center()
            .bg(design::role::surface_chrome(cx).alpha(1.0))
            // Below the body, so the boundary is the band's top edge — the same edge the Logs
            // toolbar rules, and the same 1px.
            .border_t_1()
            .border_color(design::role::border_subtle(cx));
        if self.terminal_services.is_none() && self.terminals.is_empty() {
            return bar
                .child(
                    label_small("Select a context to open a terminal.")
                        .text_color(design::role::fg_tertiary(cx))
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
            let mut chips = Vec::with_capacity(self.terminals.len());
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
                    .map(|severity| (design::health_icon(severity), severity));
                // The verdict slot is always reserved, so a session that ends does not slide the
                // title sideways, and it stays empty while the Dock has no verdict rather than
                // claiming a healthy state it has not observed.
                let verdict_slot = div()
                    .id(("terminal-chip-verdict", index))
                    .debug_selector(move || format!("terminal-chip-verdict-{index}"))
                    .flex_none()
                    .w(design::size::STATUS_MARKER)
                    .h(design::size::STATUS_MARKER)
                    .items_center()
                    .when_some(verdict, |this, (icon, severity)| {
                        this.child(
                            Icon::new(icon)
                                .with_size(Size::Size(design::size::STATUS_MARKER))
                                .text_color(design::icon::status(cx, severity)),
                        )
                    });
                let mut chip = Tab::new()
                    .debug_selector(move || format!("terminal-chip-{index}"))
                    .label(title)
                    .aria_label(aria_title.clone())
                    .aria_description(aria_description)
                    .accessibility_id(format!("terminal-session-{index}"))
                    .aria_keyshortcuts("Enter Delete Backspace ArrowLeft ArrowRight Home End")
                    // The chip's own identity mark, and it states its ink because an
                    // `Icon` resolves a missing colour to the theme's foreground
                    // rather than to the tab's: every chip drew the shell mark at
                    // `fg.primary` whatever its state, so the one chip the reader is
                    // on and the eleven beside it carried an identical glyph.
                    .prefix(
                        Icon::new(IconName::SquareTerminal)
                            .with_size(Size::Size(design::icon::IN_TOOLBAR))
                            .text_color(if active {
                                design::icon::active(cx)
                            } else {
                                design::icon::resting(cx)
                            }),
                    )
                    .suffix(verdict_slot)
                    // As on the header's tabs: no cursor, per §9.3.
                    .w(design::size::MAIN_CONTENT_MIN)
                    .h(BAND_CONTROL)
                    .track_focus(&focus)
                    .tab_index(3isize)
                    .tab_stop(active)
                    // As with the header's tabs: the role and the selected state are the Dock's
                    // to say, the paint is the component's.
                    .role(Role::Tab)
                    .aria_selected(active)
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
                    }));
                chip.interactivity()
                    .tooltip(command_tooltip(aria_title.clone()));
                chips.push(chip);
            }
            // gpui-kit's tabs are not a roving-focus group — nothing moves focus when the
            // selection changes — so the arrows, Home, End and Delete stay on the chips, above.
            sessions = sessions.child(
                TabBar::new("terminal-session-bar")
                    .with_size(Size::Size(BAND_CONTROL))
                    .h(BAND_CONTROL)
                    .selected_index(self.active_terminal)
                    .on_click(cx.listener(|this, index, window, cx| {
                        this.activate_terminal(*index, window, cx);
                    }))
                    .children(chips),
            );
        }
        let maximize_label = if self.terminal_maximized {
            "Restore terminal"
        } else {
            "Maximize terminal"
        };
        let mut commands = h_flex().flex_none().gap(space::XS).items_center();
        if self.terminal_available() && !self.terminals.is_empty() {
            commands = commands.child(
                div()
                    .id("terminal-add-target")
                    .track_focus(&self.terminal_add_focus)
                    .border_l(design::border::FOCUS_RAIL)
                    .border_color(design::role::border_subtle(cx).alpha(0.))
                    .focus_visible(|this| this.border_color(design::focus::border(cx)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if presses(&event.keystroke) {
                            cx.stop_propagation();
                            this.new_local_terminal(window, cx);
                        }
                    }))
                    .child(
                        Button::new("terminal-add")
                            .icon(Icon::new(IconName::Plus))
                            .ghost()
                            .with_size(Size::Size(BAND_CONTROL))
                            .tab_index(5isize)
                            .tab_stop(false)
                            .tooltip("Open a local shell with the current context.")
                            .accessibility_label("New terminal")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_local_terminal(window, cx)
                            })),
                    ),
            );
        }
        if (!compact || self.terminal_maximized) && !self.terminals.is_empty() {
            commands = commands.child(
                Button::new("terminal-maximize")
                    .icon(Icon::new(if self.terminal_maximized {
                        IconName::WindowRestore
                    } else {
                        IconName::Maximize
                    }))
                    .ghost()
                    .with_size(Size::Size(BAND_CONTROL))
                    .tab_index(7isize)
                    .tooltip(maximize_label)
                    .accessibility_label(maximize_label)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_terminal_maximized(cx))),
            );
        }
        if self.terminal_available() || !self.terminals.is_empty() {
            commands = commands.child(
                div()
                    .flex_none()
                    .debug_selector(|| "terminal-actions-trigger".to_owned())
                    .child(self.render_terminal_menu("terminal-actions-trigger", cx)),
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
            target.interactivity().tooltip(command_tooltip(title));
            target.into_any_element()
        } else if let Some(context) = self.terminal_context_label() {
            let mut target = div()
                .id("terminal-context")
                .debug_selector(|| "terminal-context".to_owned())
                .flex_1()
                .min_w(px(0.))
                .child(label_text(context.clone()).truncate());
            target.interactivity().tooltip(command_tooltip(context));
            target.into_any_element()
        } else {
            div().flex_1().min_w(px(0.)).into_any_element()
        };
        bar.child(sessions_area)
            // A session that ended is reported by the same one-line chip the tab strip uses, not by
            // the shared `status_message`. That primitive is a full alert with a `Show details`
            // disclosure and a copyable reason block, and it belongs in a 32px row with room to grow
            // — this band is 28px and holds a scrollable session strip, so an alert there either cut
            // its own reason off or pushed the sessions out of the row. The chip carries the verdict
            // in one line; the reason and the next step stay in its tooltip and its accessible name.
            .when_some(
                self.terminals.get(self.active_terminal).and_then(|entry| {
                    let exit = entry.exit.clone()?;
                    let recovery = "Select Restart to open a new session.";
                    Some(format!("{exit}. {recovery}"))
                }),
                |this, description| {
                    this.child(self.render_status_chip_with(
                        "Session ended",
                        Severity::Warning,
                        &description,
                        cx,
                    ))
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

    /// One Terminal actions menu, drawn twice: once in the Terminal tab's own band and
    /// once in the strip's overflow slot.
    ///
    /// `id` is an argument because two live copies of the same trigger used to share one element
    /// id, and the Terminal tab mounts both in the same frame — the strip is above the band and the
    /// band is the Terminal toolbar. A duplicate id is a second node claiming to be the first, so
    /// whichever one the popover anchored to was a matter of paint order.
    fn render_terminal_menu(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
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
        // gpui-kit owns the popup surface and the trigger's dismissal, so the
        // panel only describes the rows.
        //
        // The trigger stays visibly pressed while its menu is up, from the same flag and the same
        // wash the Logs overflow menu uses. This one did not, in either of its two places: gpui-kit
        // moves focus into the menu on open, so the trigger's own hover and press states have
        // nothing left to say, and a menu that appears under an unmarked trigger reads as an
        // unowned popup. One flag covers both copies because only one of them can be open.
        let open_wash = design::state::press_on(
            design::role::surface_chrome(cx),
            design::role::fg_primary(cx),
        );
        let menu_open = self.overflow_menu_open.get();
        let trigger_panel = panel.clone();
        labelled(
            Button::new(id)
                .ghost()
                .with_size(Size::Size(BAND_CONTROL))
                .h(BAND_CONTROL)
                .icon(Icon::new(IconName::Menu))
                .tab_index(8isize)
                .dropdown_caret(true)
                .tooltip("Terminal actions"),
            "Actions",
        )
        // The two words differ on purpose: `Actions` on its own is the vaguest word on the
        // strip, and the announced name says which pane's menu the caret opens.
        .accessibility_label("Terminal actions")
        .when(menu_open, |this| this.bg(open_wash))
        .dropdown_menu(move |menu, _, _| {
            let panel = panel.clone();
            let active_title = active_title.clone();
            let mut menu = menu;
            if has_service {
                let new_panel = panel.clone();
                menu = menu.item(menu_item("New terminal").icon(IconName::Plus).on_click(
                    move |_, window, cx| {
                        new_panel
                            .update(cx, |panel, cx| panel.new_local_terminal(window, cx))
                            .ok();
                    },
                ));
            }
            if has_terminal {
                let restart_panel = panel.clone();
                let restart_title = active_title.clone();
                if can_restart {
                    menu = menu.item(
                        menu_item(format!("Restart {restart_title}"))
                            .icon(design::glyph::action::restart())
                            .on_click(move |_, window, cx| {
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
                    .item(menu_item("Split terminal").icon(IconName::Split).on_click(
                        move |_, window, cx| {
                            split_panel
                                .update(cx, |panel, cx| panel.split_terminal(window, cx))
                                .ok();
                        },
                    ))
                    .item(
                        menu_item(if maximized {
                            "Restore terminal"
                        } else {
                            "Maximize terminal"
                        })
                        .icon(if maximized {
                            IconName::WindowRestore
                        } else {
                            IconName::Maximize
                        })
                        .checked(maximized)
                        .on_click(move |_, _, cx| {
                            maximize_panel
                                .update(cx, |panel, cx| panel.toggle_terminal_maximized(cx))
                                .ok();
                        }),
                    );
                let close_panel = panel.clone();
                menu = menu.item(menu_item("Close terminal").icon(IconName::X).on_click(
                    move |_, window, cx| {
                        close_panel
                            .update(cx, |panel, cx| {
                                panel.close_terminal(active_terminal, window, cx)
                            })
                            .ok();
                    },
                ));
            }
            menu
        })
        // The trigger stays pressed while its menu is up, and the update is on the panel
        // rather than the flag so the strip repaints in the same frame — the same
        // arrangement `render_overflow_menu` uses for the Logs menu.
        .on_open_change(move |open, _window, cx| {
            if let Some(panel) = trigger_panel.upgrade() {
                panel.update(cx, |panel, cx| {
                    panel.overflow_menu_open.set(*open);
                    cx.notify();
                });
            }
        })
        .into_any_element()
    }

    fn log_viewport_height(&self, row_height: Pixels) -> Pixels {
        let height = if self.log().wrap {
            self.log().list_state.viewport_bounds().size.height
        } else {
            self.log()
                .scroll_handle
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
        if self.log().wrap {
            self.log_mut().list_state.scroll_by(distance);
            if distance > px(0.)
                && self.log().follow
                && self.log().list_state.is_scrolled_to_end() == Some(true)
            {
                self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
            }
            return;
        }
        let mut state = self.log_mut().scroll_handle.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        let offset = state.base_handle.offset();
        let max_offset = state.base_handle.max_offset().y;
        let next_y = (offset.y + distance).clamp(-max_offset, px(0.));
        state.base_handle.set_offset(point(offset.x, next_y));
    }

    fn scroll_log_to_top(&mut self) {
        if self.log().wrap {
            self.log_mut().list_state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        } else {
            let mut state = self.log_mut().scroll_handle.0.borrow_mut();
            state.deferred_scroll_to_item = None;
            state
                .base_handle
                .set_offset(point(state.base_handle.offset().x, px(0.)));
        }
        self.log_mut().following = false;
    }

    fn scroll_log_to_end(&mut self) {
        if self.log().wrap {
            self.log_mut().list_state.scroll_to_end();
            if self.log().follow {
                self.log_mut().list_state.set_follow_mode(FollowMode::Tail);
            }
        } else {
            self.log_mut()
                .scroll_handle
                .0
                .borrow_mut()
                .deferred_scroll_to_item = None;
            self.log_mut().scroll_handle.scroll_to_bottom();
        }
        self.log_mut().following = self.log_mut().follow;
    }

    /// Keeps the selection inside the visible rows after the buffer or the filter changed.
    fn clamp_log_selection(&mut self) {
        let last = self.visible_log_count().saturating_sub(1);
        if let Some(selection) = self.log_mut().log_selection.as_mut() {
            selection.anchor = selection.anchor.min(last);
            selection.head = selection.head.min(last);
        }
    }

    /// Buffer index of a visible row, or `None` when the row has no line behind it.
    fn log_row_index(&self, row: usize) -> Option<usize> {
        if self.log_view_is_filtered() {
            self.log().visible_log_indices.get(row).copied()
        } else {
            self.log().buffer.line(row).map(|_| row)
        }
    }

    /// Moves the caret to `row`, dropping any range. A plain arrow press collapses the selection.
    fn focus_log_row(&mut self, row: usize, row_height: Pixels) {
        let row = row.min(self.visible_log_count().saturating_sub(1));
        self.log_mut().log_selection = Some(LogSelection {
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
        self.log_mut().log_selection = Some(LogSelection {
            anchor: previous.anchor,
            head: target,
        });
        self.scroll_log_row_into_view(target, row_height);
    }

    /// Scrolls the minimum amount that brings `row` into the viewport.
    fn scroll_log_row_into_view(&mut self, row: usize, row_height: Pixels) {
        let page = self.log_viewport_height(row_height);
        let rows = (f32::from(page) / f32::from(row_height)).floor().max(1.0) as usize;
        if self.log().wrap {
            let top = self.log().list_state.logical_scroll_top().item_ix;
            if row < top {
                self.log_mut().list_state.scroll_to(ListOffset {
                    item_ix: row,
                    offset_in_item: px(0.),
                });
            } else if row >= top + rows {
                self.log_mut().list_state.scroll_to(ListOffset {
                    item_ix: row + 1 - rows,
                    offset_in_item: px(0.),
                });
            }
            return;
        }
        let state = self.log_mut().scroll_handle.0.borrow_mut();
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
        state.base_handle.set_offset(point(
            offset.x,
            px(next_y.clamp(-f32::from(max_offset), 0.)),
        ));
    }

    /// Raw text of the selected rows, one line each, or `None` when nothing is selected.
    fn log_selection_text(&self) -> Option<String> {
        let selection = self.log().log_selection?;
        let (start, end) = selection.bounds();
        let mut text = String::new();
        for row in start..=end {
            let Some(index) = self.log_row_index(row) else {
                continue;
            };
            let Some(line) = self.log().buffer.line(index) else {
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
        // The two chords that belong to the Dock rather than to the log list, on either tab.
        // `⌘F` is the find bar `UI-SPEC` §16.3 asks for and `⌘W` is §16.4's; both are read here
        // because the Dock's content is the ancestor of the rows, the terminal, and both fields,
        // so this is the one place a chord aimed at the Dock can be caught before the surface
        // inside it swallows it. Everything else with a modifier is left to travel.
        let modifiers = event.keystroke.modifiers;
        let command = modifiers.platform || modifiers.control;
        if command && !modifiers.shift && !modifiers.alt {
            match event.keystroke.key.as_str() {
                "f" if self.active_tab.is_logs() => {
                    self.open_find(cx);
                    cx.stop_propagation();
                    return;
                }
                "w" => {
                    self.close_current_view(window, cx);
                    return;
                }
                _ => {}
            }
        }
        if !self.active_tab.is_logs() || !self.focus_handle.is_focused(window) {
            return;
        }
        // The find bar owns the keyboard while it is open: a stray arrow must move the caret in
        // the field rather than scroll the rows behind it.
        if self.log().find_open {
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
            let previous = self.log().log_selection.unwrap_or_default();
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
        if !self.active_tab.is_logs() || !self.focus_handle.is_focused(window) {
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
        if self.active_tab != DockTab::Terminal || self.active_terminal >= self.terminals.len() {
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
        let selection = self.log().log_selection?;
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
            self.log().buffer.len()
        ))
    }

    /// Handles a pointer press on a row: it moves the caret, and Shift extends the range.
    fn on_log_row_click(&mut self, row: usize, extend: bool, cx: &mut Context<Self>) {
        if row >= self.visible_log_count() {
            return;
        }
        let row_height = log_row_height(cx);
        match (extend, self.log().log_selection) {
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
        let reported = self.log_mut().list_follow_report.replace(None);
        let owner_follows = if self.log().wrap {
            reported.unwrap_or_else(|| self.log().list_state.is_following_tail())
        } else {
            self.uniform_list_at_bottom()
        };
        self.log_mut().following = self.log_mut().follow && owner_follows;
    }
}

impl Render for DockPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The body has to stop taking work when it is not on screen: a filter pass that keeps
        // scoring lines behind a collapsed body is work nobody can see, and a stream that keeps
        // following a tail it is not showing is the exact thing §16.3 says not to do.
        let collapsed = self.body_collapsed(window);
        if self.active_tab.is_logs() && !collapsed {
            self.sync_scroll_follow();
        }
        // The filter pass continues here, one bounded budget per frame, so a burst of log lines
        // never turns into one full scan of the buffer per batch. The pass only asks for the next
        // frame while its own budget lasts: a live stream outruns the pass, so an unbounded ask
        // would keep the window redrawing long after the stream could catch up.
        if self.active_tab.is_logs() && !collapsed {
            self.continue_filter_pass(cx);
        }
        self.sync_focus_handles();
        let selection_aria = self.log_selection_aria();
        v_flex()
            .id("dock-panel")
            .debug_selector(|| "dock-panel".to_owned())
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            // The Dock's three planes, in the ladder's own order.
            //
            // `role::surface_chrome` is the strip and the toolbar band, and it is the plane the
            // whole shell is chrome on, so the Dock's chrome band and the title bar and the status
            // bar above it read as one family and the Dock reads as a region rather than as a fifth
            // band of content. `role::surface_content` is the body — the log pane and the terminal
            // panes' own canvas.
            //
            // The body used to be `surface.inset`, the terminal's and the log body's darkest step.
            // That put the Dock's content plane *below* the resource table's rather than beside it:
            // in Dark the table sits on `#111216` and the Dock's own body on `#050607`, so the two
            // tabs of one Dock were cut out of two different ladders and crossing between them moved
            // the content plane under the reader. `surface.inset` is still where content *recedes*
            // — it is the terminal canvas, which the session draws itself — but a log is content
            // that is read, and it belongs on the plane the table and the YAML editor use.
            //
            // The step from chrome to content is 1.02–1.06:1, which is a hint and not a boundary,
            // and the design says so. What carries the boundary above the strip is the shell's own
            // Dock splitter — the interactive handle, with its 1px line, 20px hit area and four
            // states — and what carries it below the strip is the toolbar's top rule. This file
            // draws neither, because a boundary has exactly one owner and this row is not it.
            .bg(design::role::surface_content(cx).alpha(1.0))
            .text_color(design::role::fg_primary(cx))
            .key_context("Dock")
            .track_scroll(&self.width_handle)
            // The tab strip is unconditional. A maximised terminal used to take it away with it,
            // and so does a body collapsed to nothing: in both cases the Dock was left with no
            // way to switch, which is the one thing the strip exists for. §16.2.
            .child(self.render_tabs(window, cx))
            .when(!collapsed, |this| {
                this.child(
                    div()
                        .id("dock-content")
                        .debug_selector(|| "dock-content".to_owned())
                        .role(Role::TabPanel)
                        .aria_label(tab_panel_label(self.active_tab, self.terminals.len()))
                        .accessibility_id(format!("dock-panel-{}", self.active_tab.slot()))
                        .when(self.active_tab.is_logs() && self.log_list_has_lines(), |this| {
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
                        // The rail is always reserved, so focusing the tab content does not move
                        // the log rows and the terminal inside it.
                        .border_l(design::border::FOCUS_RAIL)
                        .border_color(design::role::border_subtle(cx).alpha(0.0))
                        .focus_visible(|style| style.border_color(design::focus::border(cx)))
                        .on_action(cx.listener(Self::log_copy_action))
                        .on_action(cx.listener(Self::restart_terminal_action))
                        .on_key_down(cx.listener(Self::on_log_key_down))
                        .child(self.render_content(window, cx)),
                )
            })
    }
}

///
/// `common::empty_state` is the shared shape, but it draws itself on a padded
/// card, and the Dock is the one surface that has to hold an empty state at its
/// own minimum height: at `design::size::DOCK_MIN` the log body is 58px, and the
/// card's padding alone is 48 of them. So the Dock draws the same parts on the
/// app's type scale, and names the title and the hint, which are the two rows
/// that have to fit.
///
/// An empty `hint` draws no description at all, which is §4.13's default rather than a
/// special case: "说明 默认**没有**". The three states that pass a sentence are the three where
/// the sentence is the only thing on screen that says *why*, and the one that does not is the
/// "really none" state, whose action button is already labelled with the instruction.
fn dock_empty_state(
    icon: IconName,
    title: &'static str,
    hint: impl Into<SharedString>,
    action: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let hint = hint.into();
    let has_hint = !hint.trim().is_empty();
    let loading = icon == IconName::LoaderCircle;
    // Every state in this panel is drawn here, the waiting one included.
    //
    // The loader used to be handed to `common::empty_state_with_action` instead, on the theory
    // that the shared primitive owns the waiting treatment. It does not own the *shape*: that
    // primitive draws gpui-kit's `Empty`, which is a 24px-padded card with a dashed 1px frame,
    // a 32px icon tile and a 14px title — so the Logs panel showed a framed card with a dashed
    // border while every other Logs state showed bare text on the inset plane, 200px apart, and
    // the two were the same control. Three §2 rule 5 violations in one state (a decorative
    // stroke, a 12px radius where §2.2 gives cards `r-lg`, 24px of padding where §4.13 centres
    // the group), one of them a dashed border, which is the one thing §4.13's drawing does not
    // contain anywhere. The Dock's minimum height is 142px; a padded card does not fit in it.
    //
    // What the shared primitive is actually good for is the sweep, and §4.14 grades this state
    // as a spinner with no layout change. `spinner` is that, and it stops itself under reduced
    // motion. The 160px accent progress bar the card carried went with it: §4.14 asks for
    // progress only past 2s and only with a number to show, and waiting for a first log line has
    // neither — so the bar was motion that also spent a third of the panel's accent budget on a
    // state with nothing wrong in it.
    let glyph: AnyElement = if loading {
        spinner(
            icon,
            design::icon::incidental(cx),
            Size::Size(design::icon::LEAD),
        )
    } else {
        Icon::new(icon)
            .with_size(Size::Size(design::icon::LEAD))
            .text_color(design::icon::incidental(cx))
            .into_any_element()
    };
    div()
        .id(title)
        .debug_selector(|| "empty-state".to_owned())
        .size_full()
        .min_w(px(0.))
        .min_h(px(0.))
        // A `Status` role for the one state that is a status rather than a place: §9.3's rule is
        // that a reader who cannot see the sweep still hears that the pane is working.
        .role(if loading { Role::Status } else { Role::Region })
        .aria_label(title)
        .when(has_hint, |this| this.aria_description(hint.clone()))
        .child(
            v_flex()
                .size_full()
                .min_w(px(0.))
                .min_h(px(0.))
                .items_center()
                .justify_center()
                .text_center()
                .text_color(design::role::fg_primary(cx))
                // The rows are spaced by what each pair *is*, not by one blanket gap, because
                // `design::size::DOCK_MIN` leaves the Terminal tab 86px of body and this block
                // does not fit in it at 8px everywhere. `⌘⌃Home` sets the Dock to its floor, so
                // "at the floor" is a state a reader reaches with one keystroke, and a button
                // whose bottom two pixels are under the toolbar's rule is the 1px misregistration
                // `UI-SPEC` §8's zero-roughness list is about. `the_tern_empty_state_fits_at_the_
                // docks_minimum_height` holds the arithmetic against the tokens.
                //
                // 24 (icon) + 6 (icon↔text, §2.4's tight value) + 20 (title 15/20) + 4 + 28
                // (the §4.13 action) = 82, in 86.
                .child(glyph)
                .child(
                    div()
                        .debug_selector(|| "empty-state-title".to_owned())
                        .min_w(px(0.))
                        .mt(space::ICON)
                        .child(label_panel_title(title)),
                )
                .when(has_hint, |this| {
                    this.child(
                        div()
                            .debug_selector(|| "empty-state-hint".to_owned())
                            .w_full()
                            .min_w(px(0.))
                            .mt(space::XXS)
                            // `body`, not `caption`. §2.3 names `caption` for section headings and
                            // table headers and calls the empty state's own description a
                            // *paragraph* ("40ch（空状态说明）"), and its rule for the four levels is
                            // that adjacent ones differ by 2px **and by weight** — the example it
                            // gives is `body 13` against `metadata 11` reading as one level. This
                            // was that same pair with the weights not separated either: `title 15/600`
                            // directly above `caption 11/600`, four pixels apart and the same weight,
                            // so the sentence read as a second heading rather than as the note under
                            // one, and it also carried `caption`'s +0.06em tracking, which is
                            // letterspacing meant for short uppercase labels and not for a sentence.
                            //
                            // 13/400 against 15/600 is the pair the scale is built for: 2px and a
                            // weight, so the two levels separate in greyscale.
                            //
                            // The cap is the design token, so every panel wraps the
                            // same sentence at the same width.
                            .max_w(design::size::EMPTY_MEASURE)
                            // The description is the quieter half of the pair, so it wears the
                            // quieter ink. It inherited the title's `fg.primary` from the column
                            // above, and a sentence is wider than a title: the two lines came out
                            // the same weight and the second one read as the louder of the pair,
                            // which is the wrong way round for a state the reader is meant to read
                            // once. `UI-SPEC` §4.13 draws the title at `fg.primary` and leaves the
                            // explanation below it.
                            .child(label_body(hint).text_color(design::role::fg_secondary(cx))),
                    )
                })
                .when_some(action, |this, action| {
                    this.child(div().flex_none().mt(space::XS).child(action))
                }),
        )
        .into_any_element()
}

/// Names the block the log body spends on an empty state.
///
/// The wrapper is what tells a body that has nothing to say apart from a body with lines in it,
/// so the state the reader is looking at is addressable without reading the text in it.
fn empty_state_block(state: AnyElement) -> AnyElement {
    div()
        .id("empty-state-block")
        .debug_selector(|| "empty-state-block".to_owned())
        .size_full()
        .min_w(px(0.))
        .min_h(px(0.))
        .child(state)
        .into_any_element()
}

/// What the list knows about a row: the selection it belongs to, and the panel that owns it.
#[derive(Clone)]
struct LogRowContext {
    selection: Option<LogSelection>,
    panel: WeakEntity<DockPanel>,
    /// The rows `⌘F` matched, and which of them the reader is on. `None` while the find bar is
    /// closed, so the common case costs one `Option` test per row and no background at all.
    find: Option<FindHighlight>,
}

/// Which rows the find bar matched, in the list's own row positions.
///
/// The rows are positions and not buffer indices because that is what a row knows about itself,
/// and it is what the find bar has to scroll to. It is sorted because the list asks every visible
/// row whether it is a hit, and a binary search over a hundred hits is cheaper than a set built
/// per frame for the same answer.
#[derive(Clone)]
struct FindHighlight {
    rows: Rc<Vec<usize>>,
    active: usize,
}

impl FindHighlight {
    /// `Some(true)` for the hit the reader is on, `Some(false)` for a hit, `None` for a miss.
    fn state(&self, row: usize) -> Option<bool> {
        self.rows
            .binary_search(&row)
            .ok()
            .map(|index| index == self.active)
    }
}

/// The shape the list draws a row in: wrap or no wrap, and the columns the buffer reserves for
/// timestamps. The list picks one shape per pass, so the row takes it whole instead of loose flags.
#[derive(Clone, Copy)]
struct LogRowGeometry {
    wrap: bool,
    timestamp_reserve: usize,
}

/// One log line, drawn as the three columns `UI-SPEC` §16.3 specifies: a timestamp, the level,
/// and the message.
///
/// The level lane is a **mark and a word**, not three renderings of one thing and not one. It was
/// a coloured dot, a coloured glyph *and* the word at once, which put a field of six-pixel marks
/// down the left of a hundred visible lines and made the word — the only one of the three that
/// survives a greyscale screenshot — the smallest. The glyph went, because two marks for one fact
/// is not redundancy. The dot comes back, because a mark with the word *beside* it is: the reader
/// scanning the column for the one line that is not ordinary gets a column of shapes, the reader
/// reading gets a word, and the word is the product's own four rather than whatever token the pod
/// happened to write. One mark, one word, two inks — the pairing `panels/forwards.rs` and the Helm
/// release table already use.
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
        timestamp_reserve,
    } = geometry;
    // One read of the configured data role, so the glyph, the line the row is tall enough for and
    // every column the row reserves come from the same setting.
    let typography = crate::settings::data_typography(cx);
    let row_height = typography.line_height;
    // The product's own four-word vocabulary, not the token the pod happened to write.
    //
    // `LogLine::label` is the raw first token — `FATAL`, `CRIT`, `WARNING`, `TRACE`, and on
    // `k8s-apiserver` an `I0929` that is a timestamp fragment rather than a level at all — and it
    // is a variable-width word in a lane sized for the widest of them. Drawing it put a
    // five-letter token, a two-letter one and a six-character non-word in the same column on the
    // same screen, and a reader scanning for the one line that is not ordinary had to read the
    // column rather than look at it. `level_word` is the four words `LogLevelScope` filters on,
    // so the word on a row and the level filter above it cannot disagree about what `WARN` means,
    // and it is the word that survives a greyscale screenshot.
    let level_word = line.level_word();
    let timestamp = line.timestamp.clone().unwrap_or_default();
    let display_message = line.display_message().clone();
    let copy_text = line.raw.as_ref().to_owned();
    // A long line shows less than the copy button puts on the clipboard, so the control says so.
    let long = line.display_columns() > LOG_COPY_COLUMN_THRESHOLD;
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
        level_word,
        display_message
    );
    let mut row = h_flex()
        .id(("dock-log-row", row_id))
        .debug_selector(|| "dock-log-row".to_owned())
        .flex_none()
        .min_w(px(0.))
        .px(space::SM)
        .font(font.clone())
        .font_features(typography.features.clone())
        .text_size(px(f32::from(typography.size)))
        .line_height(px(f32::from(typography.line_height)))
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
            design::focus::border(cx)
        } else {
            design::role::border_subtle(cx).alpha(0.0)
        });
    if selected {
        row = row.bg(design::text_selection::background(cx));
    } else if let Some(active) = row_context
        .find
        .as_ref()
        .and_then(|find| find.state(row_id))
    {
        // Selection outranks a find hit: a row the reader has selected is a row they are acting
        // on, and a highlight behind it would be two answers to the same question.
        row = row.bg(if active {
            design::search_match::active_background(cx)
        } else {
            design::search_match::background(cx)
        });
    }
    if wrap {
        row = row.w_full().items_start().gap(space::SM);
    } else {
        row = row
            .w(log_row_width(&line, timestamp_reserve, &typography))
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
        .text_color(log_timestamp_role(cx))
        .child(timestamp.clone());
    if !timestamp.is_empty() {
        timestamp_cell
            .interactivity()
            .tooltip(command_tooltip(timestamp.clone()));
    }
    // The level lane is a mark *and* a word: `6px mark + space::SM + the canonical word`, on the
    // fixed lane from `level_column_width` so every message in the buffer starts in the same
    // column and a column of `ERROR` lines reads as a column rather than as a ragged edge.
    //
    // The mark was removed once, on the argument that "a 6px dot is a colour and nothing else",
    // and that is true of a dot that has no word beside it. The word is here — and it is the
    // product's own four words, so the word survives greyscale — which makes the mark the
    // *second* channel instead of the only one: the reader scanning for the one line that is not
    // ordinary gets to look at a column of shapes, and the reader reading gets a word. That is
    // the same pairing `panels/forwards.rs` and the Helm release table already use, with the same
    // two inks: `design::role::status_for` for the mark, `status_word_for` for the word.
    //
    // A line the parser found no level in gets no cell. Reserving the column for an empty string
    // is a 52px gutter of nothing in front of every line of a stream that writes bare text, and
    // `k8s-apiserver`'s `I0929 17:10:31.984750 …` lines are exactly that: the whole prefix reads
    // as a timestamp, the level is empty, and the reader pays for a column that says nothing on
    // every line of the stream.
    let level_cell = h_flex()
        .debug_selector(|| "dock-log-severity".to_owned())
        .flex_none()
        .w(level_column_width(&typography))
        .h(row_height)
        .items_center()
        .gap(space::SM)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(
            div()
                .flex_none()
                .size(design::size::STATUS_DOT)
                .rounded_full()
                .bg(log_level_mark(line.severity, cx)),
        )
        .child(
            div()
                .flex_none()
                .min_w(level_word_floor(&typography))
                .h(row_height)
                .items_center()
                .overflow_hidden()
                .text_ellipsis()
                .text_color(log_level_role(line.severity, cx))
                .child(level_word),
        );
    let prefix = h_flex()
        .flex_none()
        .gap(space::SM)
        .child(timestamp_cell)
        .when(!level_word.is_empty(), |this| this.child(level_cell));
    let mut message = div()
        .debug_selector(|| "dock-log-message".to_owned())
        .min_w(px(0.))
        // One ink for every line. The message used to wear `danger` outright on an `ERROR` line,
        // which is the "wall of colour" the brief rules out: a message is a paragraph of arbitrary
        // length, and painting all of it — every wrapped continuation line included — turns one
        // row into a block of red in a surface whose reader scans for *the line that is not
        // ordinary*. A hundred errors then reads as a hundred red paragraphs, and the severity
        // stops being a signal because it is the background.
        //
        // Severity is carried by the level lane instead: a mark and a word, both in the severity's
        // own ink, on a fixed column. That is the channel that survives greyscale, that stays
        // legible when the pane is a hundred rows deep, and that costs one word of colour rather
        // than the whole line. What is left here is body copy, and body copy is `fg_secondary`.
        .text_color(design::role::fg_secondary(cx))
        .child(display_message.clone());
    if wrap {
        message = message.flex_1().whitespace_normal();
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
    // It is *reserved* on every row and *painted* on one. The slot is always there so a row never
    // changes width when the pointer crosses it, and the paint is on the row under the pointer or
    // holding the caret, because ten visible lines used to draw ten copies of the same icon down
    // the right edge — a wall of glyphs on the one surface where the reader is scanning hardest for
    // the line that is not ordinary.
    let copy_label = if long {
        "Copy full log line"
    } else {
        "Copy log line"
    };
    // The control is out of the tab order on purpose: the row itself takes the caret and the
    // selection chord copies, so a second stop would make the copy path a Tab away from the
    // line it copies.
    //
    // Its height is the row's own line, not the 28px control height. A 28px control in a row is
    // what made the grid 28px tall whatever the reader's data line was: the tallest child decides
    // a row's height, and a control the reader only reaches for on one row at a time was deciding
    // it for all ten thousand.
    let copy_width = design::size::CONTROL;
    let copy_height = row_height;
    let copy = Button::new(("dock-log-copy", row_id))
        .icon(Icon::new(IconName::Copy))
        .ghost()
        .with_size(Size::Size(copy_width))
        .h(copy_height)
        .tab_index(-1isize)
        .tab_stop(false)
        .accessibility_label(copy_label)
        .tooltip(if long {
            "Copy the full line. The row shows only part of it.".to_owned()
        } else {
            copy_label.to_owned()
        })
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
            cx.stop_propagation();
        });
    message = h_flex()
        .min_w(px(0.))
        .gap(space::SM)
        .when(wrap, |this| this.flex_1().items_start())
        .when(!wrap, |this| this.flex_none().items_center())
        .child(message)
        .child(
            div()
                .id(("dock-log-copy-target", row_id))
                .debug_selector(|| "dock-log-copy".to_owned())
                .flex_none()
                // The slot is always in the flow, so no row changes width when the pointer
                // crosses it, and the paint is on one row at a time. The caret row keeps its
                // control painted because the reader has already chosen that line — the pointer
                // is on the keyboard now, and a control that vanishes the moment the pointer
                // leaves is a control the keyboard path cannot see.
                .opacity(if focused { 1.0 } else { 0.0 })
                .hover(|this| this.opacity(1.0))
                .child(copy),
        );
    message
        .interactivity()
        .tooltip(command_tooltip(display_message));

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

    use gpui_kit::{
        Focusable, Modifiers, ScrollDelta, ScrollWheelEvent, Size, TestAppContext, TouchPhase,
        VisualTestContext, size,
    };

    use super::*;
    use crate::panels::logs::{LOG_TIMESTAMP_COLUMNS, LogEvent, LogSink, LogSubscription};

    /// gpui-kit installs its own theme and global state, so a test brings up only what the Dock
    /// reads: the settings store behind the data typography, and the keymap the close hint reads
    /// its chord from.
    fn init_app(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            crate::settings::init(cx);
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
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

    /// A second Pod, for the cases that need a target the Dock is not already holding.
    fn other_request() -> LogRequest {
        LogRequest {
            namespace: Some("team-a".into()),
            name: "api-7d2f".into(),
            containers: vec!["app".into()],
        }
    }

    fn third_request() -> LogRequest {
        LogRequest {
            namespace: Some("team-a".into()),
            name: "cache-0".into(),
            containers: vec!["cache".into()],
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
        Entity<DockPanel>,
        Rc<RefCell<FakeLogs>>,
        &mut VisualTestContext,
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
        Entity<DockPanel>,
        Rc<RefCell<FakeLogs>>,
        &mut VisualTestContext,
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
    fn scroll_log_list(selector: &'static str, cx: &mut VisualTestContext) {
        let list = cx.debug_bounds(selector).expect("the log list");
        cx.simulate_event(ScrollWheelEvent {
            position: list.center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(120.))),
            modifiers: Default::default(),
            touch_phase: TouchPhase::Moved,
        });
    }

    fn push(state: &Rc<RefCell<FakeLogs>>, events: Vec<LogEvent>, cx: &mut VisualTestContext) {
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

    /// A wrapped row is whole data lines, at every width, and never draws outside itself.
    ///
    /// Two defects met in the narrow-Dock path and both were invisible in a screenshot of a wide
    /// window. `flex_1` on a child of a *column* flexbox sizes the child's height, not its width,
    /// so the message took its width from its own content: a 600-character line was laid out 4320px
    /// wide inside a 400px row, wrapped nowhere, and ran off the right edge while "wrap long lines"
    /// was switched on. And because the prefix sat on a line of its own, the row's pitch was
    /// 18 + `space::XS` + 18 = 40px for a one-line message, so the continuation lines of a wrapped
    /// line were further apart than the 18px the first line was set in.
    ///
    /// The row is one layout at every width now, so the invariant is one number: a row is
    /// `n * typography.line_height` for some whole `n`, and the message column is inside it.
    /// The three controls on the right of the strip are the same size, and all of them are
    /// findable with a pointer.
    #[gpui_kit::test]
    fn the_three_strip_controls_are_one_size_and_findable(cx: &mut TestAppContext) {
        let (_panel, _state, cx) = setup(cx);
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let mut heights = Vec::new();
        for (name, selector) in [
            ("collapse", "dock-collapse"),
            ("overflow", "dock-overflow"),
            ("close", "dock-close"),
        ] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("the {name} control is laid out"));
            let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            assert!(
                height >= design::border::HIT.into(),
                "the {name} control is {height}px tall and a pointer has to find it without aiming"
            );
            assert!(
                width >= design::border::HIT.into(),
                "the {name} control is {width}px wide and a pointer has to find it without aiming"
            );
            heights.push((name, height));
        }
        let (_, first) = heights[0];
        for (name, height) in &heights[1..] {
            assert!(
                (height - first).abs() <= 0.5,
                "the strip's three controls are {heights:?}, and a row of three different heights \
                 is a row where the middle one is the wrong target"
            );
            let _ = name;
        }
    }

    /// Dragging the Dock to its minimum height and back leaves the body exactly where the two
    /// fixed bands say it should be, every time.
    ///
    /// `UI-SPEC` §11.3 fixes the minimum at `design::size::DOCK_MIN` and derives it: 28 of tab strip
    /// + 28 of toolbar + 28 of banner + three default log lines. So the chrome has to add up to 84
    /// for the minimum to still buy three lines, and the body has to be the Dock's height minus
    /// exactly those bands — with no rounding, no cached measurement and no state that remembers
    /// the height the Dock used to be. The banner was `design::size::ROW` (32) where the arithmetic
    /// says 28, which made the chrome 88 and the minimum worth two and a half lines.
    #[gpui_kit::test]
    fn the_log_body_keeps_its_height_across_the_minimum_drag(cx: &mut TestAppContext) {
        // The drag itself belongs to the shell; what this holds is the half the Dock owns, which is
        // that nothing inside the panel latches a height. So the sequence is the one a reader
        // performs: down to the minimum, one pixel past it, up to a comfortable height, back down.
        for (label, dock_height) in [
            ("at the minimum", design::size::DOCK_MIN),
            ("one below", design::size::DOCK_MIN - px(1.)),
            ("comfortable", px(320.)),
            ("back down", design::size::DOCK_MIN),
        ] {
            let (panel, state, cx) = setup_in_dock(cx, Some(px(960.)), Some(dock_height));
            push(
                &state,
                (0..8)
                    .map(|index| {
                        LogEvent::Line(format!("2026-09-22T21:14:0{index}Z INFO ready {index}"))
                    })
                    .collect(),
                cx,
            );
            cx.simulate_resize(REAL_WINDOW);
            cx.run_until_parked();
            let data_line = f32::from(cx.update(|_, cx| log_row_height(cx)));
            let strip = cx.debug_bounds("dock-tabs-row").expect("the strip");
            let toolbar = cx.debug_bounds("dock-log-toolbar").expect("the toolbar");
            let body = cx.debug_bounds("dock-log-body").expect("the log body");
            let row = cx.debug_bounds("dock-log-row").expect("a log row");
            let expected = f32::from(dock_height)
                - f32::from(strip.size.height)
                - f32::from(toolbar.size.height);
            assert!(
                (f32::from(strip.size.height) - f32::from(design::size::DOCK_TABS)).abs() <= 1.0,
                "{label}: §16.2 keeps the strip at 28px whatever the body is doing"
            );
            assert!(
                (f32::from(toolbar.size.height) - f32::from(design::size::DOCK_TOOLBAR)).abs()
                    <= 1.0,
                "{label}: the toolbar is 28px too, so crossing a tab does not move the log body"
            );
            assert!(
                (f32::from(body.size.height) - expected).abs() <= 1.0,
                "{label}: the log body drew {:.2}px against {expected:.2}px of room, so something \
                 inside the panel is holding a height the drag did not ask for",
                f32::from(body.size.height)
            );
            assert!(
                (f32::from(row.size.height) - data_line).abs() <= 1.0,
                "{label}: the row is still the reader's data line at the bottom of the drag"
            );
            assert!(
                !panel.read_with(cx, |panel, _| panel.log_body_is_too_short()),
                "{label}: three log lines is what the minimum was derived from, so a body of \
                 {expected:.0}px must not report itself too short to read"
            );
            assert!(
                f32::from(body.size.height) >= data_line * 3.0,
                "{label}: a body of {:.0}px cannot show the three lines §11.3 derived the \
                 minimum from",
                f32::from(body.size.height)
            );
        }
    }

    #[gpui_kit::test]
    fn a_wrapped_log_row_is_whole_data_lines_at_every_width(cx: &mut TestAppContext) {
        for (label, width) in [("wide", None), ("narrow", Some(px(400.)))] {
            let (_panel, state, cx) = setup_in_dock(cx, width, Some(REAL_DOCK_HEIGHT));
            // One line of ordinary words and one 600-character unbroken token: a wrapper that only
            // breaks on spaces passes the first and fails the second.
            push(
                &state,
                vec![
                    LogEvent::Line(format!(
                        "2026-09-22T21:14:02.331Z INFO {}",
                        "word ".repeat(20).trim_end()
                    )),
                    LogEvent::Line(format!(
                        "2026-09-22T21:14:03.331Z ERROR {}",
                        "x".repeat(600)
                    )),
                ],
                cx,
            );
            cx.simulate_resize(REAL_WINDOW);
            cx.run_until_parked();
            let data_line = f32::from(cx.update(|_, cx| log_row_height(cx)));
            let row = cx
                .debug_bounds("dock-log-row")
                .expect("a log row is laid out");
            let message = cx
                .debug_bounds("dock-log-message")
                .expect("the log message is laid out");
            // The row is a stack of lines, so "every line is the data line" and "the row is a whole
            // number of data lines" are the same statement. A hundredth of a line is the rounding a
            // fractional device scale leaves behind.
            let lines = f32::from(row.size.height) / data_line;
            assert!(
                (lines - lines.round()).abs() <= 0.06,
                "{label}: the row drew {:.2}px, which is {lines:.3} data lines, so its \
                 continuation lines are not the same line as its first one",
                f32::from(row.size.height)
            );
            assert!(
                lines >= 2.0,
                "{label}: the fixture has to actually wrap, and this row is {lines:.2} lines"
            );
            assert!(
                (f32::from(message.size.height) - f32::from(row.size.height)).abs() <= 1.0,
                "{label}: the row is {row:?} and its text is {message:?}, so something other than \
                 the text is deciding the row's height"
            );
            assert!(
                message.origin.x >= row.origin.x && message.right() <= row.right() + px(1.),
                "{label}: the message is drawn outside its row. row {row:?}, message {message:?}"
            );
        }
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
            assert_eq!(panel.log().buffer.text(), "second\nthird\n");
            assert_eq!(panel.log().buffer_bytes, 11);
        });
    }

    struct CompactDockHarness {
        dock: Entity<DockPanel>,
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
    ) -> (Entity<DockPanel>, &mut VisualTestContext) {
        let (Some(width), Some(height)) = (width, height) else {
            return cx.add_window_view(|_, cx| DockPanel::new(cx));
        };
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let dock = cx.new(DockPanel::new);
            CompactDockHarness {
                dock,
                width: Some(width),
                height,
            }
        });
        let dock = harness.read_with(cx, |harness, _| harness.dock.clone());
        (dock, cx)
    }

    /// A window the app allows, tall enough for the Dock to have a body.
    ///
    /// It used to be `design::size::WINDOW_MIN`, 960x640. `UI-SPEC` §11.3 says a window in
    /// 640-759 collapses the Dock's body to nothing and leaves the tab strip, so the old constant
    /// was a window in which almost none of what these tests assert about the log body can be on
    /// screen. The width is still the product's narrowest, so the compact breakpoint is still
    /// measured; the height is above `design::size::DOCK_COLLAPSE_BELOW`, which is the height
    /// rule's whole subject. `a_short_window_leaves_only_the_tab_strip` is where the other half
    /// is held.
    const REAL_WINDOW: Size<Pixels> = size(px(960.), px(800.));
    /// A window in the band `UI-SPEC` §11.3 folds the Dock's body away in.
    const SHORT_WINDOW: Size<Pixels> = size(px(960.), px(700.));
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
        dock: Entity<DockPanel>,
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
    #[gpui_kit::test]
    fn dock_header_close_control_is_visible_and_runs_the_toggle_action(cx: &mut TestAppContext) {
        init_app(cx);
        let toggled = Rc::new(Cell::new(0));
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let dock = cx.new(DockPanel::new);
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
        cx.simulate_click(close.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(toggled.get(), 1, "the control runs the ToggleDock action");
    }

    /// The Logs empty state has to name a control, and there has to be a control. It used to be an
    /// icon, a title, and a sentence naming a menu that is not on screen, while the two sibling
    /// states in the same panel - the Terminal empty state and the log failure state - both had a
    /// real button.
    #[gpui_kit::test]
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
        assert!(
            f32::from(open.size.height) >= f32::from(design::size::HIT_MIN),
            "the control is grabbable: {open:?}"
        );
        assert!(
            open.origin.y >= band.origin.y && open.bottom() <= band.bottom(),
            "the control shares the band that owns every other log command: {open:?} in {band:?}"
        );
        // `UI-SPEC` §16.2 draws the band under the body, so "the control shares the band" and
        // "the body sits above it" are the same fact read from two ends.
        assert!(
            state.bottom() <= band.origin.y,
            "the control is below the body: {open:?} in {band:?}"
        );
        assert!(panel.read_with(cx, |panel, _| panel.log_band_is_shell()));
    }

    /// The `Open Logs` control is painted with the isobaric band and nowhere else. Once a target
    /// and a source are both there, the log controls own the band, and a control that reopens what
    /// is already open must not take a tab stop.
    #[gpui_kit::test]
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

    /// Every control in a 28px band has to *fit* in it, on both tabs, and fit at the same height.
    ///
    /// A band is 28px including the 1px rule that separates it from the body, so its interior is
    /// 27px, and a 28px control centred in 27px hangs half a pixel over the rule and half a pixel
    /// into the status bar's own hairline. That is not visible in a capture and it is not a
    /// rounding curiosity either: it is the 1px misregistration §8's zero-roughness list is about,
    /// and the assertion in `the_log_body_empty_state_offers_a_control` is what measured it.
    ///
    /// The second half is the one a screenshot cannot make at all. gpui-kit's labelled `Button`
    /// reads `Size::Size(px)` as horizontal padding and takes its height from its text, so a
    /// control that was *asked* for 28px measured 20px while the one beside it, drawn by hand,
    /// measured 28px — both on the same row.
    #[gpui_kit::test]
    fn band_controls_fit_inside_the_band_and_match_each_other(cx: &mut TestAppContext) {
        init_app(cx);
        fn check(cx: &mut VisualTestContext, band: &'static str, controls: &[&'static str]) {
            let edges = cx
                .debug_bounds(band)
                .unwrap_or_else(|| panic!("{band} is laid out"));
            assert!(
                (f32::from(edges.size.height) - f32::from(design::size::DOCK_TOOLBAR)).abs() <= 1.0,
                "{band} is {}px, not the 28px the drawing gives it: {edges:?}",
                f32::from(edges.size.height)
            );
            for control in controls {
                let bounds = cx
                    .debug_bounds(control)
                    .unwrap_or_else(|| panic!("{control} is laid out"));
                assert!(
                    bounds.origin.y >= edges.origin.y && bounds.bottom() <= edges.bottom(),
                    "{control} runs past the {band} it sits in: {bounds:?} in {edges:?}"
                );
                assert!(
                    (f32::from(bounds.size.height) - f32::from(BAND_CONTROL)).abs() <= 1.0,
                    "{control} is {}px tall beside controls of {}px in the same {band}",
                    f32::from(bounds.size.height),
                    f32::from(BAND_CONTROL)
                );
            }
        }

        // No source: the band is the status line and the one push button in the Dock's body row.
        // `setup` opens a stream, so this one starts from the bare panel.
        {
            let (panel, cx) = add_dock_window(cx, None, None);
            assert!(panel.read_with(cx, |panel, _| panel.log_band_is_shell()));
            cx.simulate_resize(REAL_WINDOW);
            cx.run_until_parked();
            check(cx, "dock-log-toolbar-shell", &["dock-open-logs"]);
        }

        // Streaming: the band carries `Follow`, which is drawn here rather than by `Button` and is
        // therefore the one control whose height nothing else in the crate can set for it.
        {
            let (panel, state, cx) = setup(cx);
            panel.update(cx, |panel, cx| {
                panel.set_log_factory(
                    Some(Rc::new(|_request, _options, _sink| {
                        Box::new(FakeSubscription)
                    })),
                    cx,
                );
                panel.open_logs(request(), cx);
            });
            push(&state, vec![LogEvent::Line("INFO up".into())], cx);
            cx.simulate_resize(REAL_WINDOW);
            cx.run_until_parked();
            assert!(!panel.read_with(cx, |panel, _| panel.log_band_is_shell()));
            check(cx, "dock-log-toolbar", &["dock-follow", "dock-log-level"]);
        }
    }

    /// The Logs band is isobaric with the Terminal band. Dropping the whole 40px toolbar when
    /// there is no source moved the content's top edge by a full toolbar on a high-frequency
    /// interaction.
    #[gpui_kit::test]
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
            (f32::from(band.size.height) - f32::from(design::size::DOCK_TOOLBAR)).abs() <= 1.0,
            "the band keeps the toolbar height with no source: {band:?}"
        );
        assert!(
            (f32::from(band.origin.y - body.bottom()) - f32::from(design::border::LINE)).abs()
                <= 1.0,
            "the body ends above the band and its rule"
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
        assert!(
            (f32::from(terminal_body.bottom()) - f32::from(body.bottom())).abs() <= 1.0,
            "and it does not move the edge the band rules off from either"
        );
    }

    /// At `design::size::DOCK_MIN` the Terminal tab has 86px of body and the Logs tab 58px, so
    /// the empty state has to be built out of rows that fit rather than out of a padded card.
    ///
    /// The Logs "really none" state used to be a title plus a sentence and this test asserted both
    /// of them stayed inside the body. `UI-SPEC` §4.13's table gives that row no 说明 — "默认
    /// **没有**" — so the state is now title plus the band's `Open Logs`, and the sentence is gone.
    /// The guarantee underneath the test is not "there is a sentence and it fits"; it is "no row of
    /// this panel escapes the body at the panel's own minimum height", and that has to keep holding
    /// for the state that does carry a button.
    ///
    /// The arithmetic is the point, so it is written against the tokens rather than against a
    /// measured number that can drift: the block is icon 24 + `space::ICON` 6 + title 20 +
    /// `space::XS` 4 + the §4.13 action 28, and the body is `DOCK_MIN` − `DOCK_TABS` −
    /// `DOCK_TOOLBAR`. The Logs tab loses a third band to the failure banner and has 58px for a
    /// 50px block.
    #[gpui_kit::test]
    fn compact_log_empty_state_fits_without_clipping(cx: &mut TestAppContext) {
        init_app(cx);
        // Built by hand rather than through `add_dock_window`, which needs both a width and a
        // height and would give the Dock the 400px compact width instead of the whole window. A
        // capped description is a narrower constraint in a narrow body, so the wide case is the
        // one that can overflow sideways.
        let (harness, cx) = cx.add_window_view(|_, cx| {
            let dock = cx.new(DockPanel::new);
            CompactDockHarness {
                dock,
                width: None,
                height: design::size::DOCK_MIN,
            }
        });
        let panel = harness.read_with(cx, |harness, _| harness.dock.clone());
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(
                Some(fake_services(
                    Rc::new(RefCell::new(FakeTerminals::default())),
                    Rc::new(RefCell::new(FakeForwards::default())),
                )),
                cx,
            );
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        // Every row of the state has to sit inside the state.
        fn rows_stay_inside(cx: &mut VisualTestContext, rows: &[&'static str], what: &str) {
            let state = cx
                .debug_bounds("empty-state")
                .unwrap_or_else(|| panic!("{what} is laid out"));
            assert!(
                f32::from(state.size.width) > 0.0,
                "{what} has width: {state:?}"
            );
            for row in rows {
                let bounds = cx
                    .debug_bounds(row)
                    .unwrap_or_else(|| panic!("{what} lays out {row}"));
                assert!(
                    f32::from(bounds.size.width) > 0.0 && f32::from(bounds.size.height) > 0.0,
                    "{what}/{row} is laid out with no box: {bounds:?}"
                );
                assert!(
                    bounds.origin.y >= state.origin.y,
                    "{what}/{row} starts above the state: {bounds:?} in {state:?}"
                );
                assert!(
                    bounds.bottom() <= state.bottom(),
                    "{what}/{row} escapes the state: {bounds:?} in {state:?}"
                );
            }
        }

        // The Logs tab: title only, and no dead description row to overflow with.
        assert!(
            cx.debug_bounds("empty-state-hint").is_none(),
            "§4.13: the 'really none' state has no 说明, so there is no empty row under the title"
        );
        let log_body = f32::from(
            design::size::DOCK_MIN
                - design::size::DOCK_TABS
                - design::size::DOCK_TOOLBAR
                - design::size::DOCK_TOOLBAR,
        );
        let log_block = f32::from(design::size::ICON_LARGE)
            + f32::from(space::ICON)
            + f32::from(design::text::TITLE_LINE_HEIGHT);
        assert!(
            log_block <= log_body,
            "the Logs block is {log_block}px in a {log_body}px body: the banner row is in this \
             tab's chrome, so the body is the smallest in the Dock"
        );
        rows_stay_inside(cx, &["empty-state-title"], "the Logs empty state");

        // The Terminal tab: a title and the action, and no 说明 — §4.13's default, and the only
        // shape that fits the 86px this tab has at the Dock's floor.
        panel.update(cx, |panel, cx| panel.show_terminal_tab(cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-state-hint").is_none(),
            "the Terminal state says the same thing with its button and with the band under it"
        );
        let terminal_body = f32::from(
            design::size::DOCK_MIN - design::size::DOCK_TABS - design::size::DOCK_TOOLBAR,
        );
        let terminal_block = f32::from(design::size::ICON_LARGE)
            + f32::from(space::ICON)
            + f32::from(design::text::TITLE_LINE_HEIGHT)
            + f32::from(space::XS)
            + f32::from(design::size::CONTROL);
        assert!(
            terminal_block <= terminal_body,
            "the Terminal block is {terminal_block}px in a {terminal_body}px body: this tab has no \
             failure banner, so it has 28px more than the Logs tab and no more"
        );
        rows_stay_inside(
            cx,
            &["empty-state-title", "terminal-add"],
            "the Terminal empty state",
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
    #[gpui_kit::test]
    fn the_dock_close_chord_follows_the_focused_surface(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
        let dock = KeyContext::parse(DOCK_CLOSE_CONTEXT).expect("a plain context name");
        let terminal = KeyContext::parse(DOCK_TERMINAL_CONTEXT).expect("a plain context name");

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
        // `UI-SPEC` §16.3 draws the level as a word in a fixed column, so the column has to hold
        // the widest word the parser can produce or a `CRITICAL` line runs past the row's measured
        // width and the no-wrap list scrolls a row wider than the content it draws.
        assert_eq!(
            level_column_width(&typography),
            typography.columns(LOG_SEVERITY_COLUMNS as f32),
            "the level column holds the widest level word, whatever this line carries"
        );
        let fixed = log_row_fixed_width(timestamp_column_width(reserve, &typography), &typography);
        let copy = design::size::CONTROL + space::SM;
        assert_eq!(
            log_row_width(&plain, reserve, &typography),
            fixed + typography.columns(plain.message_columns() as f32) + copy,
            "a short line reserves the copy control too, so it stays copyable"
        );
        assert_eq!(
            log_row_width(&long, reserve, &typography),
            fixed + typography.columns(long.message_columns() as f32) + copy
        );
        assert!(
            log_row_width(&critical, reserve, &typography)
                > log_row_width(&plain, reserve, &typography)
        );
    }

    /// A row reserves the same columns however wide the Dock is.
    ///
    /// It used to reserve fewer: a compact row dropped the level word and kept a dot, so the same
    /// stream read two different ways in two windows and the level of every line on screen
    /// depended on the width of the frame. `UI-SPEC` §16.3 makes the level the column that never
    /// gives way, so the reservation is now unconditional and this holds it there.
    #[test]
    fn every_row_reserves_the_columns_it_draws() {
        let stamp = LogLine::parse("2026-09-22T21:14:02.331Z INFO ready");
        let plain = LogLine::parse("plain");
        let reserve = LOG_TIMESTAMP_COLUMNS;
        let typography = log_typography(
            crate::settings::PRODUCT_DATA_FONT_SIZE,
            crate::settings::PRODUCT_DATA_LINE_HEIGHT * crate::settings::PRODUCT_DATA_FONT_SIZE,
        );
        let fixed = log_row_fixed_width(timestamp_column_width(reserve, &typography), &typography);
        let copy = design::size::CONTROL + space::SM;
        assert_eq!(
            log_row_width(&plain, reserve, &typography),
            fixed + typography.columns(plain.message_columns() as f32) + copy,
            "a row reserves the timestamp and the level before the message"
        );
        assert_eq!(
            log_row_width(&stamp, reserve, &typography),
            fixed + typography.columns(stamp.message_columns() as f32) + copy,
            "a stamped row adds exactly the timestamp column"
        );
        let no_stamp_reserve = 0;
        assert_eq!(
            log_row_width(&plain, no_stamp_reserve, &typography),
            log_row_fixed_width(px(0.), &typography)
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
            log_row_width(&long, reserve, &typography)
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
            log_row_width(&plain, reserve, &large) > log_row_width(&plain, reserve, &default),
            "the whole row has to grow: the uniform list scrolls the width the row reserves"
        );
    }

    #[test]
    fn tab_panels_are_named_after_the_active_tab() {
        assert_eq!(tab_panel_label(DockTab::Logs(0), 0).as_ref(), "Logs");
        assert_eq!(
            tab_panel_label(DockTab::Logs(1), 0).as_ref(),
            "Logs",
            "a second stream is the same panel, so the panel is named the same way"
        );
        assert_eq!(tab_panel_label(DockTab::Terminal, 0).as_ref(), "Terminal");
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
            tab_panel_label(DockTab::Terminal, 3).as_ref(),
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
        let chord = |key: &str, modifiers: Modifiers| Keystroke {
            key: key.to_owned(),
            key_char: None,
            modifiers,
        };
        assert!(is_log_copy_chord(&chord("c", Modifiers::control())));
        // The keysym carries whatever case the layout and CapsLock produce, so the chord is
        // matched on the key alone.
        assert!(is_log_copy_chord(&chord("C", Modifiers::control())));
        for modifiers in [
            Modifiers::default(),
            Modifiers::control_shift(),
            Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
            Modifiers {
                control: true,
                platform: true,
                ..Default::default()
            },
        ] {
            assert!(!is_log_copy_chord(&chord("c", modifiers)), "{modifiers:?}");
        }
        assert!(!is_log_copy_chord(&chord("v", Modifiers::control())));
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
    #[gpui_kit::test]
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
        assert!(
            (f32::from(first_tab.origin.x) - f32::from(tabs.origin.x) - f32::from(space::SM)).abs()
                <= 1.0,
            "the first tab starts at the strip's own inset. `docs/mockup/secondary.html` draws \
             `.dtabs` with `padding: 0 8px`; the strip used to hand its left edge to the first \
             tab because the component's bar had no inset of its own."
        );
        assert!(
            (f32::from(toolbar.size.height) - f32::from(design::size::DOCK_TOOLBAR)).abs() <= 1.0
        );
        assert!(toolbar.origin.y >= tabs.bottom());
        assert!(row.origin.x >= tabs.origin.x);
        let row_height = cx.update(|_, cx| log_row_height(cx));
        assert!(row.size.height >= row_height);
        // A narrow Dock wraps the message; it does not lay it out at its own content width and let
        // the rest of the line run off the row. The message used to be measured at 4320px inside a
        // 400px row, so a long line's tail was simply not on screen, and "wrap long lines" was on.
        assert!(
            message.size.width <= row.size.width && message.origin.x < row.right(),
            "the message is drawn inside its row: row {row:?}, message {message:?}"
        );
        let typography = cx.update(|_, cx| crate::settings::data_typography(cx));
        assert!(
            message.size.width >= log_columns(&typography, 8) - px(1.),
            "and it still has room to read: {}px of message in a {}px row",
            f32::from(message.size.width),
            f32::from(row.size.width)
        );
        assert!(cx.debug_bounds("dock-log-filter").is_none());
    }

    /// The row's height is the reader's data line, and nothing on the row may decide otherwise.
    ///
    /// The per-row copy control used to be `design::size::CONTROL` — 28px — in the wrapped list, and
    /// the tallest child decides a row's height, so a control the reader reaches for on one row at
    /// a time was setting the pitch of all ten thousand. This is the invariant that catches it: the
    /// wrapped row and the no-wrap row are both the configured data line, so the same buffer reads
    /// at one density in both modes and the ring cap buys the lines the reader asked for.
    #[gpui_kit::test]
    fn the_wrapped_row_is_the_data_line_and_not_its_tallest_child(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![LogEvent::Line(
                "2026-09-22T21:14:02.331Z INFO ready".to_owned(),
            )],
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let expected = cx.update(|_, cx| log_row_height(cx));

        let wrapped = f32::from(
            cx.debug_bounds("dock-log-row")
                .expect("a wrapped log row is laid out")
                .size
                .height,
        );
        panel.update(cx, |panel, cx| panel.toggle_wrap(cx));
        cx.run_until_parked();
        let nowrap = f32::from(
            cx.debug_bounds("dock-log-row")
                .expect("a no-wrap log row is laid out")
                .size
                .height,
        );

        assert!(
            (wrapped - f32::from(expected)).abs() <= 1.0,
            "the wrapped row drew {wrapped}px against a {expected}px data line; a control on the \
             row is taller than the text it sits on"
        );
        assert!(
            (nowrap - wrapped).abs() <= 1.0,
            "one buffer read at {wrapped}px wrapped and {nowrap}px not wrapped is two densities \
             for one stream, decided by a display toggle"
        );
    }

    /// The log row is the data line the reader configured.
    ///
    /// `LOG_ROW_HEIGHT` was a `const` reading `design::text::MONO_SM_LINE_HEIGHT`, and a `const`
    /// cannot read a runtime setting: raising "Data font size" made the glyphs taller and left the
    /// row, the uniform list's item height and the scroll arithmetic at 18px, which is a taller
    /// glyph in a shorter box — the case the setting's own help text says cannot happen. The row
    /// is read at the configured size here, with the setting moved through the store so no test
    /// writes the reader's settings file.
    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
            + log_row_fixed_width(timestamp_column_width(reserve, &typography), &typography)
            - space::SM;

        assert!((f32::from(timestamp.origin.y) - f32::from(row.origin.y)).abs() <= 1.0);
        assert!((f32::from(severity.origin.y) - f32::from(row.origin.y)).abs() <= 1.0);
        assert!((f32::from(timestamp.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(severity.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(message.origin.x) - f32::from(message_origin)).abs() <= 1.0);
        assert!(f32::from(message.size.height) >= f32::from(row_height) * 2.0 - 1.0);
    }

    #[gpui_kit::test]
    /// Mouse and trackpad scrolling must move the log list and report follow state, and must not
    /// read the list state that the list already borrowed for its own scroll event.
    #[gpui_kit::test]
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
            cx.simulate_resize(REAL_WINDOW);
            cx.run_until_parked();
            assert!(
                panel.read_with(cx, |panel, _| panel.is_following()),
                "wrap={wrap}: a fresh stream follows the tail"
            );
            // The wheel goes to the scrollable surface, not to a row. A list that is following
            // its tail puts its first laid-out row against the bottom edge of the window, and a
            // wheel event aimed there is outside the frame: it reaches nothing, the list does not
            // move, and the assertion below fails for a reason that has nothing to do with
            // scrolling.
            let list = cx
                .debug_bounds(if wrap {
                    "dock-log-scroll"
                } else {
                    "dock-log-nowrap-scroll"
                })
                .expect("the log list is laid out");

            for _ in 0..5 {
                cx.simulate_event(ScrollWheelEvent {
                    position: list.center(),
                    delta: ScrollDelta::Pixels(point(px(0.), px(120.))),
                    modifiers: Modifiers::none(),
                    touch_phase: TouchPhase::Moved,
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
    #[gpui_kit::test]
    fn follow_reads_the_active_scroll_owner(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..400)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
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
            let half = px(panel
                .log()
                .scroll_handle
                .0
                .borrow()
                .base_handle
                .max_offset()
                .y
                / px(2.));
            assert!(half > px(0.), "the uniform list has room to scroll");
            panel
                .log_mut()
                .scroll_handle
                .0
                .borrow_mut()
                .base_handle
                .set_offset(point(px(0.), -half));
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
    #[gpui_kit::test]
    fn the_list_scroll_handler_records_state_without_a_panel(cx: &mut TestAppContext) {
        init_app(cx);
        let (harness, cx) = cx.add_window_view(|_, _| ScrollReportHarness::new());
        let report = harness.read_with(cx, |harness, _| Rc::clone(&harness.report));
        assert_eq!(report.get(), None, "the list starts with nothing to report");

        cx.simulate_event(ScrollWheelEvent {
            position: point(px(200.), px(150.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(120.))),
            modifiers: Modifiers::none(),
            touch_phase: TouchPhase::Moved,
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
    #[gpui_kit::test]
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
                !panel.log().wrap,
                "the uniform list is the scroll owner under test"
            );
            assert!(panel.log().list_follow_report.get().is_none());
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
    #[gpui_kit::test]
    fn a_failed_log_stream_keeps_the_navigation_keys(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            panel.log_mut().phase = LogPhase::Failed {
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
                panel.log().log_selection.is_none(),
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
    #[gpui_kit::test]
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
                    let Some(selection) = panel.log().log_selection else {
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
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    /// `UI-SPEC` §16.1 is the Dock's one rule: the Dock holds streams, the centre holds lists. A
    /// port forward is a list — §14.6 rules out a resident panel for it outright — and it already
    /// has one, in the status bar, built from the very snapshots this panel publishes. So the Dock
    /// must not draw a second copy, and the model behind it must keep working, because that panel
    /// is the only place a forward can be stopped.
    #[gpui_kit::test]
    fn the_dock_holds_no_forward_list_and_still_publishes_the_model(cx: &mut TestAppContext) {
        let (panel, _terminals, _forwards, cx) = setup_terminals(cx);
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
                .expect("start forward");
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("dock-forwards-list").is_none(),
            "a list in the Dock breaks §16.1, and the status bar already draws this one"
        );
        assert!(
            cx.debug_bounds("terminal-toolbar").is_some(),
            "the session controls stay: only the list left"
        );

        panel.read_with(cx, |panel, _| {
            let snapshots = panel.forward_snapshots();
            assert_eq!(snapshots.len(), 1, "the status bar reads this row");
            assert_eq!(
                snapshots[0].phase,
                ForwardPhase::Running,
                "a forward the Dock started is stoppable from the panel that lists it"
            );
            let summary = panel.forward_summary();
            assert_eq!(summary.active, 1);
        });
    }

    /// A failed forward drops the port it used to bind, in the model as well as in the row.
    #[gpui_kit::test]
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
        });
    }

    /// The Tail menu, Load More History, and the cap notice must offer the same ladder.
    #[gpui_kit::test]
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
    /// spends a screen of empty space on columns the row does not have.
    #[gpui_kit::test]
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

        let expected = panel.read_with(cx, |panel, cx| {
            let line = panel.log().buffer.line(1).expect("line");
            let reserve = panel.log().buffer.timestamp_column_reserve();
            let typography = crate::settings::data_typography(cx);
            log_row_width(&line, reserve, &typography)
        });
        let drawn = cx.debug_bounds("dock-log-row").expect("nowrap row");
        assert!(
            (f32::from(drawn.size.width) - f32::from(expected)).abs() <= 1.0,
            "the row drew {} but reserved {expected}",
            drawn.size.width
        );
        // The copy control fills the line and uses the width the row reserves for it.
        let copy = cx.debug_bounds("dock-log-copy").expect("copy control");
        let row_height = cx.update(|_, cx| log_row_height(cx));
        assert!((f32::from(copy.size.height) - f32::from(row_height)).abs() <= 1.0);
        assert!((f32::from(copy.size.width) - f32::from(design::size::CONTROL)).abs() <= 1.0);
    }

    #[gpui_kit::test]
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
        let scroll = panel.read_with(cx, |panel, _| panel.log().scroll_handle.clone());
        assert_eq!(scroll.is_scrolled_to_end(), Some(true));
        assert!(panel.read_with(cx, |panel, _| panel.is_following()));

        scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
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

    #[gpui_kit::test]
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
            panel.read_with(cx, |panel, _| panel
                .log()
                .list_state
                .logical_scroll_top()
                .item_ix),
            0
        );
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        assert!(panel.read_with(
            cx,
            |panel, _| panel.log().list_state.logical_scroll_top().item_ix > 0
        ));
    }

    #[gpui_kit::test]
    /// The strip's own keyboard: the arrows move the selection and move the focus with it.
    ///
    /// The Terminal tab's focus handle used to be asserted at index 1 because the strip was two
    /// fixed tabs. `UI-SPEC` §16.3's cap puts a second `Logs` tab between `Logs` and `Terminal`, so
    /// the Terminal handle is the *last* slot rather than the second one, and the assertion reads
    /// the slot off the tab instead of hard-coding an index. What is under test is unchanged: the
    /// key that moves the selection also moves the focus, so a keyboard reader's caret is always
    /// on the tab that is selected.
    fn tab_arrows_move_selection_and_focus(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        let handles = panel.read_with(cx, |panel, _| panel.tab_focus_handles.clone());
        let terminal = DockTab::Terminal.slot();
        cx.update(|window, cx| window.focus(&handles[0], cx));
        cx.simulate_keystrokes("right");
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab),
            DockTab::Terminal
        );
        assert!(cx.update(|window, _| handles[terminal].is_focused(window)));
    }

    #[gpui_kit::test]
    fn idle_log_target_does_not_show_a_status_chip(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| panel.log_mut().phase = LogPhase::Idle);
        assert!(!panel.read_with(cx, |panel, _| panel.should_show_log_status()));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log_status_label()),
            None
        );
        panel.update(cx, |panel, _| panel.log_mut().phase = LogPhase::Streaming);
        assert!(panel.read_with(cx, |panel, _| panel.should_show_log_status()));
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log_status_label()),
            Some("Live")
        );
    }

    /// The stream keeps running on the Terminal tab and behind a maximised pane, so a state that
    /// needs acting on must not be tied to the Logs tab.
    ///
    /// The state this test drives is `Failed`, not `Streaming`. It used to drive `Streaming` and
    /// assert a `Live` chip on the strip, which is the one assertion `UI-SPEC` §3.2's semantic
    /// inversion forbids: a healthy stream marked in `status.success` on the surface where the
    /// reader decides whether to trust the lines. A strip that says nothing while the stream is
    /// delivering is the inversion; a strip that says `Reconnecting` while a terminal is open is
    /// the coverage. Both are asserted here.
    #[gpui_kit::test]
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
            "the stream is still a thing the status bar reports while the Terminal tab is open"
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_none(),
            "`UI-SPEC` §3.2: a delivering stream is not a state the strip marks"
        );
        panel.update(cx, |panel, cx| {
            panel.log_mut().phase = LogPhase::Failed {
                reason: "stream closed".to_owned(),
            };
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-status").is_some(),
            "a stream that stopped is reported on the Terminal tab, where no other surface can"
        );

        panel.update(cx, |panel, cx| panel.toggle_terminal_maximized(cx));
        cx.run_until_parked();
        // `UI-SPEC` §16.2 (D30): the tab strip is permanent. A maximised terminal used to take it
        // away with it, which left a Dock with a live stream behind it and no way to get back to
        // the stream's own tab — the one state in which the strip is the only way out. Maximising
        // now hides the forwards strip, which is what it was for, and nothing else.
        assert!(
            cx.debug_bounds("dock-tabs-row").is_some(),
            "a maximised terminal keeps the tab strip: it is the only way to switch views"
        );
        assert!(
            cx.debug_bounds("dock-close").is_some(),
            "the close control belongs to the strip, and the strip stays"
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_some(),
            "the state stays on the strip while the session is maximised"
        );
    }

    /// "Following" was only an unselected button. Scrolling away now has a visible state.
    #[gpui_kit::test]
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

    /// The tab's name survives a long scope; the scope is what gives way.
    ///
    /// The strip draws `cluster/namespace/name:container` and it used to draw that as one label
    /// with a tail truncation under one cap, so the tail — the Pod name, the only part of the
    /// string that says *which* Pod — was the part that disappeared. On the product's own cluster
    /// that read `kind-k8s-gpui-3n/kube-system/co…`, and a reader with two log tabs open could
    /// not tell them apart. §2.3 asks for the *middle* of a name to shorten because a k8s hash is
    /// at its tail, and the scope is a fact the title bar prints 60px above.
    ///
    /// What does the work is the two independent caps, not the flex order: the strip is
    /// `flex_none` all the way down, so neither half can be pushed by the other and each stops at
    /// its own number. The shrink declarations are what keep that true if the row ever does get a
    /// bound, and the assertions below are about the numbers.
    #[gpui_kit::test]
    fn a_long_scope_shortens_the_scope_and_not_the_object_name(cx: &mut TestAppContext) {
        init_app(cx);
        fn tab_widths(
            cx: &mut TestAppContext,
            context: &str,
            namespace: &str,
            name: &str,
        ) -> (f32, f32, f32, f32) {
            let (panel, _state, cx) = setup_in_dock(cx, None, None);
            panel.update(cx, |panel, cx| {
                let mut services = fake_services(
                    Rc::new(RefCell::new(FakeTerminals::default())),
                    Rc::new(RefCell::new(FakeForwards::default())),
                );
                services.context = Some(context.into());
                panel.set_terminal_services(Some(services), cx);
                // Handing the Dock a cluster drops every stream that belonged to the one it was
                // on, so the harness's own stream is gone the moment the context is set and has
                // to be opened again for the strip to hold two.
                panel.open_logs(request(), cx);
                // The harness's own stream is closed afterwards, so the one left on the strip is
                // the only `Logs` tab and it is the first one whatever its slot number.
                panel.open_logs(
                    LogRequest {
                        namespace: Some(namespace.into()),
                        name: name.into(),
                        containers: vec!["coredns".into()],
                    },
                    cx,
                );
                panel.show_logs_slot(1, cx);
                panel.close_log_slot(0, cx);
            });
            cx.simulate_resize(REAL_WINDOW);
            cx.run_until_parked();
            let object = cx
                .debug_bounds("dock-tab-object-0")
                .expect("the tab names its object");
            let scope = cx
                .debug_bounds("dock-tab-scope-0")
                .expect("the tab names its scope");
            let detail = cx
                .debug_bounds("dock-tab-detail-0")
                .expect("the tab's detail row");
            (
                f32::from(object.size.width),
                f32::from(scope.size.width),
                f32::from(object.right()),
                f32::from(detail.right()),
            )
        }

        // `coredns-559f6c778d-jwghp` is the shape a generated name actually has: 26 characters of
        // `caption`, about 155px against a 160px cap, so it is drawn whole.
        const NAME: &str = "coredns-559f6c778d-jwghp";
        // The short scope has to be short enough to be drawn whole under `tab_scope_max_width`, or
        // the two cases are indistinguishable and the first assertion says nothing. Ten characters
        // of `caption` is about 60px against a 128px cap.
        let short = tab_widths(cx, "k", "default", NAME);
        let long = tab_widths(
            cx,
            "kind-dev",
            "team-platform-data-ingestion-staging-eu-west-1-that-will-not-fit",
            NAME,
        );

        assert_eq!(
            short.0, long.0,
            "the object's width is its own and not the scope's: {} against {}",
            short.0, long.0
        );
        assert!(
            short.1 < long.1 && short.1 < f32::from(tab_scope_max_width()),
            "the scope is the half that gives way and a short one is drawn whole: {} against {}",
            short.1,
            long.1
        );
        assert!(
            long.1 <= f32::from(tab_scope_max_width()) + 0.5,
            "and a long one stops at its own cap instead of pushing the row wider: {} against a \
             cap of {}",
            long.1,
            f32::from(tab_scope_max_width())
        );
        // The regression itself: with the scope at its cap and the product's own name shape, the
        // name is still whole. Under one label under one cap it was `co…`.
        assert!(
            long.0 >= 150.,
            "a name that fits its cap is drawn whole whatever the scope is doing: {}",
            long.0
        );
        assert!(
            long.0 <= f32::from(tab_name_max_width()) + 0.5,
            "and a name that does not fit is cut at its own cap, not at the scope's: {} against a \
             cap of {}",
            long.0,
            f32::from(tab_name_max_width())
        );
        assert!(
            long.2 <= long.3 + 0.5,
            "the name is drawn inside the row that holds it: {} against {}",
            long.2,
            long.3
        );
    }

    /// `UI-SPEC` §16.3's cap, and the three things it has to be true of at once.
    ///
    /// The cap is a statement about *connections*, so it is checked on connections: the second Pod
    /// keeps its subscription, keeps receiving, and keeps its own tab while the reader is on the
    /// first. The refusal is checked where §16.2 puts the only always-painted Dock surface, because
    /// a body folded to nothing is exactly the state a reader is in when they ask for a third
    /// stream, and a refusal that can only be seen in the body is a refusal they never see. And the
    /// way out of the refusal is checked, because a cap with no exit is a wall.
    #[gpui_kit::test]
    fn the_dock_holds_two_log_streams_and_refuses_the_third(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![LogEvent::Line(
                "2026-09-22T21:14:02.331Z INFO web ready".to_owned(),
            )],
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.open_log_streams()),
            1,
            "the target the harness opened is one stream"
        );

        panel.update(cx, |panel, cx| panel.open_logs(other_request(), cx));
        cx.run_until_parked();
        push(
            &state,
            vec![LogEvent::Line(
                "2026-09-22T21:14:03.000Z INFO api ready".to_owned(),
            )],
            cx,
        );
        cx.run_until_parked();
        let after_second = panel.read_with(cx, |panel, _| {
            (
                panel.open_log_streams(),
                panel.log().buffer.len(),
                panel
                    .log_slot(1)
                    .map(|stream| (stream.buffer.len(), stream.subscription.is_some())),
            )
        });
        assert_eq!(after_second.0, 2, "two Pods are two streams");
        assert_eq!(
            after_second.1, 1,
            "the stream in the body is the one just opened"
        );
        assert_eq!(
            after_second.2,
            Some((1, true)),
            "the stream that was in the body kept its lines and is still connected, because a \
             parked stream that stopped streaming would make the cap a lie"
        );

        // The strip names both, so a reader can tell which tab is which.
        let tabs = panel.read_with(cx, |panel, _| panel.tabs());
        assert_eq!(
            tabs,
            vec![DockTab::Logs(0), DockTab::Logs(1), DockTab::Terminal]
        );
        let (first, second) = panel.read_with(cx, |panel, _| {
            (
                panel.tab_detail(DockTab::Logs(0)),
                panel.tab_detail(DockTab::Logs(1)),
            )
        });
        assert!(
            first
                .as_ref()
                .is_some_and(|label| label.ends_with("web-0:app")),
            "the first tab still names the first Pod: {first:?}"
        );
        assert!(
            second
                .as_ref()
                .is_some_and(|label| label.ends_with("api-7d2f:app")),
            "and the second names the second, so selecting one does not renumber the other: \
             {second:?}"
        );

        // The body is folded away: §16.2's hardest case, and the one the cap has to answer in.
        panel.update(cx, |panel, cx| {
            panel.body_collapse_requested = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-content").is_none(),
            "the body is gone"
        );
        assert!(
            cx.debug_bounds("dock-tabs-row").is_some(),
            "the strip is the one Dock surface that is always on screen"
        );

        panel.update(cx, |panel, cx| panel.open_logs(third_request(), cx));
        cx.run_until_parked();
        let refused = panel.read_with(cx, |panel, _| {
            (
                panel.open_log_streams(),
                panel.log_cap_notice.clone(),
                panel
                    .log()
                    .request
                    .as_ref()
                    .map(|request| request.name.clone()),
            )
        });
        assert_eq!(
            refused.0, MAX_ACTIVE_LOG_STREAMS,
            "the third stream is refused"
        );
        assert_eq!(
            refused.2.as_deref(),
            Some("api-7d2f"),
            "the refusal leaves the stream the reader was reading alone"
        );
        let notice = refused
            .1
            .expect("the refusal is on the strip, not only in a toast");
        assert!(
            notice.0.contains("close one"),
            "the refusal has to name the way out of it, in the words §16.3 asks for: {:?}",
            notice.0
        );
        assert!(
            notice.1.contains("web-0") && notice.1.contains("api-7d2f"),
            "the detail names both streams the reader has to choose between: {:?}",
            notice.1
        );
        assert!(
            cx.debug_bounds("dock-log-status").is_some(),
            "the strip is painted with the refusal while the body is folded away"
        );

        // A filter set on one stream follows that stream and not the other, and a stream that is
        // selected with a filter already on it is scored, not shown empty.
        panel.update(cx, |panel, cx| panel.set_log_filter("web", cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log().log_filter.clone()),
            "web".to_owned(),
            "the filter the reader typed is on the stream they typed it on"
        );
        // The stream in the body is the one just opened, so the *other* tab is the first one.
        let other_tab = cx
            .debug_bounds("dock-tab-0")
            .expect("the first stream has a tab");
        cx.simulate_click(other_tab.center(), Modifiers::none());
        cx.run_until_parked();
        cx.run_until_parked();
        let (filter, visible, retained) = panel.read_with(cx, |panel, _| {
            (
                panel.log().log_filter.clone(),
                panel.visible_log_count(),
                panel.log().buffer.len(),
            )
        });
        assert_eq!(
            filter, "",
            "and the other stream has no filter, because it had none"
        );
        assert_eq!(
            visible, retained,
            "and every one of its lines is visible, rather than an empty pane under a filter the \
             reader cannot see"
        );

        // And the way out: closing the stream the reader is not looking at frees the slot.
        panel.update(cx, |panel, cx| {
            panel.body_collapse_requested = false;
            panel.close_log_slot(0, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.open_log_streams()),
            1,
            "closing a tab is what makes room for another stream"
        );
        panel.update(cx, |panel, cx| panel.open_logs(third_request(), cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| {
                panel
                    .log()
                    .request
                    .as_ref()
                    .map(|request| request.name.to_string())
            }),
            Some("cache-0".to_owned()),
            "after closing one, the third opens"
        );
        assert!(
            panel.read_with(cx, |panel, _| panel.log_cap_notice.is_none()),
            "a resolved refusal does not stay on the strip"
        );
    }

    /// Asking for a stream the Dock already holds selects its tab instead of restarting it.
    ///
    /// `OpenLogs` is a chord a reader presses on the selection they are looking at, and a table row
    /// stays selected while they read. Restarting on every press would clear the buffer and the
    /// scroll position of the very stream they opened it for, so the second press would read as
    /// the app losing the lines.
    #[gpui_kit::test]
    fn asking_for_an_open_stream_selects_it_and_keeps_its_lines(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..8)
                .map(|index| {
                    LogEvent::Line(format!("2026-09-22T21:14:0{index}Z INFO web line {index}"))
                })
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.open_logs(other_request(), cx));
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log().buffer.len()),
            0,
            "the body is the stream that was just opened, and it has said nothing yet"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log_slot(0).map(|s| s.buffer.len())),
            Some(8),
            "and the stream that was in the body kept its eight lines"
        );

        // Come back to the first stream by clicking its tab, the way a reader does.
        let parked_tab = cx
            .debug_bounds("dock-tab-0")
            .expect("the first stream has a tab");
        cx.simulate_click(parked_tab.center(), Modifiers::none());
        cx.run_until_parked();
        let before = panel.read_with(cx, |panel, _| panel.log().buffer.len());
        assert_eq!(
            before, 8,
            "the click brought back the stream that had the lines"
        );

        // Now ask for it again the way the chord does, while the other one is on screen.
        panel.update(cx, |panel, cx| {
            panel.show_logs_slot(1, cx);
            panel.open_logs(request(), cx);
        });
        cx.run_until_parked();
        let after = panel.read_with(cx, |panel, _| {
            (
                panel.active_tab,
                panel.log().buffer.len(),
                panel.open_log_streams(),
            )
        });
        assert_eq!(
            after.0,
            DockTab::Logs(0),
            "the request selected the tab that had it"
        );
        assert_eq!(after.1, before, "and did not throw the reader's lines away");
        assert_eq!(
            after.2, 2,
            "and did not spend a second slot on a stream that was already open"
        );
    }

    /// The strip is the only way out of a collapsed Dock, so it cannot be conditional.
    ///
    /// `UI-SPEC` §11.3 folds the body away below `design::size::DOCK_COLLAPSE_BELOW` and §16.2
    /// keeps the strip; `⌃` folds it on request at any height. Both have to leave the strip and
    /// both controls on screen, and this holds the two rules together because either one alone is
    /// the shape the other one is there to prevent.
    #[gpui_kit::test]
    fn a_short_window_leaves_only_the_tab_strip(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..50)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        cx.simulate_resize(SHORT_WINDOW);
        cx.run_until_parked();
        assert!(
            panel.read_with(cx, |panel, _| panel.log_list_has_lines()),
            "the stream is still running, the body is just not showing it"
        );
        assert!(
            cx.debug_bounds("dock-tabs-row").is_some(),
            "the tab strip is the Dock's one permanent row"
        );
        assert!(cx.debug_bounds("dock-collapse").is_some());
        assert!(cx.debug_bounds("dock-close").is_some());
        assert!(
            cx.debug_bounds("dock-content").is_none(),
            "a Dock below the height breakpoint has a strip and nothing under it"
        );

        // The same window, with the body the reader is entitled to: it comes back, and nothing
        // about the strip changed.
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-content").is_some());
        assert!(cx.debug_bounds("dock-tabs-row").is_some());
    }

    /// `⌃` folds the body and `×` hides the Dock, and a reader has to be able to tell them apart
    /// without reading two tooltips. They are two controls, two names and two shapes on the same
    /// row; this holds that the control exists, that it moves the body, and that the strip and
    /// the close control are untouched by it.
    #[gpui_kit::test]
    fn the_collapse_control_folds_the_body_and_keeps_the_strip(cx: &mut TestAppContext) {
        let (_panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("ready".into())], cx);
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let control = cx.debug_bounds("dock-collapse").expect("collapse control");
        assert!(
            f32::from(control.size.height) <= f32::from(design::size::DOCK_TABS),
            "the control and the ring it reserves have to fit the strip, or they overflow it: \
             {control:?}"
        );
        assert!(cx.debug_bounds("dock-content").is_some());

        let collapse = cx.debug_bounds("dock-collapse").expect("collapse control");
        cx.simulate_click(collapse.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-content").is_none(),
            "the control folds the body"
        );
        assert!(
            cx.debug_bounds("dock-tabs-row").is_some(),
            "and leaves the strip, because the strip is the way back"
        );
        assert!(cx.debug_bounds("dock-close").is_some());

        cx.simulate_click(collapse.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-content").is_some());
    }

    /// The shell has to be able to fold and unfold the body without a click, and the answer it
    /// gets has to be the same answer the `⌃` control gets.
    ///
    /// `UI-SPEC` §16.2 keeps the 28px strip resident precisely so the Dock can be open with a
    /// collapsed body — which is what the shell wants on launch, where a 200px band saying
    /// "nothing here" is a fifth of the window spent on an absence. `set_body_collapsed` is the
    /// only channel for that, and it is deliberately a *request* like the control's: if it answered
    /// for itself then a shell that unfolded the Dock in a 700px window would get a body the
    /// `⌃` control is still describing as folded, and the second half of this test is what stops
    /// that from being an invisible regression.
    #[gpui_kit::test]
    fn the_shell_can_fold_the_body_and_the_height_rule_still_has_the_last_word(
        cx: &mut TestAppContext,
    ) {
        init_app(cx);
        let (panel, cx) = add_dock_window(cx, Some(COMPACT_DOCK_WIDTH), Some(REAL_DOCK_HEIGHT));
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-content").is_some(),
            "a Dock with nothing asked for still shows its body until something folds it"
        );

        // The launch case: the strip is on screen and the body is not.
        panel.update(cx, |panel, cx| panel.set_body_collapsed(true, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-content").is_none());
        assert!(
            cx.debug_bounds("dock-tabs-row").is_some(),
            "§16.2: the strip stays, because the strip is the only way back"
        );
        assert!(cx.debug_bounds("dock-close").is_some());

        // And the control can still unfold it, so the shell has not taken the gesture away.
        let collapse = cx.debug_bounds("dock-collapse").expect("collapse control");
        cx.simulate_click(collapse.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-content").is_some());

        // Below the breakpoint the window is the answer, whoever asked.
        panel.update(cx, |panel, cx| panel.set_body_collapsed(true, cx));
        cx.simulate_resize(SHORT_WINDOW);
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.set_body_collapsed(false, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-content").is_none(),
            "§11.3: an unfold request cannot overrule the height rule, so the `⌃` control is not \
             promising something it will not do"
        );
    }

    /// `⌘F` finds, `Enter` steps, and `Escape` leaves.
    ///
    /// A find that cannot be reached from the keyboard is a filter, and a find that cannot be
    /// left is a trap — `UI-SPEC` §9.4 requires Escape to always do something. The bar also has to
    /// be find and not filter: a query that finds nothing must not empty the pane, or the reader
    /// loses the lines they were reading to find the line in.
    #[gpui_kit::test]
    fn find_highlights_hits_without_hiding_the_lines_around_them(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..40)
                .map(|index| {
                    LogEvent::Line(format!(
                        "2026-09-23T10:00:{index:02}Z INFO handler-{index:02} ready"
                    ))
                })
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&focus, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-log-find").is_none());

        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-find").is_some(),
            "⌘F opens the find bar"
        );
        assert!(
            cx.debug_bounds("dock-log-filter").is_none(),
            "find takes the field's slot rather than adding a second one"
        );
        let find_field = cx
            .debug_bounds("shared-text-input")
            .expect("the find field is laid out");
        cx.simulate_click(find_field.center(), Modifiers::none());
        cx.simulate_input("handler-07");
        cx.run_until_parked();
        let count = panel.read_with(cx, |panel, _| {
            (panel.log().find_hits.len(), panel.log().find_rows.len())
        });
        assert_eq!(count, (1, 1), "one line carries the query");
        assert!(
            panel.read_with(cx, |panel, _| panel.log().buffer.len()) == 40,
            "a find that matched one line out of forty hid the other thirty-nine"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel
                .find_highlight()
                .map(|find| find.rows.len())),
            Some(1),
            "the row the list can draw is highlighted"
        );

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-log-find").is_none());
        assert!(panel.read_with(cx, |panel, _| panel.log().find_query.is_empty()));
        assert!(
            cx.debug_bounds("dock-log-row").is_some(),
            "Escape hands the pane back, it does not take the pane away"
        );
    }

    /// `⌘F` puts the caret in the field it opened, and one Escape closes it and hands the keyboard
    /// back to the rows.
    ///
    /// This is the invariant that broke, and it broke in the worst way: the find bar opened, the
    /// focus went somewhere else, typing did nothing, Escape did not close it, and after closing it
    /// `⌘F` never opened it again. The cause was outside this file — `secondary-f` was bound to a
    /// global action in the keymap, so the panel's own handler and the app's dispatch were both
    /// claiming one key — and nothing here tested what the bar *does* to the focus, so the fix to
    /// the keymap could have left it broken. `UI-SPEC` §9.4 requires Escape to always do something;
    /// a bar that swallows a keystroke the field cannot see is a bar the keyboard cannot get out
    /// of, and a bar that opens without the caret is a bar that cannot be typed into.
    #[gpui_kit::test]
    fn find_takes_the_caret_and_one_escape_gives_the_rows_back(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..12)
                .map(|index| {
                    LogEvent::Line(format!("2026-09-23T10:00:{index:02}Z INFO handler ready"))
                })
                .collect(),
            cx,
        );
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let list_focus = panel.read_with(cx, |panel, _| panel.focus_handle());
        cx.update(|window, cx| window.focus(&list_focus, cx));
        cx.run_until_parked();

        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-find").is_some(),
            "⌘F opens the find bar"
        );
        // Read the field's handle now that the field has been laid out. `TextInput` builds its
        // focus handle lazily, on the first frame that has a window, so a handle read before the
        // bar was ever opened is the builder's and is not the one the caret is in.
        let find_focus = cx.update(|_, cx| {
            panel.read_with(cx, |panel, cx| panel.find_input.read(cx).focus_handle(cx))
        });
        assert!(
            cx.update(|window, _| find_focus.is_focused(window)),
            "the field the bar just opened holds the caret; a bar that opens without it cannot be \
             typed into, and the reader's only sign is that letters do nothing"
        );
        // Typing has to land in the field, which is the half of the bug that looked like a dead
        // keyboard: the bar was on screen and the query stayed empty.
        cx.simulate_input("handler");
        cx.run_until_parked();
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log().find_query.clone()),
            "handler".to_owned(),
            "the field the caret was in is the field that reports the query"
        );
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.log().find_hits.len()),
            12,
            "every line matched, so the counter is a count and not a zero"
        );

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-find").is_none(),
            "one Escape closes the bar; a bar whose Escape only empties the field has spent the \
             key on half the answer"
        );
        assert!(
            cx.update(|window, _| list_focus.is_focused(window)),
            "the keyboard goes back to the rows the reader was reading"
        );
        assert!(
            cx.debug_bounds("dock-log-row").is_some(),
            "and the rows are still there"
        );

        // And it can be opened again, which is the third way the first bug presented.
        cx.simulate_keystrokes("cmd-f");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dock-log-find").is_some(),
            "⌘F opens the bar a second time"
        );
        let reopened = cx.update(|_, cx| {
            panel.read_with(cx, |panel, cx| panel.find_input.read(cx).focus_handle(cx))
        });
        assert!(cx.update(|window, _| reopened.is_focused(window)));
    }

    /// Every level role the log body draws clears its floor in both appearances.
    ///
    /// This is the check that catches what looking at a screenshot does not: a theme can put a
    /// `tertiary` four steps below the surface it is read on, and a dark screenshot will not show
    /// it because everything is dark. The Dock is where that matters most — the level is the one
    /// column a reader scans, and a timestamp nobody can read is a timestamp nobody is using.
    ///
    /// The levels are held to the *graphic* floor, not the text floor, and the reasoning is on the
    /// record in the design layer: a level word is read as a mark first and as a word second, and
    /// three-to-one is what that first read needs. `fg_disabled` is deliberately absent — §16.3
    /// asks for it and it is not contrast-solved yet, so the Dock uses `fg_tertiary` and reports
    /// it. When `fg_disabled` is solved this is the line that moves.
    #[gpui_kit::test]
    fn every_log_level_role_clears_its_floor_in_both_appearances(cx: &mut TestAppContext) {
        for appearance in [design::Appearance::Light, design::Appearance::Dark] {
            init_app(cx);
            let (_panel, _state, cx) = setup(cx);
            cx.update(|_, cx| design::set_appearance(cx, appearance));
            let measured: [(&str, f32); 6] = cx.update(|_, cx| {
                let surface = design::role::surface_content(cx);
                let contrast = |ink: Hsla| {
                    design::calculate_contrast_ratio(
                        design::composite_surface(surface, ink),
                        surface,
                    )
                };
                [
                    ("ERROR", contrast(log_level_role(Severity::Error, cx))),
                    ("WARN", contrast(log_level_role(Severity::Warning, cx))),
                    ("INFO", contrast(log_level_role(Severity::Neutral, cx))),
                    ("DEBUG", contrast(log_level_role(Severity::Muted, cx))),
                    ("timestamp", contrast(log_timestamp_role(cx))),
                    ("paused note", contrast(design::role::fg_tertiary(cx))),
                ]
            });
            for (name, ratio) in measured {
                assert!(
                    ratio >= design::MARKER_MIN_CONTRAST,
                    "{appearance:?}: the {name} role measures {ratio:.2}:1 on the Dock's content \
                     surface, under the {floor}:1 floor a scanned mark needs",
                    floor = design::MARKER_MIN_CONTRAST,
                );
            }
        }
    }

    /// A stream that stopped has to say when it last spoke.
    ///
    /// A pane of frozen lines and a pane of quiet lines are the same pixels, and the reader has no
    /// other way to tell them apart. Without the age of the last line, a reader watching a pod
    /// after a rollout has no way to know that the thing they are reading stopped two minutes ago.
    #[gpui_kit::test]
    fn a_stopped_stream_says_when_it_last_delivered_a_line(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            (0..10)
                .map(|index| LogEvent::Line(format!("line {index}")))
                .collect(),
            cx,
        );
        panel.update(cx, |panel, _| {
            panel.log_mut().phase = LogPhase::Failed {
                reason: "connection reset by peer".to_owned(),
            };
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        let note = cx
            .debug_bounds("dock-log-last-line")
            .expect("the stopped stream reports the age of its last line");
        assert!(f32::from(note.size.width) > 0.0);
        assert!(cx.debug_bounds("dock-log-banner").is_some());

        // Nothing has arrived, so the note is the only thing on the row that says the pane is not
        // quiet — and it goes when the stream is delivering again.
        panel.update(cx, |panel, cx| {
            panel.log_mut().phase = LogPhase::Streaming;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("dock-log-last-line").is_none());
    }

    /// A short line has no per-line button to fall back on, so the keyboard path is the path.
    #[gpui_kit::test]
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
            let selection = panel.log().log_selection.expect("the list took the caret");
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
    #[gpui_kit::test]
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
            let selection = panel.log().log_selection.expect("caret");
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
    #[gpui_kit::test]
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
                panel.log().filter_cursor,
                FILTER_SCORING_BUDGET,
                "one pass scores one budget"
            );
            assert_eq!(
                panel.log().visible_log_indices.len(),
                FILTER_SCORING_BUDGET,
                "the first pass stops at its budget"
            );
        });
        // Drain the rest of the pass, so the next batch is the only work left to account for.
        panel.update(
            cx,
            |panel, cx| while panel.run_filter_pass(log_row_height(cx)) {},
        );
        let scored = panel.read_with(cx, |panel, _| panel.log().visible_log_indices.len());
        assert_eq!(scored, FILTER_SCORING_BUDGET + 500);
        push(
            &state,
            lines(FILTER_SCORING_BUDGET + 500, FILTER_SCORING_BUDGET + 600),
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.log().visible_log_indices.len() - scored,
                100,
                "the next batch costs only its own lines, a rescan would add every match twice"
            );
            assert_eq!(panel.log().filter_cursor, panel.log().buffer.len());
        });
    }

    /// A filter pass continues on the following frames until the buffer is scored.
    #[gpui_kit::test]
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
            assert_eq!(panel.log().filter_cursor, panel.log().buffer.len());
            assert_eq!(panel.visible_log_count(), panel.log().buffer.len());
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
    #[gpui_kit::test]
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
            panel.log_mut().filter_frames_left = 1;
            assert!(
                panel.continue_filter_pass(cx),
                "the first frame is inside budget"
            );
            assert!(
                !panel.continue_filter_pass(cx),
                "the pass must stop asking once its budget is spent"
            );
            assert!(
                panel.log().filter_pass_deferred,
                "the pass is throttled, not finished"
            );
            assert_eq!(
                panel.log().filter_cursor,
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
                panel.log().filter_cursor,
                FILTER_SCORING_BUDGET * 4 + 10,
                "the batch scored the lines that arrived"
            );
            assert_eq!(
                panel.log().visible_log_indices.len(),
                FILTER_SCORING_BUDGET * 4 + 10
            );
        });
        panel.update(cx, |panel, cx| {
            assert!(
                !panel.continue_filter_pass(cx),
                "the pass has nothing left to score"
            );
            assert!(
                !panel.log().filter_pass_deferred,
                "a finished pass is no longer throttled"
            );
            assert_eq!(
                panel.log().filter_frames_left,
                FILTER_PASS_FRAME_BUDGET,
                "the next pass starts on a full budget"
            );
        });
    }

    /// A filter over the whole retained history finishes without waiting for the log stream, so
    /// the frame budget is wide enough to cover the ring.
    #[gpui_kit::test]
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
                panel.log().filter_cursor,
                panel.log().buffer.len(),
                "the pass scored the ring inside its frame budget"
            );
            assert!(!panel.log().filter_pass_deferred);
        });
    }

    /// Opening a new target starts its own pass. The match set and the buffer are cleared, so the
    /// cursor has to be cleared with them: a cursor left at the end of the old buffer would skip
    /// the first lines of the new one.
    #[gpui_kit::test]
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
            assert_eq!(panel.log().filter_cursor, panel.log().buffer.len());
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
                panel.log().visible_log_indices.len(),
                200,
                "the new target scored its own lines, not the tail of the old one"
            );
        });
    }

    /// Ring eviction rebases the stored match set instead of losing it.
    #[gpui_kit::test]
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
            assert_eq!(panel.log().filter_cursor, panel.log().buffer.len());
            assert_eq!(panel.log().visible_log_indices.len(), RING_CAPACITY);
            assert_eq!(
                *panel.log().visible_log_indices.last().expect("last match"),
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
            assert_eq!(panel.log().buffer.len(), RING_CAPACITY);
            assert!(
                panel
                    .log()
                    .visible_log_indices
                    .iter()
                    .all(|index| *index < panel.log().buffer.len()),
                "every stored index still points at a buffered line"
            );
            assert!(
                panel
                    .log()
                    .visible_log_indices
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            );
            assert_eq!(
                panel.log().visible_log_indices.len(),
                RING_CAPACITY,
                "the rebased set keeps every match, the 30 that arrived included"
            );
            assert_eq!(
                *panel.log().visible_log_indices.last().expect("last match"),
                RING_CAPACITY - 1
            );
        });
    }

    /// Severity is parsed from the first token, so the level scope is the only way to ask for
    /// warnings or errors without typing their labels.
    #[gpui_kit::test]
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
            assert_eq!(panel.log().visible_log_indices, vec![2]);
        });
        panel.update(cx, |panel, cx| {
            panel.set_log_level(LogLevelScope::Warning, cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.log().visible_log_indices, vec![1, 2]);
        });
        panel.update(cx, |panel, cx| panel.set_log_level(LogLevelScope::All, cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.log().visible_log_indices.is_empty());
            assert_eq!(panel.visible_log_count(), 4);
        });
        // The scope and the free-text filter compose.
        panel.update(cx, |panel, cx| {
            panel.set_log_level(LogLevelScope::Warning, cx)
        });
        panel.update(cx, |panel, cx| panel.set_log_filter("upstream", cx));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.log().visible_log_indices, vec![1, 2]);
        });
    }

    /// The ring buffer drops the oldest lines silently. The toolbar now says so.
    #[gpui_kit::test]
    fn dropped_log_lines_are_reported(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("only line".into())], cx);
        assert_eq!(panel.read_with(cx, |panel, _| panel.log().dropped_lines), 0);
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
                panel.log().dropped_lines > 0,
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
    #[gpui_kit::test]
    fn a_squeezed_dock_says_instead_of_showing_half_a_row(cx: &mut TestAppContext) {
        init_app(cx);
        // `DOCK_MIN` is stated for the product default data font, while the
        // harness installs the upstream Base theme and its buffer font is larger.
        // So the two facts are checked separately: the token equals the
        // derivation at the product default, and the live derivation clears it at
        // whatever font is actually installed. Asserting the token against a live
        // number would compare 12px against 16px and call the disagreement a
        // layout bug.
        // `UI-SPEC` §16.2 fixed the Dock's two bands at 28px, so the floor no longer has to equal
        // this derivation — it has to *clear* it. Asserting the old equality would only re-state
        // the chrome's own sum; asserting the inequality holds the fact the token exists for,
        // which is that a Dock at its minimum still shows three log lines.
        // `UI-SPEC` §16.2 fixed both Dock bands at 28px, so the chrome is 88px where it was
        // 100px and the shell's floor token — 154px, derived against the 40px toolbar it used to
        // have — is now 12px more than the Dock needs. That surplus is safe and this is the
        // property that matters: the shell must never hand the Dock a box shorter than the Dock's
        // own chrome plus three log rows, or the strip and the toolbar are squeezed off the rows
        // they exist to frame. The assertion used to read the other way round.
        assert!(
            design::size::DOCK_MIN >= dock_chrome_height() + design::text::MONO_SM_LINE_HEIGHT * 3.,
            "the floor must clear the fixed chrome plus three product-default log rows: \
             {:?} against {:?}",
            design::size::DOCK_MIN,
            dock_chrome_height() + design::text::MONO_SM_LINE_HEIGHT * 3.,
        );
        let recommended = cx.update(|cx| dock_min_recommended(cx));
        assert!(
            recommended <= design::size::DOCK_MIN,
            "the shell's floor must not sit below what the Dock needs, or the chrome is clipped: \
             the Dock wants {recommended:?} and the floor is {:?}",
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

    #[gpui_kit::test]
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
            assert_eq!(panel.log().buffer.len(), 2);
            assert_eq!(panel.log().synced, 2);
            assert_eq!(
                panel.log().buffer.line(0).expect("line").label.as_ref(),
                "INFO"
            );
            assert_eq!(
                panel.log().buffer.line(1).expect("line").severity,
                Severity::Muted
            );
        });
    }

    #[gpui_kit::test]
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
        cx.simulate_click(filter_bounds.center(), Modifiers::none());
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
            assert_eq!(panel.log().log_filter, "warn");
            assert_eq!(panel.visible_log_count(), 1);
            assert_eq!(panel.log().visible_log_indices, vec![1]);
            assert_eq!(panel.log().buffer.len(), 2);
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
            assert_eq!(panel.log().visible_log_indices, vec![1, 3]);
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
            assert_eq!(panel.log().visible_log_indices, vec![1, 3, 4]);
            assert!(!panel.is_following());
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.log().log_filter.is_empty());
            assert_eq!(panel.visible_log_count(), 5);
            assert!(panel.log().visible_log_indices.is_empty());
        });
    }

    #[gpui_kit::test]
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

    /// A reason the API server has already ruled on gets no second request.
    ///
    /// The reconnect ladder used to run for every reason, so a deleted Pod sat in
    /// "Reconnecting, attempt 3" for half a minute — five identical requests against a server that
    /// has answered — and the reader had to wait out a wait that could not end in an answer. The
    /// transient failure still retries; the ruled-on one does not.
    #[gpui_kit::test]
    fn a_pod_the_server_no_longer_has_is_not_reconnected(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![LogEvent::Ended(
                "The log request failed: pods \"web-0\" not found. Check the Pod, cluster \
                 connection, and access permissions, then try again."
                    .into(),
            )],
            cx,
        );
        panel.read_with(cx, |panel, _| {
            assert!(
                matches!(panel.phase(), LogPhase::Failed { .. }),
                "a missing Pod is reported, not retried: {:?}",
                panel.phase()
            );
        });
        cx.executor()
            .advance_clock(RECONNECT_BASE * 8 + Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            state.borrow().restarts,
            1,
            "no request went out again against a Pod that is gone"
        );
    }

    /// A container that ran to completion is the answer, not a fault.
    ///
    /// A `--follow` stream closes when the process ends, which for a Job is what the reader opened
    /// the logs to see. It used to be classified as a failed request: a red dot, an alert icon, an
    /// `Error` verdict, and five reconnection attempts against a container that had exited zero.
    #[gpui_kit::test]
    fn a_container_that_exited_is_reported_as_finished(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(
            &state,
            vec![LogEvent::Ended("The log stream ended.".into())],
            cx,
        );
        panel.read_with(cx, |panel, _| {
            let notice = panel.log_failure_notice().expect("a verdict to report");
            assert_eq!(notice.title, "Container finished");
            assert_eq!(notice.severity, Severity::Muted);
            assert!(
                !notice.guidance.contains("log stream stopped"),
                "nothing stopped: {}",
                notice.guidance
            );
        });
        cx.executor()
            .advance_clock(RECONNECT_BASE * 8 + Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            state.borrow().restarts,
            1,
            "a finished container is not reconnected behind the reader's back"
        );
    }

    /// A first request that never answers has to become a failure, not a spinner.
    ///
    /// The watchdog used to be armed only when `attempts > 0`, so the one connection with no way
    /// out of it was the first one: the request goes out, nothing arrives, and the pane says
    /// "Waiting for the first log line…" until the window closes. `UI-SPEC` §9.3 wants a network
    /// timeout visible inside ten seconds, which is a statement about the first attempt too.
    #[gpui_kit::test]
    fn a_first_request_that_never_answers_becomes_a_failure(cx: &mut TestAppContext) {
        let (_panel, _state, cx) = setup(cx);
        cx.run_until_parked();
        assert_eq!(_state.borrow().restarts, 1, "the request went out once");

        cx.executor()
            .advance_clock(RECONNECT_WAIT + Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            _panel.read_with(cx, |panel, _| {
                matches!(panel.phase(), LogPhase::Reconnecting { attempt: 1, .. })
            }),
            "a stream that said nothing for ten seconds is reconnecting, not connecting"
        );
    }

    #[gpui_kit::test]
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
            panel.read_with(cx, |panel, _| panel.log().buffer.is_empty()),
            "a stream change must clear old lines"
        );
    }

    /// Load More History grows history one step at a time.
    #[gpui_kit::test]
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

    /// The failure the log source reports for a Pod that is still Pending: no container has a
    /// status yet, so the Pod is not missing.
    const PENDING_POD_REASON: &str = "The log request failed: containerStatuses is empty for pod \
         \"web-0\". Check the Pod, cluster connection, and access permissions, then try again.";

    /// A Pod that has not started yet is not a Pod that is gone, and the wording must not send
    /// the user looking for a Pod that is still there.
    #[gpui_kit::test]
    fn a_pending_pod_names_the_container_and_not_a_missing_pod(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            panel.log_mut().phase = LogPhase::Failed {
                reason: PENDING_POD_REASON.to_owned(),
            };
        });
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let notice = panel.log_failure_notice().expect("a failure to report");
            assert_eq!(notice.title, "No container");
            let guidance = notice.guidance.to_ascii_lowercase();
            assert!(
                guidance.contains("no container can stream yet"),
                "{guidance}"
            );
            // The next step is named with the word on the control. The old sentence said
            // "select Retry" for a button the Dock renamed to `Reconnect` when it decided a
            // reconnect is what it does, so a reader following the sentence looked for a word
            // that is not on the screen.
            assert!(
                guidance.contains("reconnect"),
                "the guidance names the control: {guidance}"
            );
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
    #[gpui_kit::test]
    fn a_failed_stream_is_reported_by_one_surface(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        panel.update(cx, |panel, _| {
            panel.log_mut().phase = LogPhase::Failed {
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
            panel.log_mut().phase = LogPhase::Failed {
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
    #[gpui_kit::test]
    fn every_failure_class_reports_its_own_next_step(cx: &mut TestAppContext) {
        let (panel, _state, cx) = setup(cx);
        for (reason, title, severity) in [
            ("pods \"web-0\" not found", "Pod missing", Severity::Warning),
            (PENDING_POD_REASON, "No container", Severity::Warning),
            (
                "pods \"web-0\" is forbidden: User \"dev\" cannot get resource \"pods/log\"",
                "Access denied",
                Severity::Error,
            ),
            (
                "The server rejected our request for an unknown reason",
                "Log request failed",
                Severity::Error,
            ),
        ] {
            panel.update(cx, |panel, _| {
                panel.log_mut().phase = LogPhase::Failed {
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
            panel.log_mut().phase = LogPhase::Failed {
                reason: "pods \"web-0\" not found".to_owned(),
            };
        });
        panel.read_with(cx, |panel, _| {
            let notice = panel.log_failure_notice().expect("a failure to report");
            let guidance = notice.guidance.to_ascii_lowercase();
            assert_eq!(notice.title, "Pod missing");
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

    #[gpui_kit::test]
    fn clear_keeps_streaming_and_empties_the_buffer(cx: &mut TestAppContext) {
        let (panel, state, cx) = setup(cx);
        push(&state, vec![LogEvent::Line("a".into())], cx);
        panel.update(cx, |panel, cx| panel.clear(cx));
        panel.read_with(cx, |panel, _| {
            assert!(panel.log().buffer.is_empty());
            assert_eq!(panel.log().synced, 0);
            assert!(matches!(panel.phase(), LogPhase::Streaming));
        });
        push(&state, vec![LogEvent::Line("b".into())], cx);
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.log().buffer.len(),
                1,
                "the stream continues after the buffer is cleared"
            );
        });
    }

    /// The burst a log stream actually produces is absorbed without blocking the UI, the buffer
    /// stays capped, and the per-line cost does not grow with the size of the burst.
    ///
    /// The last clause is the one this test exists for, and it is deliberately **not** an
    /// absolute wall-clock number. This suite runs 1000-odd tests in parallel on a machine shared
    /// with other builds, and an absolute threshold is the one kind of assertion a busy machine
    /// breaks without any code having changed: this test failed at 11.3s against a 10s budget
    /// under load and passed at 4.5s alone, on the same commit. A threshold that reports a
    /// regression only when someone else is compiling is not a measurement.
    ///
    /// So the cost is measured **per line, at two sizes, in the same run**. Whatever the machine
    /// is doing, it is doing it to both measurements, so the ratio survives it, and the thing
    /// being caught is the thing that actually matters: work per line that grows with the length
    /// of the stream — a re-render per line, a linear scan of the buffer per push, a `Vec` that
    /// is drained by copy. Those turn the 100,000-line case into ten times the per-line cost of
    /// the 10,000-line case, and the ratio sees it. The absolute budget is kept as a backstop
    /// against a pathological change, at a level no amount of contention can reach, so the test
    /// still fails loudly rather than merely slowly if something is wrong in a new way.
    #[gpui_kit::test]
    fn burst_of_one_hundred_thousand_lines_is_buffered(cx: &mut TestAppContext) {
        /// How much worse per line the big burst may be than the small one.
        ///
        /// Four, not one-and-a-bit: the small burst pays the one-off costs the big one amortises
        /// — the channel filling, the first park, the buffer reaching its cap — so its per-line
        /// figure is the pessimistic one and some slack is the honest reading. Anything superlinear
        /// shows up as ten, and the ring buffer's own ceiling means a correct implementation
        /// cannot be worse than this.
        const PER_LINE_TOLERANCE: f64 = 4.0;
        /// A backstop no contention can reach, so it cannot be what fails a CI run.
        const ABSOLUTE_BACKSTOP: Duration = Duration::from_secs(60);

        fn lines(count: usize) -> Vec<LogEvent> {
            (0..count)
                .map(|i| {
                    LogEvent::Line(format!(
                        "2026-09-23T10:{:02}:{:02}Z INFO m32-line {i}",
                        (i / 60) % 60,
                        i % 60
                    ))
                })
                .collect()
        }

        let (panel, state, cx) = setup(cx);

        let small_started = Instant::now();
        push(&state, lines(10_000), cx);
        let small = small_started.elapsed();

        let large_started = Instant::now();
        push(&state, lines(100_000), cx);
        let large = large_started.elapsed();

        let per_line = |total: Duration, count: usize| total.as_nanos() as f64 / count as f64;
        let small_per_line = per_line(small, 10_000);
        let large_per_line = per_line(large, 100_000);

        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.log().buffer.len(), crate::panels::logs::RING_CAPACITY);
            assert!(panel.log().buffer_bytes <= LOG_BUFFER_MAX_BYTES);
            assert_eq!(panel.log().lines_received, 110_000);
            assert!(matches!(panel.phase(), LogPhase::Streaming));
        });

        assert!(
            large_per_line <= small_per_line * PER_LINE_TOLERANCE,
            "cost per line grew with the size of the burst, which is what a per-line re-render or \
             a linear scan of the buffer looks like: {small_per_line:.0}ns/line at 10,000 and \
             {large_per_line:.0}ns/line at 100,000 ({}x, budget {PER_LINE_TOLERANCE}x)",
            large_per_line / small_per_line.max(f64::MIN_POSITIVE),
        );
        assert!(
            large < ABSOLUTE_BACKSTOP,
            "100,000 lines took {large:?}, past the {ABSOLUTE_BACKSTOP:?} backstop. This is not a \
             performance budget on a shared machine — the per-line assertion above is the one that \
             catches a regression — so reaching this means the work itself changed shape."
        );
    }

    /// Uses the download filename pattern.
    #[gpui_kit::test]
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

    impl Render for FakeTerminalView {
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
        Entity<DockPanel>,
        Rc<RefCell<FakeTerminals>>,
        Rc<RefCell<FakeForwards>>,
        &'a mut VisualTestContext,
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

    #[gpui_kit::test]
    fn missing_terminal_services_show_connect_without_add(cx: &mut TestAppContext) {
        init_app(cx);
        let (panel, cx) = cx.add_window_view(|_, cx| DockPanel::new(cx));
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(None, cx);
            panel.show_terminal_tab(cx);
        });
        cx.simulate_resize(REAL_WINDOW);
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

    #[gpui_kit::test]
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
        let add = cx.debug_bounds("terminal-add").expect("New terminal");
        assert!(f32::from(context.size.width) > 0.0);
        assert!(context.origin.y >= toolbar.origin.y);
        assert!(context.bottom() <= toolbar.bottom());
        assert!(add.size.height >= design::size::CONTROL);
        assert!(add.origin.y >= state.origin.y);
        assert!(add.bottom() <= state.bottom());
        // A compact Dock drops the maximize control, so the empty state must not depend on it.
        assert!(cx.debug_bounds("terminal-maximize").is_none());

        cx.simulate_click(add.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |panel, _| panel.terminal_count()), 1);
        assert_eq!(terminals.borrow().opened, 1);
    }

    #[gpui_kit::test]
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
        cx.simulate_resize(REAL_WINDOW);
        cx.run_until_parked();
        assert!(panel.read_with(cx, |panel, _| panel.terminal_available()));
        assert!(cx.debug_bounds("terminal-add").is_some());
        assert!(cx.debug_bounds("terminal-actions-trigger").is_some());
    }

    #[gpui_kit::test]
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
        assert!(
            (f32::from(toolbar.size.height) - f32::from(design::size::DOCK_TOOLBAR)).abs() <= 1.0
        );
        assert!(target.origin.y >= toolbar.origin.y);
        assert!(target.bottom() <= toolbar.bottom());
        assert!(
            (f32::from(actions.size.height) - f32::from(BAND_CONTROL)).abs() <= 1.0,
            "the session controls take the band's control height"
        );
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
    ///
    /// `UI-SPEC` §16.4 adds the 16px inset to the same three edges the rule already covered.
    #[gpui_kit::test]
    fn the_terminal_pane_insets_the_session_and_rules_off_the_dock(cx: &mut TestAppContext) {
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
        // `UI-SPEC` §16.4 puts 16px of the Dock's own surface around the canvas. The assertion
        // used to be `view.origin == pane.origin`, which held the padding at zero; the spec
        // changed that on purpose, so the boundary is now checked where it is: a rule below the
        // session, `space::LG` on the three other sides.
        let inset = space::LG;
        assert!(
            (f32::from(pane.bottom() - view.bottom()) - f32::from(inset + design::border::LINE))
                .abs()
                <= 1.0,
            "the pane's bottom edge is {inset:?} of padding and one rule below the session: \
             pane {pane:?}, view {view:?}"
        );
        for (got, name) in [
            (view.origin.x - pane.origin.x, "left"),
            (view.origin.y - pane.origin.y, "top"),
            (pane.right() - view.right(), "right"),
        ] {
            assert!(
                (f32::from(got) - f32::from(inset)).abs() <= 1.0,
                "the session is inset {inset:?} from the {name} edge: pane {pane:?}, view {view:?}"
            );
        }
    }

    /// The verdict slot is reserved whether or not there is a verdict, so a session that ends does
    /// not slide its neighbours along the session strip.
    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn open_terminal_switches_to_terminal_tab(cx: &mut TestAppContext) {
        let (panel, terminals, _forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("local terminal opens");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminal_count(), 1);
            assert_eq!(panel.terminal_focus_handles.len(), 1);
            assert_eq!(
                panel.active_tab,
                DockTab::Terminal,
                "opening a terminal selects the Terminal tab"
            );
        });
        assert_eq!(terminals.borrow().opened, 1);
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
    #[gpui_kit::test]
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
                Bounds::new(point(px(0.), px(0.)), size(px(1_000.), px(100.)))
            )),
            TERMINAL_SPLIT_MAX_RATIO,
            "a pointer past the right edge clamps too"
        );
    }

    #[gpui_kit::test]
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

    /// Switching cluster closes the sessions pointed at the old one.
    ///
    /// A shell is a live connection and a forward is a live tunnel, both into the cluster they
    /// were started against. Keeping them across a cluster switch left the reader with chips
    /// naming one cluster in a window whose every other surface named another, and three easy
    /// ways to run a command against the wrong cluster. A *namespace* switch is not a cluster
    /// switch and must leave a shell alone.
    #[gpui_kit::test]
    fn switching_cluster_closes_the_old_cluster_sessions(cx: &mut TestAppContext) {
        let (panel, terminals, forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| panel.open_terminal(TerminalKind::Local, cx))
            .expect("open");
        assert_eq!(panel.read_with(cx, |panel, _| panel.terminal_count()), 1);

        // The namespace moves and the shell stays: comparing two namespaces is what it is for.
        let mut scoped = fake_services(Rc::clone(&terminals), Rc::clone(&forwards));
        scoped.namespace = Some("team-a".to_owned());
        let scoped_context = scoped.context.clone();
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(Some(scoped), cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.terminal_count(), 1);
            assert_eq!(
                panel.terminals[0].request.context.as_deref(),
                scoped_context.as_deref(),
            );
        });

        let mut elsewhere = fake_services(Rc::clone(&terminals), Rc::clone(&forwards));
        elsewhere.context = Some("other-cluster".to_owned());
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(Some(elsewhere), cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.terminal_count(),
                0,
                "a shell in the previous cluster does not survive the switch"
            );
            assert_eq!(
                panel.terminal_focus_handles.len(),
                0,
                "the focus handles go with the sessions, or the next session inherits a stale one"
            );
        });
    }

    /// Switching cluster closes the old cluster's forwards.
    #[gpui_kit::test]
    fn switching_cluster_closes_the_old_cluster_forwards(cx: &mut TestAppContext) {
        let (panel, terminals, forwards, cx) = setup_terminals(cx);
        panel
            .update(cx, |panel, cx| {
                panel.start_forward(
                    ForwardRequest {
                        context: Some("kind-dev".to_owned()),
                        namespace: Some("default".to_owned()),
                        name: "web-0".into(),
                        remote_port: 8080,
                        local_port: None,
                    },
                    cx,
                )
            })
            .expect("start forward");
        cx.run_until_parked();
        assert_eq!(panel.read_with(cx, |panel, _| panel.forward_count()), 1);

        let mut elsewhere = fake_services(Rc::clone(&terminals), Rc::clone(&forwards));
        elsewhere.context = Some("other-cluster".to_owned());
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(Some(elsewhere), cx)
        });
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.forward_count(),
                0,
                "a tunnel into the previous cluster does not survive the switch"
            );
        });
    }

    /// A log stream does not follow the window to another cluster.
    ///
    /// A Pod name is not unique across clusters. The stream used to reconnect against the new
    /// factory and keep filling the same tab, so a reader who switched from `prod` to `staging`
    /// with `default/web-0` open went on reading a *different* Pod's logs under the same label,
    /// with nothing on screen saying the tab had changed what it was pointed at.
    #[gpui_kit::test]
    fn switching_cluster_closes_the_old_cluster_log_streams(cx: &mut TestAppContext) {
        let (panel, terminals, forwards, cx) = setup_terminals(cx);
        panel.update(cx, |panel, cx| {
            panel.open_logs(
                LogRequest {
                    namespace: Some("default".into()),
                    name: "web-0".into(),
                    containers: vec!["app".into()],
                },
                cx,
            )
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.request().is_some(),
                "the stream is open before the switch"
            );
        });

        let mut elsewhere = fake_services(Rc::clone(&terminals), Rc::clone(&forwards));
        elsewhere.context = Some("other-cluster".to_owned());
        panel.update(cx, |panel, cx| {
            panel.set_terminal_services(Some(elsewhere), cx)
        });
        panel.read_with(cx, |panel, _| {
            assert!(
                panel.request().is_none(),
                "a stream against the previous cluster does not survive the switch"
            );
            assert_eq!(panel.phase(), &LogPhase::Idle);
        });
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
        assert_eq!(
            panel.read_with(cx, |panel, _| panel.active_tab),
            DockTab::Logs(0)
        );
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

    #[gpui_kit::test]
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
        panel.update(cx, |panel, _| panel.active_tab = DockTab::Logs(0));
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
            assert_eq!(panel.active_tab, DockTab::Logs(0));
        });
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
