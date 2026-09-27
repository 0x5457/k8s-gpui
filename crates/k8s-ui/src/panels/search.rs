use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::ListItem;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{
    ActiveTheme, FocusableExt as _, Icon, RoleOverride, Sizable, Size, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Animation, AnimationExt as _};
use gpui_kit::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight,
    Hsla, InteractiveElement, IntoElement, Keystroke, MouseButton, ParentElement, Pixels, Render,
    Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Task, Window, div, px,
};
use k8s_core::cluster::{ClusterId, ClusterRegistry};
use k8s_core::controller::{SearchError, SearchOutcome};
use k8s_core::discovery::ResourceCatalog;
use k8s_core::fuzzy::{CasePolicy, MatchRun, Ranker, match_runs};
use k8s_core::machines::{SearchEffect, SearchEvent, SearchHit, SearchMachine, SearchPhase};
use statig::blocking::StateMachine;
use statig::prelude::IntoStateMachineExt as _;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;

use crate::design::{self, Confidence};
use crate::panels::common;
use crate::session::{ClusterSession, ResourceSpec, TextInput};

/// The outcome half of a failed search.
///
/// The reason travels through the state machine as one string, and it is what
/// went wrong rather than what to do about it: `writing.md > Best practices`
/// asks an error to sit as close to the problem as possible, and one sentence
/// carrying both the problem and the repair ran to ninety characters in a popup
/// that then truncated it. `error_repair` is the other half.
pub const SEARCH_FAILED: &str = "Resource search failed.";
pub const SEARCH_UNAVAILABLE: &str = "Resource search is unavailable for this cluster.";
pub const SEARCH_UNAUTHORIZED: &str = "Kubernetes rejected the cluster credentials.";
pub const SEARCH_FORBIDDEN: &str = "This identity cannot list resources in this cluster.";
pub const SEARCH_TIMED_OUT: &str = "Resource search timed out.";
pub const SEARCH_NO_RESOURCES: &str = "No listable resource types are available.";

const SEARCH_WIDTH: f32 = 560.0;
const SEARCH_MAX_HEIGHT: f32 = 420.0;
const SEARCH_PAGE_ROWS: isize = 10;
/// The card's own title. A state inside the card must not repeat it.
const SEARCH_CARD_TITLE: &str = "Search resources";
/// Title of the empty state, which is the one place that can add a fact the
/// header and the placeholder do not already say.
const SEARCH_IDLE_STATE_TITLE: &str = "Nothing to search yet";
const PARTIAL_RESULTS_NOTICE: &str = "Some resource types were not searched, so these results are incomplete. Check list permission, then search again.";
const NO_MATCH_NEXT_STEP: &str =
    "Try another name, or check that you can list resources in this cluster.";
const SEARCH_LIMIT_NEXT_STEP: &str = "Try a more specific resource name.";
/// Group heading above the recent searches.
pub const SEARCH_RECENT_GROUP: &str = "Recent searches";
/// Every shortcut the dialog answers, including Tab between the field, Clear, and
/// Close, and the two result actions a search result can run. The chords are DOM
/// key names, which is the vocabulary `aria-keyshortcuts` uses.
pub const SEARCH_KEYSHORTCUTS: &str = "Tab Shift+Tab ArrowUp ArrowDown Home End PageUp PageDown \
     Enter Control+Enter Control+Shift+Enter Escape";

/// What the user can do with a result without leaving the search overlay.
///
/// Opening a result is one keystroke; exec and port-forward are one keystroke too,
/// so a command-palette-style search does not need two extra steps to reach them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchResultAction {
    Open,
    Exec,
    PortForward,
}

impl SearchResultAction {
    /// Palette and tooltip wording.
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Exec => "Exec",
            Self::PortForward => "Port Forward",
        }
    }

    /// DOM key name for `aria-keyshortcuts`.
    ///
    /// `aria-keyshortcuts` speaks the DOM vocabulary, where the command key is
    /// `Control` and the macOS key is `Meta`, so this string is the same on every
    /// platform and is not what the footer shows.
    pub fn dom_keystrokes(self) -> &'static str {
        match self {
            Self::Open => "Enter",
            Self::Exec => "Control+Enter",
            Self::PortForward => "Control+Shift+Enter",
        }
    }

    /// Chord that runs the action on the selected result, as this platform names
    /// its keys.
    pub fn keystrokes(self) -> String {
        let command = command_modifier();
        match self {
            Self::Open => "Enter".to_owned(),
            Self::Exec => format!("{command}+Enter"),
            Self::PortForward => format!("{command}+Shift+Enter"),
        }
    }
}

/// The name this platform gives the command modifier.
///
/// The keymap assets spell the platform command key `secondary`, and GPUI
/// resolves that per platform: `ctrl` on Linux and Windows, `cmd` on macOS. The
/// panel answers both `Control+Enter` and `Meta+Enter`, so the hint reads its
/// name off a parsed keystroke instead of hardcoding one, and a Linux reader is
/// never told about `Cmd`.
fn command_modifier() -> &'static str {
    let secondary =
        gpui_kit::Keystroke::parse("secondary-enter").expect("the keymap's own chord parses");
    if secondary.modifiers.platform {
        "Cmd"
    } else {
        "Ctrl"
    }
}

fn search_width(container_width: f32) -> f32 {
    (container_width - f32::from(design::space::XXL)).clamp(0.0, SEARCH_WIDTH)
}

fn search_max_height(container_height: f32) -> f32 {
    (container_height - 2.0 * f32::from(design::space::XL)).clamp(0.0, SEARCH_MAX_HEIGHT)
}

// ── Overlay chrome ──────────────────────────────────────────────────────────
//
// One scrim, one focus ring, one keycap, one icon slot: the four decisions a
// modal surface makes that a panel cannot make once per call site. Each is
// written out here with the reason it is one value, because "what does a focus
// ring look like in this app" is exactly the question that goes wrong when every
// overlay answers it locally.

/// Share of the shared scrim this card keeps.
///
/// `design::role::surface_backdrop` is the product's one scrim and it is right for a
/// surface that owns the whole window. This card is opened out of the middle of a
/// working session and dismissed straight back into it, so what is behind it is a
/// task the reader is about to resume rather than a page to forget — and
/// `modality.md > Best practices` warns that a modal which obscures its previous
/// context makes people lose track of the task they suspended. At the token's full
/// strength the app behind a near-black field is gone: the cluster tree, the table
/// and the status bar all drop to one flat value, and the card floats in a void.
///
/// Composing the token with the app's own canvas at half its ink keeps the state
/// change — everything behind the card moves one step down the ladder, and the
/// card's own shadow still has something to sit on — while leaving the toolbar,
/// the tree and the current view legible enough to say what the search was opened
/// from. It is one number composed with the shared token rather than a second
/// scrim, so a theme that moves the canvas moves the scrim with it.
const SCRIM_INK: f32 = 0.38;

/// The scrim behind the search card. See [`SCRIM_INK`].
fn scrim(cx: &App) -> Hsla {
    design::role::surface_backdrop_at(cx, SCRIM_INK)
}

/// A keycap: the one shape a shortcut has in this card.
///
/// `text::MICRO` on the card's own plane, described by a `border_subtle` hairline
/// instead of a fill. A filled cap is a second surface inside a list of rows, which
/// is what made the footer's hints read as data and the palette's chips read as the
/// loudest thing on a quiet row; a hairline says the same thing for one pixel of
/// ink. Its height comes from its own type and padding rather than from a number,
/// so every cap in the card is the same box without anyone measuring one.
fn keycap(key: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .px(design::space::XS)
        .py(design::space::XXS)
        .rounded(design::radius::SM)
        .bg(design::role::surface_raised(cx))
        .border_1()
        .border_color(design::role::border_subtle(cx))
        .child(
            Label::new(key)
                .text_size(design::text::MICRO)
                .line_height(design::text::MICRO_LINE_HEIGHT)
                .text_color(design::role::fg_secondary(cx)),
        )
}

/// One footer hint: what the key does, then the key.
///
/// The verb is `text::MICRO` in the tertiary ink and the chord is a hairline cap,
/// so a hint reads as a hint. It was one `11px` string — `Navigate: ↑/↓` — which
/// put the verb, the separator and the chord at one weight and let six of them sit
/// in the card's own type size directly under the results.
fn search_hint(hint: &SearchHint, cx: &App) -> AnyElement {
    let id = format!("resource-search-hint-{}-{}", hint.verb, hint.key);
    h_flex()
        .id(id)
        .flex_none()
        .gap(design::space::XS)
        .items_center()
        // The verb and the chord are one accessible name, so a screen reader reads
        // the hint as one thing instead of stopping at the separator.
        .aria_label(hint.line())
        .child(
            Label::new(hint.verb)
                .text_size(design::text::MICRO)
                .line_height(design::text::MICRO_LINE_HEIGHT)
                .text_color(design::role::fg_tertiary(cx)),
        )
        .child(keycap(hint.key.clone(), cx))
        .into_any_element()
}

/// Width of the lane every row's icon sits in, and the size of the glyph in it.
///
/// A fixed slot is what keeps a name on one spine whether or not the row has an
/// icon to put in it. `ListItem` gives a row its padding but reserves no icon lane,
/// so the lane is the row's to reserve — and a label that moves two pixels because
/// its row gained a glyph is a different defect from a row without one.
///
/// `size::KIND_ICON` rather than `size::ICON`: this is the kind set, drawn and
/// judged at fourteen pixels, and the same glyph in the sidebar is fourteen pixels.
const ICON_SLOT: Pixels = design::size::KIND_ICON;

/// Share of a row the namespace lane takes, shared by the column header and by
/// every row under it.
const NAMESPACE_LANE: f32 = 0.28;

/// The three lanes every row in the results shares: an icon slot, the namespace,
/// and the name.
///
/// Both kinds of row are built from this one builder, so a recent search and a
/// resource put their names on the same line and the column header above them
/// names the columns those lanes really are. Three rows that each describe their
/// own columns is how the recent searches ended up two lanes to the left of every
/// result.
fn result_lanes(icon: AnyElement, namespace: AnyElement, name: AnyElement) -> AnyElement {
    h_flex()
        .w_full()
        .min_w(px(0.0))
        .gap(design::space::SM)
        .items_center()
        .child(
            div()
                .w(ICON_SLOT)
                .flex_none()
                .flex()
                .items_center()
                .child(icon),
        )
        .child(
            div()
                .w(gpui_kit::relative(NAMESPACE_LANE))
                .min_w(px(0.0))
                .flex_none()
                .child(namespace),
        )
        .child(name)
        .into_any_element()
}

/// The measure `UI-SPEC` §2.3 gives a state's description: 40ch.
///
/// The width of the state's text column, in logical pixels.
fn empty_measure() -> f32 {
    f32::from(design::text::BODY) * 0.6 * common::EMPTY_MEASURE_CH
}

/// The heading above one group of rows.
///
/// `text::CAPTION` uppercase semibold in the tertiary ink. A group head is a label
/// for a set rather than one more row of it, and the two used to be the same size,
/// weight and ink, so the only thing that said which kind a row was belonged to a
/// screen reader. The space above every head is the same `space::SM`, the first
/// one included, so the groups read as groups and the first is not welded to the
/// column header above it.
fn group_heading(id: &str, label: &str, cx: &App) -> AnyElement {
    let heading = id.to_owned();
    h_flex()
        .flex_none()
        .mt(design::space::SM)
        .h(design::size::GROUP_HEAD)
        .px(design::space::MD)
        .items_center()
        .debug_selector(move || format!("{heading}-heading"))
        .child(
            Label::new(label.to_uppercase())
                .text_size(design::text::CAPTION)
                .line_height(design::text::CAPTION_LINE_HEIGHT)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(design::role::fg_tertiary(cx)),
        )
        .into_any_element()
}

#[derive(Clone)]
pub struct SearchExecutor {
    registry: Arc<ClusterRegistry>,
    cluster: ClusterId,
    handle: Handle,
    resources: Arc<Vec<k8s_core::discovery::ResourceEntry>>,
    targets_ready: bool,
}

impl SearchExecutor {
    pub fn from_session(session: &ClusterSession) -> Option<Self> {
        match session {
            ClusterSession::Ready {
                registry,
                cluster,
                handle,
                ..
            } => Some(Self {
                registry: Arc::clone(registry),
                cluster: *cluster,
                handle: handle.clone(),
                resources: Arc::new(Vec::new()),
                targets_ready: false,
            }),
            ClusterSession::Unavailable { .. } => None,
        }
    }

    pub fn cluster_name(&self) -> Option<&str> {
        self.registry
            .get(self.cluster)
            .map(|cluster| cluster.name())
    }

    fn set_catalog(&mut self, catalog: &ResourceCatalog) {
        self.resources = Arc::new(catalog.searchable_entries());
        self.targets_ready = true;
    }

    fn targets_ready(&self) -> bool {
        self.targets_ready
    }

    fn spawn(
        &self,
        query: String,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<SearchOutcome, String>>,
    ) {
        let registry = Arc::clone(&self.registry);
        let cluster = self.cluster;
        let resources = Arc::clone(&self.resources);
        let (cancel, cancelled) = oneshot::channel();
        let task = self.handle.spawn(async move {
            tokio::select! {
                result = async {
                    let Some(cluster) = registry.get(cluster) else {
                        return Err(SEARCH_UNAVAILABLE.to_owned());
                    };
                    cluster
                        .search_cluster_resources(&resources, &query)
                        .await
                        .map_err(|error| {
                            eprintln!("k8s-gpui: resource search failed: {error}");
                            search_error_reason(error)
                        })
                } => result,
                _ = cancelled => Err(SEARCH_FAILED.to_owned()),
            }
        });
        (cancel, task)
    }
}

fn search_error_reason(error: SearchError) -> String {
    match error {
        SearchError::Timeout => SEARCH_TIMED_OUT.to_owned(),
        SearchError::Unauthorized => SEARCH_UNAUTHORIZED.to_owned(),
        SearchError::Forbidden => SEARCH_FORBIDDEN.to_owned(),
        SearchError::NoResources => SEARCH_NO_RESOURCES.to_owned(),
        SearchError::Api(_) | SearchError::Unavailable => SEARCH_FAILED.to_owned(),
    }
}

type OpenHandler = Rc<dyn Fn(SearchHit, &mut Window, &mut App)>;
/// Runs a result action other than opening the preview.
type ActionHandler = Rc<dyn Fn(SearchHit, SearchResultAction, &mut Window, &mut App)>;
type CloseHandler = Rc<dyn Fn(&mut Window, &mut App)>;

struct ActiveRequest {
    request_id: u64,
    cancel: Option<oneshot::Sender<()>>,
}

pub struct SearchView {
    /// The field, and the app's own `TextInput`.
    ///
    /// This is not one of gpui-kit's `Input` widgets, and it is the one piece of
    /// the panel that is not: `Input` answers a caret, a selection and a value, and
    /// this field also has to expose its clear button as a real tab stop for the
    /// Tab ring below, carry its own accessible name and instructions, and let a
    /// keystroke cancel an IME composition before anything else sees it. Those are
    /// the panel's own behaviours on top of a field, and `TextInput` is where they
    /// live for every field in the app.
    input: Entity<TextInput>,
    machine: StateMachine<SearchMachine>,
    effects: UnboundedReceiver<SearchEffect>,
    executor: Option<SearchExecutor>,
    target: Option<ResourceSpec>,
    open: bool,
    debounce_task: Option<Task<()>>,
    debounce_epoch: u64,
    input_sync_epoch: Rc<Cell<u64>>,
    request: Option<ActiveRequest>,
    scroll: ScrollHandle,
    container_bounds: Option<Bounds<Pixels>>,
    close_focus: FocusHandle,
    retry_focus: FocusHandle,
    /// The no-match state's own clear control. The field already has a clear
    /// button, but the state that needs the repair is below the field, so the
    /// repair is offered where the reader is looking.
    state_clear_focus: FocusHandle,
    /// Cursor over the recent searches, which have no hit index of their own.
    recent_cursor: usize,
    on_open: Option<OpenHandler>,
    on_action: Option<ActionHandler>,
    on_close: Option<CloseHandler>,
}

impl SearchView {
    pub fn new(executor: Option<SearchExecutor>, cx: &mut Context<Self>) -> Self {
        let view = cx.weak_entity();
        let input_sync_epoch = Rc::new(Cell::new(0u64));
        let queued_input_sync = input_sync_epoch.clone();
        let input = cx.new(|cx| {
            TextInput::new("Search resource names…", cx, move |_text, cx| {
                let view = view.clone();
                let epoch = queued_input_sync.get().wrapping_add(1);
                queued_input_sync.set(epoch);
                cx.defer(move |cx| {
                    view.update(cx, |view, cx| {
                        if view.input_sync_epoch.get() == epoch {
                            view.sync_query_from_input(cx);
                        }
                    })
                    .ok();
                });
            })
            .without_escape_hint()
            .with_accessibility(
                "Search resources",
                format!(
                    "Type a resource name to search. Results are grouped by kind. Use Up or Down \
                     to select a result. Use {command}+Home or {command}+End for the first or \
                     last result. Use Page Up or Page Down to move one page. Press Enter to open \
                     the result, {command}+Enter to open a shell, or {command}+Shift+Enter to \
                     start a port forward. Press Tab to move between the field, Clear, and \
                     Close. Press Escape to close the search.",
                    command = command_modifier()
                ),
                "Clear Resource Search",
            )
            .with_width(px(SEARCH_WIDTH - 2.0 * f32::from(design::space::MD)))
        });
        let (effects, receiver) = unbounded_channel();
        Self {
            input,
            machine: SearchMachine::new(effects).state_machine(),
            effects: receiver,
            executor,
            target: None,
            open: false,
            debounce_task: None,
            debounce_epoch: 0,
            input_sync_epoch,
            request: None,
            scroll: ScrollHandle::new(),
            container_bounds: None,
            close_focus: cx.focus_handle().tab_stop(true).tab_index(2isize),
            retry_focus: cx.focus_handle().tab_stop(true).tab_index(4isize),
            state_clear_focus: cx.focus_handle().tab_stop(true).tab_index(3isize),
            recent_cursor: 0,
            on_open: None,
            on_action: None,
            on_close: None,
        }
    }

    pub fn set_open_handler(&mut self, handler: OpenHandler) {
        self.on_open = Some(handler);
    }

    /// Installs the handler for exec and port-forward on a result.
    pub fn set_action_handler(&mut self, handler: ActionHandler) {
        self.on_action = Some(handler);
    }

    /// True when `action` has a handler, so the footer only advertises chords
    /// the shell actually answers.
    fn supports(&self, action: SearchResultAction) -> bool {
        match action {
            SearchResultAction::Open => self.on_open.is_some(),
            SearchResultAction::Exec | SearchResultAction::PortForward => self.on_action.is_some(),
        }
    }

    /// Result actions this panel can run right now.
    fn footer_actions(&self) -> Vec<SearchResultAction> {
        [
            SearchResultAction::Open,
            SearchResultAction::Exec,
            SearchResultAction::PortForward,
        ]
        .into_iter()
        .filter(|action| self.supports(*action))
        .collect()
    }

    pub fn set_close_handler(&mut self, handler: CloseHandler) {
        self.on_close = Some(handler);
        self.close_focus = self.close_focus.clone().tab_stop(true).tab_index(2isize);
    }
    pub fn set_executor(&mut self, executor: Option<SearchExecutor>, cx: &mut Context<Self>) {
        self.executor = executor;
        if self.open {
            let query = self.query().to_owned();
            self.dispatch(SearchEvent::QueryChanged { query }, cx);
        } else {
            cx.notify();
        }
    }

    pub fn set_catalog(&mut self, catalog: &ResourceCatalog, cx: &mut Context<Self>) {
        if let Some(executor) = self.executor.as_mut() {
            executor.set_catalog(catalog);
        }
        if self.open && !self.query().is_empty() {
            let query = self.query().to_owned();
            self.dispatch(SearchEvent::QueryChanged { query }, cx);
        } else {
            cx.notify();
        }
    }

    pub fn catalog_waiting(&self) -> bool {
        self.executor
            .as_ref()
            .is_some_and(|executor| !executor.targets_ready())
    }

    pub fn open(&mut self, spec: ResourceSpec, cx: &mut Context<Self>) -> bool {
        self.open_inner(Some(spec), None, cx)
    }

    pub fn open_cluster(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.open_inner(None, Some(window), cx)
    }

    /// `window` is the caller's, or `None` for `open`, which only the tests call
    /// and which therefore has nothing to write the field's empty value from.
    fn open_inner(
        &mut self,
        target: Option<ResourceSpec>,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.target = target;
        self.open = true;
        self.recent_cursor = 0;
        match window {
            Some(window) => self.input.update(cx, |input, cx| input.clear(window, cx)),
            None => self.input.update(cx, |input, cx| input.clear_pending(cx)),
        }
        self.dispatch(SearchEvent::Open, cx);
        true
    }

    #[cfg(test)]
    pub(crate) fn inject_results_for_test(&mut self, hits: Vec<SearchHit>) {
        self.inject_results_for_test_with_query(hits, "test");
    }

    #[cfg(test)]
    fn inject_results_for_test_with_query(&mut self, hits: Vec<SearchHit>, query: &str) {
        self.inject_results_for_test_at_request(hits, query, 1, 1, false, false);
    }

    /// Drives the machine straight into a results state, which the input never
    /// asked for. Any input change queued by an earlier edit is still waiting
    /// on the next effect pass, so drop it here: it would otherwise read the
    /// input's older text and clear the results this just installed.
    #[cfg(test)]
    fn inject_results_for_test_at_request(
        &mut self,
        hits: Vec<SearchHit>,
        query: &str,
        request_id: u64,
        scanned: usize,
        truncated: bool,
        partial: bool,
    ) {
        self.debounce_epoch = self.debounce_epoch.wrapping_add(1);
        self.debounce_task = None;
        self.request = None;
        self.input_sync_epoch
            .set(self.input_sync_epoch.get().wrapping_add(1));
        self.machine.handle(&SearchEvent::Open);
        self.machine.handle(&SearchEvent::QueryChanged {
            query: query.to_owned(),
        });
        let mut effective_request_id = request_id;
        while let Ok(effect) = self.effects.try_recv() {
            if let SearchEffect::Schedule {
                request_id: scheduled_id,
                ..
            } = effect
            {
                effective_request_id = scheduled_id;
            }
        }
        self.machine.handle(&SearchEvent::DebounceElapsed {
            request_id: effective_request_id,
        });
        self.machine.handle(&SearchEvent::Results {
            request_id: effective_request_id,
            hits,
            scanned,
            truncated,
            partial,
        });
        while self.effects.try_recv().is_ok() {}
    }

    /// Closes the panel and empties the field.
    ///
    /// Three of the callers are session teardown — a cluster switch, a kubeconfig
    /// reload, a startup that never reached a registry — and one of those runs
    /// inside a detached task where no window exists or can be had. The field is
    /// closed with the panel and the next open empties it again, so the value is
    /// left for the frame that draws the panel rather than written through.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.dispatch(SearchEvent::Close, cx);
        self.target = None;
        self.open = false;
        self.input.update(cx, |input, cx| input.clear_pending(cx));
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    pub fn container_bounds(&self) -> Option<Bounds<Pixels>> {
        self.container_bounds
    }

    pub fn set_container_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if self.container_bounds == Some(bounds) {
            return;
        }
        self.container_bounds = Some(bounds);
        cx.notify();
    }

    pub fn phase(&self) -> SearchPhase {
        self.machine.state().phase()
    }

    pub fn query(&self) -> &str {
        self.machine.inner().query()
    }

    pub fn hits(&self) -> &[SearchHit] {
        self.machine.inner().hits()
    }

    pub fn scanned(&self) -> usize {
        self.machine.inner().scanned()
    }

    pub fn truncated(&self) -> bool {
        self.machine.inner().truncated()
    }

    pub fn target(&self) -> Option<&ResourceSpec> {
        self.target.as_ref()
    }

    pub fn partial(&self) -> bool {
        self.machine.inner().partial()
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.machine.inner().selected_index()
    }

    /// Recent queries offered while the field is empty.
    pub fn recents(&self) -> &[String] {
        self.machine.inner().recents()
    }

    /// True when the list shows recent searches instead of results.
    ///
    /// A recent list only appears with an empty field, and an empty field has no
    /// results, so the two lists never share a row.
    fn shows_recents(&self) -> bool {
        self.query().is_empty() && !self.machine.inner().recents().is_empty()
    }

    /// True when the list has nothing to show, so the dialog is showing its
    /// state instead of rows.
    fn shows_state(&self) -> bool {
        !self.shows_recents() && self.hits().is_empty()
    }

    /// The no-match state's clear control, when a completed search read the whole
    /// scope and matched nothing. A failed search offers Retry and a search still
    /// running has the field's own clear right above it, so the control appears
    /// only where it is the answer.
    fn state_clear_focus(&self) -> Option<FocusHandle> {
        (self.phase() == SearchPhase::Results && self.shows_state() && !self.query().is_empty())
            .then(|| self.state_clear_focus.clone())
    }

    /// Empties the field and puts the caret back in it.
    fn clear_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.clear(window, cx));
        self.recent_cursor = 0;
        self.focus_input(window, cx);
    }

    /// Hit indices in display order: one run per kind, in the order the kinds
    /// first appear, with the hit order kept inside a run.
    ///
    /// Grouping is a stable partition, so a kind never appears twice and the
    /// selection still names a hit the user can open.
    fn display_order(&self) -> Vec<usize> {
        let hits = self.hits();
        let mut kinds: Vec<String> = Vec::new();
        let mut order: Vec<usize> = Vec::with_capacity(hits.len());
        for (index, hit) in hits.iter().enumerate() {
            if !kinds.iter().any(|kind| kind == &hit.resource.kind) {
                kinds.push(hit.resource.kind.clone());
                order.extend(
                    (index..hits.len())
                        .filter(|later| hits[*later].resource.kind == hit.resource.kind),
                );
            }
        }
        order
    }

    /// Row of the selected hit in display order.
    fn selected_row(&self, order: &[usize]) -> Option<usize> {
        let selected = self.selected_index()?;
        order.iter().position(|index| *index == selected)
    }

    fn move_row(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.shows_recents() {
            let len = self.machine.inner().recents().len();
            if len == 0 {
                return;
            }
            let offset = delta.unsigned_abs() % len;
            let next = if delta >= 0 {
                (self.recent_cursor + offset) % len
            } else if self.recent_cursor >= offset {
                self.recent_cursor - offset
            } else {
                len - (offset - self.recent_cursor)
            };
            self.recent_cursor = next;
            self.reveal_row(next);
            cx.notify();
            return;
        }
        let order = self.display_order();
        if order.is_empty() {
            return;
        }
        let current = self.selected_row(&order).unwrap_or_default();
        let next = (current as isize + delta).rem_euclid(order.len() as isize) as usize;
        self.select_index(order[next], cx);
    }

    fn move_row_to(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.shows_recents() {
            let len = self.machine.inner().recents().len();
            if len == 0 {
                return;
            }
            self.recent_cursor = row.min(len - 1);
            self.reveal_row(self.recent_cursor);
            cx.notify();
            return;
        }
        let order = self.display_order();
        if let Some(index) = order.get(row).copied() {
            self.select_index(index, cx);
        }
    }

    /// Row reached by Page Up or Page Down from the current selection. Paging
    /// wraps, so the last page and the first page are both reachable.
    fn page_row(&self, delta: isize) -> usize {
        let (len, current) = if self.shows_recents() {
            (self.machine.inner().recents().len(), self.recent_cursor)
        } else {
            let order = self.display_order();
            (order.len(), self.selected_row(&order).unwrap_or_default())
        };
        if len == 0 {
            return 0;
        }
        let offset = (delta * SEARCH_PAGE_ROWS).unsigned_abs() % len;
        if delta >= 0 {
            (current + offset) % len
        } else if current >= offset {
            current - offset
        } else {
            len - (offset - current)
        }
    }

    pub fn handle_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input_focus = self.input.read(cx).focus_handle(cx);
        let input_focused = input_focus.is_focused(window);
        let clear_focus = (!self.input.read(cx).text().is_empty())
            .then(|| self.input.read(cx).clear_focus_handle());
        let clear_focused = clear_focus
            .as_ref()
            .is_some_and(|focus| focus.is_focused(window));
        let close_focused = self.close_focus.is_focused(window);
        let retry_focused = self.retry_focus.is_focused(window);
        let state_clear_focused = self.state_clear_focus.is_focused(window);
        if keystroke.key == "tab" {
            let mut targets = vec![input_focus.clone()];
            if let Some(clear_focus) = clear_focus {
                targets.push(clear_focus);
            }
            if self.on_close.is_some() {
                targets.push(self.close_focus.clone());
            }
            if self.state_clear_focus().is_some() {
                targets.push(self.state_clear_focus.clone());
            }
            if self.phase() == SearchPhase::Error {
                targets.push(self.retry_focus.clone());
            }
            // The ring is read from the focus itself instead of from a running
            // index, so a control that only exists in one state cannot leave the
            // count wrong.
            let current = targets
                .iter()
                .position(|handle| handle.is_focused(window))
                .unwrap_or(0);
            let next = if keystroke.modifiers.shift {
                (current + targets.len() - 1) % targets.len()
            } else {
                (current + 1) % targets.len()
            };
            window.focus(&targets[next], cx);
            return;
        }
        if clear_focused && matches!(keystroke.key.as_str(), "enter" | "return" | "space") {
            self.input.update(cx, |input, cx| input.clear(window, cx));
            window.focus(&input_focus, cx);
            return;
        }
        if clear_focused && matches!(keystroke.key.as_str(), "home" | "end") {
            return;
        }
        let command_home_end = (keystroke.modifiers.control || keystroke.modifiers.platform)
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.function;
        // A modifier plus Enter runs a result action, so exec and port-forward are
        // one keystroke away instead of two steps after opening a preview. A focused
        // button keeps its own activation, so the chord only applies to the list.
        if matches!(keystroke.key.as_str(), "enter" | "return")
            && (keystroke.modifiers.control || keystroke.modifiers.platform)
            && !clear_focused
            && !close_focused
            && !retry_focused
            && !state_clear_focused
        {
            let action = if keystroke.modifiers.shift {
                SearchResultAction::PortForward
            } else {
                SearchResultAction::Exec
            };
            if self.supports(action)
                && let Some(hit) = self.machine.inner().selected_hit()
            {
                self.activate_hit_action(hit, action, window, cx);
                cx.stop_propagation();
                return;
            }
        }
        match keystroke.key.as_str() {
            "escape" => {
                let canceled = input_focused
                    && self
                        .input
                        .update(cx, |input, cx| input.cancel_composition(window, cx));
                if !canceled {
                    if let Some(handler) = self.on_close.clone() {
                        handler(window, cx);
                    } else {
                        self.close(cx);
                    }
                }
                cx.stop_propagation();
            }
            "up" => {
                self.move_row(-1, cx);
                self.focus_input(window, cx);
                cx.stop_propagation();
            }
            "down" => {
                self.move_row(1, cx);
                self.focus_input(window, cx);
                cx.stop_propagation();
            }
            "home" | "end" if input_focused && !command_home_end => {
                self.input.update(cx, |input, cx| {
                    input.handle_keystroke(keystroke, window, cx)
                });
                cx.stop_propagation();
            }
            "home" | "end" => {
                if !input_focused {
                    window.focus(&input_focus, cx);
                }
                let last = if self.shows_recents() {
                    self.machine.inner().recents().len()
                } else {
                    self.display_order().len()
                };
                self.move_row_to(
                    if keystroke.key == "home" {
                        0
                    } else {
                        last.saturating_sub(1)
                    },
                    cx,
                );
                cx.stop_propagation();
            }
            "pageup" => {
                let row = self.page_row(-1);
                self.move_row_to(row, cx);
                self.focus_input(window, cx);
                cx.stop_propagation();
            }
            "pagedown" => {
                let row = self.page_row(1);
                self.move_row_to(row, cx);
                self.focus_input(window, cx);
                cx.stop_propagation();
            }
            "enter" | "return" | "space" if retry_focused => {
                self.retry(window, cx);
                cx.stop_propagation();
            }
            "enter" | "return" | "space" if state_clear_focused => {
                self.clear_query(window, cx);
                cx.stop_propagation();
            }
            "enter" | "return" | "space" if close_focused => {
                if let Some(handler) = self.on_close.clone() {
                    handler(window, cx);
                }
                cx.stop_propagation();
            }
            "enter" | "return" if self.shows_recents() => {
                self.recall_recent(window, cx);
                cx.stop_propagation();
            }
            "enter" | "return" => {
                if let Some(hit) = self.machine.inner().selected_hit() {
                    self.activate_hit(hit, window, cx);
                }
            }
            // Every other key is already owned by the field itself: its caret
            // and edit keys arrive through the input's own key listener, so
            // forwarding them here would apply the same edit twice.
            _ => {}
        }
    }

    fn set_query(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.open {
            self.dispatch(
                SearchEvent::QueryChanged {
                    query: text.to_owned(),
                },
                cx,
            );
        }
    }

    /// Re-reads the input's live text instead of the snapshot captured when the
    /// change was queued. The input notifies from inside its own update, so the
    /// dispatch has to be deferred; by the time it runs the text may already be
    /// newer, and applying the old snapshot would drive the machine off a query
    /// the user never typed and drop results that are still current.
    fn sync_query_from_input(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().to_owned();
        if text.trim() != self.query() {
            self.set_query(&text, cx);
        }
    }
    fn select_index(&mut self, index: usize, cx: &mut Context<Self>) {
        self.dispatch(SearchEvent::Select { index }, cx);
        self.reveal_selection();
    }

    fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        let input_focus = self.input.read(cx).focus_handle(cx);
        if !input_focus.is_focused(window) {
            window.focus(&input_focus, cx);
        }
    }

    /// Re-runs a recent search. It uses the query path, so it is debounced and
    /// stale-guarded like a typed one.
    fn recall_recent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(query) = self
            .machine
            .inner()
            .recents()
            .get(self.recent_cursor)
            .cloned()
        else {
            return;
        };
        self.input
            .update(cx, |input, cx| input.set_text(query.clone(), window, cx));
        self.recent_cursor = 0;
        self.dispatch(SearchEvent::Recall { query }, cx);
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.phase() != SearchPhase::Error {
            return;
        }
        let query = self.query().to_owned();
        self.dispatch(SearchEvent::QueryChanged { query }, cx);
        let input_focus = self.input.read(cx).focus_handle(cx);
        window.focus(&input_focus, cx);
    }

    fn dispatch(&mut self, event: SearchEvent, cx: &mut Context<Self>) {
        self.machine.handle(&event);
        self.drain_effects(cx);
    }

    fn drain_effects(&mut self, cx: &mut Context<Self>) {
        while let Ok(effect) = self.effects.try_recv() {
            self.run_effect(effect, cx);
        }
    }

    fn run_effect(&mut self, effect: SearchEffect, cx: &mut Context<Self>) {
        match effect {
            SearchEffect::Cancel { request_id } => {
                self.debounce_epoch = self.debounce_epoch.wrapping_add(1);
                self.debounce_task = None;
                if self
                    .request
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                    && let Some(request) = self.request.take()
                    && let Some(cancel) = request.cancel
                {
                    let _ = cancel.send(());
                }
            }
            SearchEffect::Schedule {
                request_id,
                delay_ms,
            } => {
                self.debounce_epoch = self.debounce_epoch.wrapping_add(1);
                let epoch = self.debounce_epoch;
                self.debounce_task = Some(cx.spawn(async move |view, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(delay_ms))
                        .await;
                    view.update(cx, |view, cx| {
                        if view.debounce_epoch == epoch {
                            view.dispatch(SearchEvent::DebounceElapsed { request_id }, cx);
                        }
                    })
                    .ok();
                }));
            }
            SearchEffect::Query { request_id, query } => self.start_query(request_id, query, cx),
            SearchEffect::Notify => cx.notify(),
        }
    }

    fn start_query(&mut self, request_id: u64, query: String, cx: &mut Context<Self>) {
        let Some(executor) = self.executor.clone() else {
            self.dispatch(
                SearchEvent::Error {
                    request_id,
                    reason: SEARCH_UNAVAILABLE.to_owned(),
                },
                cx,
            );
            return;
        };
        if !executor.targets_ready() {
            self.request = None;
            cx.notify();
            return;
        }
        let (cancel, task) = executor.spawn(query);
        self.request = Some(ActiveRequest {
            request_id,
            cancel: Some(cancel),
        });
        let view = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let result = match task.await {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("k8s-gpui: resource search task failed: {error}");
                    Err(SEARCH_FAILED.to_owned())
                }
            };
            view.update(cx, |view, cx| {
                if view
                    .request
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    view.request = None;
                }
                let event = match result {
                    Ok(outcome) => SearchEvent::Results {
                        request_id,
                        hits: outcome.hits,
                        scanned: outcome.scanned,
                        truncated: outcome.truncated,
                        partial: outcome.partial,
                    },
                    Err(reason) => SearchEvent::Error { request_id, reason },
                };
                view.dispatch(event, cx);
            })
            .ok();
        })
        .detach();
    }

    fn reveal_selection(&self) {
        let row = self.display_order();
        if let Some(row) = self.selected_row(&row) {
            self.scroll.scroll_to_item(row);
        }
    }

    fn reveal_row(&self, row: usize) {
        self.scroll.scroll_to_item(row);
    }

    /// Opens a result and records the query, so the next empty search offers it.
    fn activate_hit(&mut self, hit: SearchHit, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query().to_owned();
        self.dispatch(SearchEvent::Committed { query }, cx);
        if let Some(handler) = self.on_open.clone() {
            handler(hit, window, cx);
        }
    }

    fn activate_hit_action(
        &mut self,
        hit: SearchHit,
        action: SearchResultAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.query().to_owned();
        self.dispatch(SearchEvent::Committed { query }, cx);
        if let Some(handler) = self.on_action.clone() {
            handler(hit, action, window, cx);
        }
    }
}

/// Row label for a recent search, spoken with its position in the list.
fn recent_description(recent: &str, index: usize, total: usize) -> String {
    format!("Recent search {recent}, {index} of {total}.")
}

/// The surface a result row is actually painted with.
///
/// gpui-kit's `ListItem` owns the selected fill, so the app reads the same theme
/// roles it does rather than keeping a second answer to "what colour is this
/// row". The match highlight solves its contrast against this value, so a change
/// to the theme's list selection moves the two together instead of leaving the
/// highlight's ink solved against a surface nothing paints.
fn row_surface(selected: bool, cx: &App) -> Hsla {
    if !selected {
        return design::role::surface_raised(cx);
    }
    if cx.theme().list.active_highlight {
        cx.theme().list_active
    } else {
        cx.theme().accent
    }
}

/// One result name, with every matched run highlighted.
///
/// The highlight comes from the shared search-match tokens, so it keeps its
/// contrast fallbacks on a selected row, and the plain and matched runs keep the
/// same type size so the name does not shift when the query changes.
fn highlighted_name(name: &str, query: &str, row: usize, selected: bool, cx: &App) -> AnyElement {
    let base = row_surface(selected, cx);
    let runs = {
        let mut ranker = Ranker::with_case(query, CasePolicy::Ignore);
        if ranker.score(name).is_some() {
            match_runs(name, ranker.ranges())
        } else {
            vec![MatchRun::Plain(name)]
        }
    };
    let mut matched_run = 0usize;
    h_flex()
        .min_w(px(0.0))
        .flex_1()
        .children(runs.into_iter().map(|run| {
            if !run.is_matched() {
                // `fg.primary`, to match the matched runs beside it.
                //
                // A result row is one name split into highlighted and plain runs, and the
                // highlight's ink is fixed by the search-match token — so the *plain* run
                // is what decides whether one name reads as one string or as two. Left on
                // `label_body`'s default it would be `fg.secondary` while the match beside
                // it was tinted, and the name would change weight halfway across. §1.4
                // also gives `fg.primary` to 对象名, and this is one.
                return common::label_body(run.text().to_owned())
                    .text_color(design::role::fg_primary(cx))
                    .truncate()
                    .into_any_element();
            }
            let index = matched_run;
            matched_run += 1;
            div()
                .id(format!("resource-search-match-{row}-{index}"))
                .debug_selector(move || format!("resource-search-match-{row}-{index}"))
                .flex_none()
                .rounded_sm()
                .bg(if selected {
                    design::search_match::active_background(cx)
                } else {
                    design::search_match::background(cx)
                })
                .text_color(design::search_match::foreground_on(cx, base, selected))
                // The matched run states its own type rather than inheriting the
                // row's, so a theme that renders a list row at a different size
                // cannot make the matched and plain halves of one name disagree.
                .text_size(design::text::BODY)
                .line_height(design::text::BODY_LINE_HEIGHT)
                .child(SharedString::from(run.text()))
                .into_any_element()
        }))
        .into_any_element()
}

/// A titled group of result rows.
///
/// The heading is drawn, not just announced: the group is the only thing that says
/// which kind a row is, and a reader scanning fifty rows across four kinds was
/// reading the names alone. It carries the kind for a screen reader too, so the
/// group is one thing with two readers rather than a visible label beside an
/// invisible one.
fn result_group(id: &str, label: &str, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    v_flex()
        .id(id.to_owned())
        .debug_selector(move || id.to_owned())
        .w_full()
        .flex_none()
        .role(Role::Group)
        .aria_label(label)
        .child(group_heading(id, label, cx))
        .children(rows)
        .into_any_element()
}

/// One footer item: what the key does, and this platform's name for the key.
struct SearchHint {
    verb: &'static str,
    key: String,
}

impl SearchHint {
    fn line(&self) -> String {
        format!("{}: {}", self.verb, self.key)
    }
}

/// Footer hints, one short item per key.
///
/// A single long line truncates at a narrow popup width, and a truncated hint is
/// a hint the user cannot read. Short items let the footer wrap, and every key
/// name comes from the platform, so the footer never advertises a key the reader
/// does not have.
fn footer_hints(actions: &[SearchResultAction]) -> Vec<SearchHint> {
    let command = command_modifier();
    let mut hints = vec![
        SearchHint {
            verb: "Navigate",
            key: "\u{2191}/\u{2193}".to_owned(),
        },
        SearchHint {
            verb: "Page",
            key: "PgUp/PgDn".to_owned(),
        },
        SearchHint {
            verb: "First",
            key: format!("{command}+Home"),
        },
        SearchHint {
            verb: "Last",
            key: format!("{command}+End"),
        },
    ];
    hints.extend(actions.iter().map(|action| SearchHint {
        verb: action.label(),
        key: action.keystrokes(),
    }));
    // Escape is listed once, as the one verb it really has here: it cancels an
    // in-progress IME composition, and otherwise it closes. The picker's footer
    // can list `Clear` and `Close` on the same key because its handler is two
    // steps, `clear_query` then `dismiss`; this panel clears through the `Clear
    // search` button and the field's own `×`, so a second verb for Escape was a
    // sentence the app could not act on.
    hints.push(SearchHint {
        verb: "Close",
        key: "Esc".to_owned(),
    });
    hints
}

/// How many things a search found.
///
/// `results` is the word every search in the app uses for this: the command
/// palette, the two switchers' group headers, and this status line. `matches`
/// was a fourth word for one job.
fn result_count(count: usize) -> String {
    design::format::count_with_noun(count, "result", "results")
}

/// How many resources a search read. An object count, so it keeps its noun.
fn scanned_count(count: usize) -> String {
    design::format::count_with_noun(count, "resource", "resources")
}

fn search_status(phase: SearchPhase, hits: usize, scanned: usize, truncated: bool) -> String {
    match phase {
        SearchPhase::Query => "Searching…".to_owned(),
        SearchPhase::Error => "Search failed".to_owned(),
        // The status line carries how much was read, so the state below it can
        // stay two short sentences instead of one that has to be truncated.
        SearchPhase::Results if hits == 0 && scanned > 0 => {
            format!("No results in {}", scanned_count(scanned))
        }
        SearchPhase::Results if hits == 0 => "No results".to_owned(),
        SearchPhase::Results if truncated => {
            format!(
                "{} shown, {} scanned",
                result_count(hits),
                scanned_count(scanned)
            )
        }
        SearchPhase::Results => format!("{} shown", result_count(hits)),
        SearchPhase::Cleared | SearchPhase::Closed => "Ready".to_owned(),
    }
}

fn search_limit_detail(scanned: usize) -> String {
    if scanned == 0 {
        "Search reached the resource limit before scanning resources.".to_owned()
    } else {
        format!("Search reached the limit after {}.", scanned_count(scanned))
    }
}

fn no_match_detail(scanned: usize) -> String {
    if scanned == 0 {
        "The search found no matching resources.".to_owned()
    } else {
        format!("The search checked {}.", scanned_count(scanned))
    }
}

fn error_title(reason: Option<&str>) -> &'static str {
    match reason {
        Some(SEARCH_UNAVAILABLE) => "Search unavailable",
        Some(SEARCH_NO_RESOURCES) => "No resource types",
        Some(SEARCH_UNAUTHORIZED) => "Authentication failed",
        Some(SEARCH_FORBIDDEN) => "Access denied",
        Some(SEARCH_TIMED_OUT) => "Search timed out",
        _ => "Search failed",
    }
}

/// The outcome a failure reports, in one short sentence.
fn error_detail(reason: Option<&str>) -> &'static str {
    match reason {
        Some(SEARCH_UNAVAILABLE) => SEARCH_UNAVAILABLE,
        Some(SEARCH_NO_RESOURCES) => SEARCH_NO_RESOURCES,
        Some(SEARCH_UNAUTHORIZED) => SEARCH_UNAUTHORIZED,
        Some(SEARCH_FORBIDDEN) => SEARCH_FORBIDDEN,
        Some(SEARCH_TIMED_OUT) => SEARCH_TIMED_OUT,
        _ => SEARCH_FAILED,
    }
}

/// What to do about a failure. Every repair is one sentence the reader can act
/// on without leaving the dialog.
fn error_repair(reason: Option<&str>) -> &'static str {
    match reason {
        Some(SEARCH_UNAVAILABLE) => "Check the cluster connection, then retry the search.",
        Some(SEARCH_NO_RESOURCES) => "Refresh resource discovery, then retry the search.",
        Some(SEARCH_UNAUTHORIZED) => "Refresh the cluster credentials, then retry the search.",
        Some(SEARCH_FORBIDDEN) => "Grant list permission for this cluster, then retry.",
        Some(SEARCH_TIMED_OUT) => "Check the cluster connection, then retry the search.",
        _ => "Check the cluster connection and resource permissions, then retry.",
    }
}

fn namespace_label(hit: &SearchHit) -> String {
    if hit.object.metadata.namespace.is_some() {
        hit.object
            .metadata
            .namespace
            .clone()
            .unwrap_or_else(|| "Unavailable".to_owned())
    } else {
        "Cluster".to_owned()
    }
}

fn result_description(hit: &SearchHit, cluster: &str) -> String {
    let kind = &hit.resource.kind;
    let name = hit.object.metadata.name.as_deref().unwrap_or("Unnamed");
    if hit.object.metadata.namespace.is_some() {
        match hit.object.metadata.namespace.as_deref() {
            Some(namespace) => format!("{kind} {name} in namespace {namespace} on {cluster}"),
            None => format!("{kind} {name} without a namespace on {cluster}"),
        }
    } else {
        format!("{kind} {name} on {cluster}")
    }
}

fn cluster_label(executor: Option<&SearchExecutor>) -> String {
    executor
        .and_then(SearchExecutor::cluster_name)
        .unwrap_or("Cluster unavailable")
        .to_owned()
}

struct SearchStateContext<'a> {
    query: &'a str,
    truncated: bool,
    catalog_waiting: bool,
    scanned: usize,
    error: Option<&'a str>,
    /// The control that runs the repair, when the state has one.
    action: Option<AnyElement>,
}

/// One of the two lines under a state's title.
///
/// `DESIGN.md` §4 splits the copy into "what happened" and "what to do next", so
/// the split has to be typographic. Both lines used the same 11px muted label
/// with `space::SM` between them, which put the two grey lines at the same
/// weight and said nothing about which one to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateLine {
    Detail,
    NextStep,
}

impl StateLine {
    /// The size this line is drawn at, and whether it is drawn in the secondary
    /// ink, as a design type role.
    fn type_role(self) -> (Pixels, bool) {
        match self {
            // What happened: secondary, so it stays behind the instruction.
            Self::Detail => (design::text::CAPTION, true),
            // What to do next: the line the reader acts on, so it is body text.
            Self::NextStep => (design::text::BODY, false),
        }
    }

    /// The line, at the role it is drawn with.
    fn label(self, text: impl Into<SharedString>, secondary: Hsla, primary: Hsla) -> Label {
        let (size, quiet) = self.type_role();
        Label::new(text)
            .text_size(size)
            .line_height(if quiet {
                design::text::CAPTION_LINE_HEIGHT
            } else {
                design::text::BODY_LINE_HEIGHT
            })
            .text_color(if quiet { secondary } else { primary })
    }
}

/// The one empty state, drawn to the shape the rest of the app draws.
///
/// `UI-SPEC` §4.13 asks for a 24px `fg.tertiary` glyph and says **不要用彩色大图标**,
/// and `common::empty_state_with_action` gives that reason at length: on a card
/// with nothing else on it, a coloured 24px glyph is the loudest thing a failure
/// state can be, and a failure is not the loudest thing on a card — the field above
/// it and the scope under it are. So every state here, including the failure and
/// the two in-flight ones, wears the same quiet mark, and the only difference
/// between them is what they say and the control they offer.
fn search_state(phase: SearchPhase, context: SearchStateContext<'_>, cx: &App) -> AnyElement {
    let SearchStateContext {
        query,
        truncated,
        catalog_waiting,
        scanned,
        error,
        action,
    } = context;
    // The scope and the cluster already sit on their own line under the field, so
    // the state says what happened and what to do, and nothing else. Both lines
    // are short enough to read at the popup width; the single sentence they used
    // to be could only be truncated.
    let (icon, title, detail, next_step, role) =
        if catalog_waiting && matches!(phase, SearchPhase::Query | SearchPhase::Cleared) {
            (
                IconName::LoaderCircle,
                "Loading resource types",
                "The resource list is still loading.".to_owned(),
                Some("Searching starts as soon as it is ready.".to_owned()),
                Role::Status,
            )
        } else {
            match phase {
                SearchPhase::Query => (
                    IconName::LoaderCircle,
                    "Searching…",
                    "Searching resource names.".to_owned(),
                    None,
                    Role::Region,
                ),
                SearchPhase::Error => (
                    IconName::TriangleAlert,
                    error_title(error),
                    error_detail(error).to_owned(),
                    Some(error_repair(error).to_owned()),
                    Role::Alert,
                ),
                SearchPhase::Results => (
                    IconName::Search,
                    "No matching resources",
                    if truncated {
                        search_limit_detail(scanned)
                    } else if query.is_empty() {
                        no_match_detail(scanned)
                    } else {
                        format!("No resources match \"{query}\".")
                    },
                    Some(if truncated {
                        SEARCH_LIMIT_NEXT_STEP.to_owned()
                    } else {
                        NO_MATCH_NEXT_STEP.to_owned()
                    }),
                    Role::Region,
                ),
                SearchPhase::Cleared | SearchPhase::Closed => (
                    IconName::Search,
                    // The card's own title is "Search resources" and the field's
                    // placeholder is "Search resource names…". A third copy of
                    // the same string, 136 logical px lower, said nothing. This
                    // is the one state whose title can add a fact: there is
                    // nothing to search *yet*.
                    SEARCH_IDLE_STATE_TITLE,
                    "Enter a resource name.".to_owned(),
                    Some("Results cover every listable kind.".to_owned()),
                    Role::Region,
                ),
            }
        };
    let description = match &next_step {
        Some(next_step) => format!("{detail} {next_step}"),
        None => detail.clone(),
    };
    let waiting = icon == IconName::LoaderCircle;
    let secondary = design::role::fg_secondary(cx);
    let primary = design::role::fg_primary(cx);
    let icon_size = Size::Size(design::size::ICON_LARGE);
    let icon: AnyElement = if waiting {
        // The one place a glyph here has to turn: two states that look alike
        // while the panel is still reading the cluster and is still waiting on
        // the API. `common::spinner` owns the sweep and stops it when the reader
        // asked for less motion.
        common::spinner(icon, design::role::fg_tertiary(cx), icon_size)
    } else {
        Icon::new(icon)
            .with_size(icon_size)
            .text_color(design::role::fg_tertiary(cx))
            .into_any_element()
    };
    // gpui-kit's `Empty` owns the media/title/description shape. The two lines
    // keep their own type roles, so the description is a column of two labels
    // rather than one string: the split between what happened and what to do is
    // the reason the state has two paragraphs at all.
    let mut lines = v_flex()
        .w_full()
        .min_w(px(0.0))
        .gap(design::space::XS)
        .child(StateLine::Detail.label(detail, secondary, primary));
    if let Some(next_step) = next_step {
        lines = lines.child(StateLine::NextStep.label(next_step, secondary, primary));
    }
    let state = Empty::new()
        // The dashed frame an `Empty` draws is a page-level cue. Inside a modal
        // card the card is already the frame, and a second one nested inside it
        // reads as a hole cut in the dialog.
        .border_0()
        .p_0()
        .rounded(design::radius::LG)
        // `Empty` grows to whatever height it is given and centres its own
        // children, which would push the repair control to the bottom of the
        // card instead of under the sentence it belongs to. The wrapper below
        // does the centring, so the group is `flex_none`.
        .flex_none()
        .gap(design::space::SM)
        // `Empty` re-applies `theme().foreground` after inheriting, so the ink
        // this card's copy is printed in is stated here rather than left to the
        // component.
        .text_color(primary)
        .header(
            EmptyHeader::new()
                .gap(design::space::SM)
                .max_w(px(empty_measure()))
                .media(
                    // The unframed slot, always, and `mb_0` with it — the same two
                    // choices `common::empty_state_with_action` makes, for the same two
                    // reasons, so the two empty-state shapes in the app are one shape.
                    //
                    // `EmptyMediaVariant::Icon` is a `size_8()` `bg(muted)` frame, and
                    // §4.13 asks for "24px fg.tertiary（**不要用彩色大图标**）" — a
                    // framed plate is the coloured-large-icon rule wearing a different
                    // hat, and it had this state alone: four other empty states in the
                    // app draw the bare glyph. `EmptyMedia`'s own `mb_2()` then added
                    // 8px to the header's 8px gap, so the glyph sat 16px from the title
                    // while every other empty state in the app had it at 8 — a sum that
                    // is on the spacing scale and still not a step.
                    EmptyMedia::new()
                        .with_variant(EmptyMediaVariant::Default)
                        .mb_0()
                        .child(icon),
                )
                .title(
                    EmptyTitle::new()
                        .text_size(design::text::TITLE)
                        .line_height(design::text::TITLE_LINE_HEIGHT)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(primary)
                        .child(title),
                )
                .description(EmptyDescription::new().child(lines)),
        )
        // Real waiting, and an indeterminate bar rather than decorative dots:
        // these two states are the only ones where the panel is waiting on the
        // API server, so they are the only two where a moving mark means
        // anything, and they are the only two places in this card that spend the
        // accent. Same bar, same token, same width as
        // `common::empty_state_with_action`, so the app has one loading treatment.
        .when(waiting, |this| {
            this.child(
                Progress::new("resource-search-loading")
                    .loading(true)
                    .color(design::role::accent(cx))
                    .w(design::size::UPDATE_PROGRESS)
                    .max_w_full()
                    .h(design::space::XS),
            )
        });
    v_flex()
        .id("resource-search-state")
        .size_full()
        .min_h(px(0.0))
        .items_center()
        .justify_center()
        .gap(design::space::SM)
        .px(design::space::XL)
        .role(role)
        .aria_label(title)
        .aria_description(description)
        .child(state)
        .when_some(action, |this, action| this.child(action))
        .into_any_element()
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let container_width = self.container_bounds.map_or_else(
            || f32::from(viewport.width),
            |bounds| f32::from(bounds.size.width),
        );
        let container_height = self.container_bounds.map_or_else(
            || f32::from(viewport.height),
            |bounds| f32::from(bounds.size.height),
        );
        let popup_width = search_width(container_width);
        let popup_max_height = search_max_height(container_height);
        let phase = self.phase();
        let query = self.query().to_owned();
        let cluster = cluster_label(self.executor.as_ref());
        let scope = "All namespaces and cluster-scoped resources";
        let catalog_waiting = self.catalog_waiting();
        let error = self.machine.state().failure_reason().map(str::to_owned);
        let hits = self.hits().to_vec();
        let selected = self.selected_index();
        let scanned = self.scanned();
        let truncated = self.truncated();
        let partial = self.partial();
        let shows_recents = self.shows_recents();
        let close = self.on_close.clone();
        let input = self.input.clone();
        let title = SEARCH_CARD_TITLE;
        let dialog_label = format!("{title}. Cluster: {cluster}. Scope: {scope}.");
        // The scope gets a line of its own under the field. Folded into the title
        // with middle dots it was a breadcrumb that repeated the toolbar behind
        // the dialog and could only be truncated.
        let scope_line = format!("{scope} on {cluster}");
        let status = (phase != SearchPhase::Error).then(|| {
            if catalog_waiting {
                "Loading resource types".to_owned()
            } else {
                search_status(phase, hits.len(), scanned, truncated)
            }
        });
        let action: Option<AnyElement> = if phase == SearchPhase::Error {
            // `Retry` is the one commit this state offers and the Enter the dialog
            // answers is the shell's, so the filled button is the right weight here
            // — but it is the *only* filled button in the card, and its wrapper is
            // what carries the focus handle and the card's one ring.
            Some(
                div()
                    .id("resource-search-retry")
                    .debug_selector(|| "resource-search-retry".to_owned())
                    .flex_none()
                    .rounded(design::radius::MD)
                    .role(Role::Button)
                    .aria_label("Retry Resource Search")
                    .tab_index(4isize)
                    .track_focus(&self.retry_focus)
                    .focus_visible(common::focus_ring(cx))
                    .child(
                        Button::new("resource-search-retry-button")
                            .label("Retry")
                            .primary()
                            .with_size(Size::Medium)
                            // One ring, drawn by the wrapper that owns the focus
                            // handle. Left on, the component's own ring would answer
                            // a handle this panel never focuses, and the two would
                            // describe focus differently if it ever did.
                            .focus_ring(false)
                            .role(RoleOverride::Presentational)
                            .tab_stop(false)
                            .on_click(cx.listener(|view, _, window, cx| view.retry(window, cx))),
                    )
                    .into_any_element(),
            )
        } else {
            // The no-match state is the one state with a repair the panel can run
            // itself, so it offers it as a control instead of describing it.
            self.state_clear_focus().map(|focus| {
                div()
                    .id("resource-search-clear")
                    .debug_selector(|| "resource-search-clear".to_owned())
                    .flex_none()
                    .rounded(design::radius::MD)
                    .role(Role::Button)
                    .aria_label("Clear Resource Search")
                    .tab_index(3isize)
                    .track_focus(&focus)
                    .focus_visible(common::focus_ring(cx))
                    .child(
                        Button::new("resource-search-clear-button")
                            .label("Clear search")
                            .ghost()
                            .with_size(Size::Medium)
                            .focus_ring(false)
                            .role(RoleOverride::Presentational)
                            .tab_stop(false)
                            .on_click(
                                cx.listener(|view, _, window, cx| view.clear_query(window, cx)),
                            ),
                    )
                    .into_any_element()
            })
        };
        let order = self.display_order();
        let recents = if shows_recents {
            self.machine.inner().recents().to_vec()
        } else {
            Vec::new()
        };
        let recent_cursor = self.recent_cursor;
        let hit_rows = order
            .iter()
            .enumerate()
            .map(|(row, index)| {
                let hit = &hits[*index];
                let name = hit
                    .object
                    .metadata
                    .name
                    .clone()
                    .unwrap_or_else(|| "Unnamed".to_owned());
                let namespace = namespace_label(hit);
                let description = result_description(hit, &cluster);
                let row_hit = hit.clone();
                let is_selected = selected == Some(*index);
                // `ListItem` is the row's own widget: it owns the selected fill,
                // the hover wash and the hit target. The app supplies one child —
                // the row's content — because a list item lays its children out
                // in a block, and this row is three columns beside each other.
                //
                // `size::PALETTE_ROW` rather than `size::ROW`: these are menu rows,
                // and they are the same number as the table's default row, so the
                // two lists in the app are the same height for the same reason.
                let mut row = ListItem::new(("resource-search-result", *index))
                    .debug_selector(move || format!("resource-search-result-{index}"))
                    .role(Role::ListBoxOption)
                    .aria_label(description)
                    .aria_selected(is_selected)
                    .aria_position_in_set(row + 1)
                    .aria_size_of_set(hits.len())
                    .selected(is_selected)
                    .w_full()
                    .h(design::size::PALETTE_ROW)
                    .flex_none()
                    .px(design::space::MD)
                    .py_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, window, cx| {
                            let focus = view.input.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                        }),
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.activate_hit(row_hit.clone(), window, cx);
                    }))
                    .child(result_lanes(
                        // The kind's own glyph, in the shared icon slot, so a
                        // column of fifty rows can be scanned by shape before it
                        // is read. The group heading above says which kind exactly;
                        // `design::kind_icon` is the design system's answer to
                        // "which category", which is the right question for a row
                        // that belongs to a group rather than for one that *is*
                        // the kind.
                        Icon::new(design::kind_icon(&hit.resource.kind))
                            .with_size(Size::Size(ICON_SLOT))
                            .text_color(design::role::fg_tertiary(cx))
                            .into_any_element(),
                        common::label_metadata(namespace)
                            .text_color(design::role::fg_secondary(cx))
                            .truncate()
                            .into_any_element(),
                        highlighted_name(&name, &query, row, is_selected, cx),
                    ));
                if is_selected {
                    row = row.aria_active_descendant();
                }
                (hit.resource.kind.clone(), row.into_any_element())
            })
            .collect::<Vec<_>>();
        let mut hit_groups: Vec<AnyElement> = Vec::new();
        let mut group_rows: Vec<AnyElement> = Vec::new();
        let mut group_kind: Option<String> = None;
        for (kind, element) in hit_rows {
            if group_kind.as_deref() != Some(kind.as_str()) {
                if let Some(kind) = group_kind.take() {
                    hit_groups.push(result_group(
                        &format!("resource-search-group-{kind}"),
                        &kind,
                        std::mem::take(&mut group_rows),
                        cx,
                    ));
                }
                group_kind = Some(kind);
            }
            group_rows.push(element);
        }
        if let Some(kind) = group_kind.take() {
            hit_groups.push(result_group(
                &format!("resource-search-group-{kind}"),
                &kind,
                group_rows,
                cx,
            ));
        }
        let recent_group = (!recents.is_empty()).then(|| {
            let rows = recents
                .iter()
                .enumerate()
                .map(|(index, recent)| {
                    let is_selected = index == recent_cursor;
                    let description = recent_description(recent, index + 1, recents.len());
                    let query = recent.clone();
                    let mut row = ListItem::new(("resource-search-recent", index))
                        .debug_selector(move || format!("resource-search-recent-{index}"))
                        .role(Role::ListBoxOption)
                        .aria_label(description)
                        .aria_selected(is_selected)
                        .aria_position_in_set(index + 1)
                        .aria_size_of_set(recents.len())
                        .selected(is_selected)
                        .w_full()
                        .h(design::size::PALETTE_ROW)
                        .flex_none()
                        .px(design::space::MD)
                        .py_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _, window, cx| {
                                let focus = view.input.read(cx).focus_handle(cx);
                                window.focus(&focus, cx);
                            }),
                        )
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.recent_cursor = index;
                            view.recall_recent(window, cx);
                        }))
                        // The same three lanes as a result row, with the namespace
                        // lane left empty: a recent search has no namespace, and
                        // that is a reason to reserve the column rather than to
                        // drop it. The names line up either way.
                        .child(result_lanes(
                            Icon::new(IconName::Search)
                                .with_size(Size::Size(ICON_SLOT))
                                .text_color(design::role::fg_tertiary(cx))
                                .into_any_element(),
                            div().into_any_element(),
                            // The recent query is a resource name — the same kind of string the
                            // rows above draw with `fg.primary`, in the same list. Left on
                            // `label_body`'s default it would be the only name in the
                            // popover one step quieter than every other, and the
                            // difference would read as "these are not resources yet".
                            common::label_body(query)
                                .text_color(design::role::fg_primary(cx))
                                .truncate()
                                .into_any_element(),
                        ));
                    if is_selected {
                        row = row.aria_active_descendant();
                    }
                    row.into_any_element()
                })
                .collect();
            result_group(
                "resource-search-recent-group",
                SEARCH_RECENT_GROUP,
                rows,
                cx,
            )
        });
        let list_rows = recent_group
            .into_iter()
            .chain(hit_groups)
            .collect::<Vec<_>>();
        let has_rows = !list_rows.is_empty();
        let result_header = (!hits.is_empty()).then(|| {
            h_flex()
                .id("resource-search-result-header")
                .flex_none()
                .h(design::size::PALETTE_ROW)
                .px(design::space::MD)
                .gap(design::space::SM)
                .items_center()
                // The column header owns this rule: it is the one boundary between
                // "what the columns are" and "what is in them", and a second one
                // under the first row would be a card inside the card.
                .border_b_1()
                .border_color(design::role::border_subtle(cx))
                // The same three lanes the rows use, including the icon slot the
                // rows leave empty for the glyph they all have. A header that
                // started its first column two lanes to the left of the rows'
                // first column was a header for a different table.
                .child(div().w(ICON_SLOT).flex_none())
                .child(
                    div()
                        .w(gpui_kit::relative(NAMESPACE_LANE))
                        .min_w(px(0.0))
                        .flex_none()
                        .child(
                            common::label_metadata("Namespace")
                                .text_color(design::role::fg_tertiary(cx)),
                        ),
                )
                .child(div().min_w(px(0.0)).flex_1().child(
                    common::label_metadata("Name").text_color(design::role::fg_tertiary(cx)),
                ))
        });

        // The overlay is the one place this product spends a transition, and this
        // is it: the scrim and the card arrive together over `motion::FAST`, and
        // nothing inside the card moves — a result set that animated itself in
        // would be the web-page tell that `motion`'s own rule exists to prevent.
        //
        // `with_animation` returns the finished frame the moment
        // `App::reduce_motion` is set, so the setting is honoured here without a
        // branch, and an opacity carries no claim about where anything is — a
        // slide across the screen would, and would have to be honoured by hand.
        div()
            .id("resource-search-overlay")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .size_full()
            .occlude()
            // The scrim states the modality, and it is the shared token composed
            // to half its ink rather than a fixed alpha, so the dimming reads the
            // same in both appearances and the app behind it stays legible. See
            // `SCRIM_INK` for why a search, which is dismissed straight back into
            // the session it came from, keeps more of its context than a dialog
            // that owns the window does.
            .bg(scrim(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _, window, cx| {
                    if let Some(handler) = &close {
                        handler(window, cx);
                    }
                }),
            )
            .flex()
            .justify_center()
            .child(
                v_flex()
                    .id("resource-search")
                    .debug_selector(|| "resource-search".to_owned())
                    .mt(design::space::XL)
                    .w(px(popup_width))
                    .min_w(px(0.0))
                    .max_h(px(popup_max_height))
                    .flex_none()
                    // A dialog is the rounder of the two floating surfaces:
                    // `radius::XL` is the token this product gives a palette and a
                    // dialog, and `radius::LG` is the popover's. The card is the
                    // most separate thing on screen, and a separate thing is the
                    // one that gets the softer corner.
                    .rounded(design::radius::XL)
                    .border_1()
                    .border_color(design::role::border_base(cx))
                    .bg(design::role::surface_raised(cx).alpha(1.0))
                    // The overlay shadow, read from the token layer rather than
                    // spelled out here. A dialog is one of the five things the
                    // design lets carry a shadow, and it is the reason the other
                    // four are on the list — which is only true if the value is
                    // one value.
                    .shadow(design::shadow::overlay(cx))
                    .overflow_hidden()
                    .role(Role::Dialog)
                    .tab_group()
                    .key_context("ResourceSearch")
                    .aria_label(dialog_label)
                    .aria_keyshortcuts(SEARCH_KEYSHORTCUTS)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .id("resource-search-header")
                            .debug_selector(|| "resource-search-header".to_owned())
                            .flex_none()
                            .h(design::size::ROW)
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .px(design::space::MD)
                            .gap(design::space::SM)
                            .items_center()
                            // The card's own mark, in the mark ink: the same
                            // `fg.tertiary` every glyph in this card is drawn in, so
                            // one icon treatment rather than a louder one where the
                            // row happens to meet it.
                            .child(
                                Icon::new(IconName::Search)
                                    .xsmall()
                                    .text_color(design::role::fg_tertiary(cx)),
                            )
                            .child(
                                h_flex()
                                    .debug_selector(|| "resource-search-title".to_owned())
                                    .min_w(px(0.0))
                                    .max_w(gpui_kit::relative(0.35))
                                    .flex_shrink_1()
                                    // The truncation is load-bearing: this is a modal header in a
                                    // 560px card, and a title that wraps pushes the close control
                                    // out of the card instead of naming what the card is.
                                    // A modal header names the surface, so it is
                                    // `fg.primary` — which is what `label_panel_title` already
                                    // carries. Named here because this panel states its ink at
                                    // every `label_*` it draws; the shared default is a floor
                                    // that keeps a forgotten call site quiet, not a decision.
                                    .child(
                                        common::label_panel_title(title)
                                            .text_color(design::role::fg_primary(cx))
                                            .truncate(),
                                    ),
                            )
                            .child(h_flex().min_w(px(0.0)).flex_1())
                            // The count is a trailing lane of its own, not a
                            // second word on the title's baseline. The title names
                            // the surface at the panel-title role in the primary ink
                            // and the count is a caption in the tertiary ink; put
                            // side by side on one line they read as a single run-on
                            // string ("Search resources 2 results shown") and the
                            // reader has to work out which half is the name. The
                            // spacer between them is what separates them, and it is
                            // the trailing edge that aligns the count in every
                            // width rather than the width of the sentence above it.
                            .when_some(status, |this, status| {
                                this.child(
                                    h_flex()
                                        .id("resource-search-status")
                                        .debug_selector(|| "resource-search-status".to_owned())
                                        .min_w(px(0.0))
                                        .max_w(gpui_kit::relative(0.45))
                                        .flex_shrink_1()
                                        .role(Role::Status)
                                        .aria_label(status.clone())
                                        .child(
                                            div()
                                                .min_w(px(0.0))
                                                .flex_1()
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                // The intent, stated here: the wrapper's
                                                // `text_color` is the one the author wrote
                                                // down, and it never reached the glyph,
                                                // because gpui-kit's `Label` re-applies
                                                // `theme().foreground` after inheriting
                                                // (see `common::RoleLabel`). A `caption`
                                                // that reports "12 shown" is a count, and
                                                // `UI-SPEC` §1.4 gives 计数 to
                                                // `fg.tertiary`; the error state does not
                                                // come through here at all — it is
                                                // suppressed and drawn in the body as the
                                                // `danger` state, so no line in this
                                                // header ever has to carry a failure.
                                                .child(
                                                    common::label_metadata(status)
                                                        .text_color(design::role::fg_tertiary(cx)),
                                                ),
                                        ),
                                )
                            })
                            .when_some(self.on_close.clone(), |this, handler| {
                                this.child(
                                    // The wrapper is the control: the focus stop, the
                                    // role, the name, and the card's one ring. The
                                    // button inside it is the appearance and the
                                    // pointer target, so one action is one control in
                                    // the accessibility tree and a click lands where
                                    // the focus is. See `focus_ring` for why the
                                    // ring lives here rather than on the component.
                                    div()
                                        .id("resource-search-close")
                                        .flex_none()
                                        .rounded(design::radius::MD)
                                        .role(Role::Button)
                                        .aria_label("Close Resource Search")
                                        .tab_index(2isize)
                                        .track_focus(&self.close_focus)
                                        .focus_visible(common::focus_ring(cx))
                                        .child(
                                            common::reusable_icon_button(
                                                "resource-search-close-button",
                                                IconName::X,
                                                "Close Resource Search",
                                            )
                                            .focus_ring(false)
                                            .role(RoleOverride::Presentational)
                                            .tab_stop(false)
                                            .tooltip("Close Resource Search")
                                            .on_click(
                                                cx.listener(move |_, _, window, cx| {
                                                    handler(window, cx)
                                                }),
                                            ),
                                        ),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .id("resource-search-input-row")
                            .flex_none()
                            .min_w(px(0.0))
                            .px(design::space::MD)
                            .gap(design::space::SM)
                            .items_center()
                            .child(input),
                    )
                    // The scope states what the search covers, on a line of its own
                    // where it can wrap instead of truncating.
                    .child(
                        div()
                            .id("resource-search-scope")
                            .debug_selector(|| "resource-search-scope".to_owned())
                            .flex_none()
                            .w_full()
                            .min_w(px(0.0))
                            .px(design::space::MD)
                            .pb(design::space::SM)
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    // Also the author's stated intent, and also swallowed
                                    // by `Label`. `fg.secondary` here and not
                                    // `fg.tertiary`: the scope is a sentence about what the
                                    // search covers, not a count or a group head, and §1.4
                                    // gives 正文 to `fg.secondary`. It sits directly under
                                    // the field it qualifies, so the two read as one
                                    // control and its sentence has to hold the same weight
                                    // as the placeholder it is a second line of.
                                    .child(
                                        common::label_metadata(scope_line)
                                            .text_color(design::role::fg_secondary(cx)),
                                    ),
                            ),
                    )
                    .child(
                        // The results stay the app's own scroll container rather
                        // than gpui-kit's `List`. `ListState` owns the selection
                        // and answers its own keys, and this list's selection is
                        // the search machine's: the same index drives exec, port
                        // forward, the recents cursor, and a wrapping pager, all
                        // of which are dispatched from the machine rather than
                        // from a list. A second owner of the selection would be a
                        // second answer to "which result is selected". The rows
                        // themselves are gpui-kit's `ListItem`, which is the part
                        // that draws a row.
                        div()
                            .id("resource-search-results")
                            .debug_selector(|| "resource-search-results".to_owned())
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .role(Role::ListBox)
                            .aria_label("Resource search results")
                            .aria_keyshortcuts(
                                "ArrowUp ArrowDown Home End PageUp PageDown Enter Control+Enter \
                                 Control+Shift+Enter",
                            )
                            .when_some(result_header, |this, header| this.child(header))
                            .when(!has_rows, |this| {
                                this.child(search_state(
                                    phase,
                                    SearchStateContext {
                                        query: &query,
                                        truncated,
                                        catalog_waiting,
                                        scanned,
                                        error: error.as_deref(),
                                        action,
                                    },
                                    cx,
                                ))
                            })
                            .when(has_rows, |this| this.children(list_rows))
                            .when(truncated && has_rows, |this| {
                                this.child(
                                    Alert::warning(
                                        "resource-search-limit-notice",
                                        search_limit_detail(scanned),
                                    )
                                    .with_size(Size::XSmall),
                                )
                            })
                            .when(partial, |this| {
                                // Incomplete results are a statement about what the
                                // app could not read, not a fault in the results it
                                // did read, so the notice keeps the neutral variant
                                // and the confidence marker rather than a status hue.
                                // A warning colour here would read as "these
                                // resources are unhealthy".
                                this.child(
                                    Alert::new(
                                        "resource-search-partial-notice",
                                        PARTIAL_RESULTS_NOTICE,
                                    )
                                    .icon(design::confidence::icon(Confidence::Unknown))
                                    .with_size(Size::XSmall),
                                )
                            }),
                    )
                    .child(
                        // The hints are help, not data, and this strip is the one
                        // place in the card that must not compete with the results
                        // above it. Three things say so: one hairline instead of a
                        // plane (a second plane inside the card is a card inside the
                        // card), `text::MICRO` verbs in the tertiary ink rather than
                        // the caption size the rows' own metadata uses, and a chord
                        // drawn as a hairline cap instead of set in the same type as
                        // the verb beside it. One short item per key with a gap
                        // between them, so the strip wraps at the popup width rather
                        // than truncating a run-on line.
                        h_flex()
                            .id("resource-search-footer")
                            .debug_selector(|| "resource-search-footer".to_owned())
                            .flex_none()
                            .w_full()
                            .min_w(px(0.0))
                            .px(design::space::MD)
                            .py(design::space::XS)
                            .gap(design::space::MD)
                            .flex_wrap()
                            .items_center()
                            .border_t_1()
                            .border_color(design::role::border_subtle(cx))
                            .children(
                                footer_hints(&self.footer_actions())
                                    .iter()
                                    .map(|hint| search_hint(hint, cx)),
                            ),
                    ),
            )
            .with_animation(
                "resource-search-appearance",
                Animation::new(design::motion::FAST),
                move |overlay, progress| overlay.opacity(progress),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::{Keystroke, Modifiers, TestAppContext, point, size};
    use k8s_core::controller::SEARCH_LIST_LIMIT;
    use k8s_core::machines::SEARCH_RESULT_LIMIT;

    use super::*;

    /// `gpui_kit::init` runs once for the process, so a test only has to make
    /// sure the global state the panel reads is the one it expects. The theme
    /// bridge parses the product theme on demand, so there is nothing to seed.
    /// Installs the component library the search card renders through.
    ///
    /// It used to take `_cx` and do nothing, which left every test in this module
    /// rendering without a theme and panicking in `cx.theme()`.
    fn init_app(cx: &mut TestAppContext) {
        crate::init_ui(cx);
    }

    fn hit(name: &str) -> SearchHit {
        hit_in_namespace(name, Some("default"))
    }

    fn pod_resource() -> k8s_core::discovery::ResourceEntry {
        k8s_core::discovery::ResourceEntry {
            group: String::new(),
            version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            plural: "pods".to_owned(),
            scope: k8s_core::discovery::ResourceScope::Namespaced,
            verbs: vec!["list".to_owned()],
        }
    }

    fn hit_in_namespace(name: &str, namespace: Option<&str>) -> SearchHit {
        let namespace = namespace.map_or(serde_json::Value::Null, |namespace| {
            serde_json::Value::String(namespace.to_owned())
        });
        SearchHit {
            resource: pod_resource(),
            object: Arc::new(
                serde_json::from_value(serde_json::json!({
                    "apiVersion": "v1",
                    "kind": "Pod",
                    "metadata": {
                        "name": name,
                        "namespace": namespace,
                        "uid": format!("uid-{name}"),
                    },
                }))
                .expect("valid pod"),
            ),
        }
    }

    fn node_hit(name: &str) -> SearchHit {
        let mut hit = hit_in_namespace(name, None);
        hit.resource = k8s_core::discovery::ResourceEntry {
            group: String::new(),
            version: "v1".to_owned(),
            kind: "Node".to_owned(),
            plural: "nodes".to_owned(),
            scope: k8s_core::discovery::ResourceScope::Cluster,
            verbs: vec!["list".to_owned()],
        };
        hit
    }

    fn keystroke(key: &str) -> Keystroke {
        Keystroke {
            modifiers: Default::default(),
            key: key.to_owned(),
            key_char: None,
        }
    }

    fn command_keystroke(key: &str, platform: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control: !platform,
                platform,
                ..Modifiers::none()
            },
            key: key.to_owned(),
            key_char: None,
        }
    }

    /// The limit notice is one short sentence on its own line, with the repair
    /// on the next one. It used to be a single sentence that carried both and had
    /// to be truncated to fit the popup.
    #[test]
    fn search_limit_notice_reports_scanned_resources_without_calling_them_total() {
        let detail = search_limit_detail(SEARCH_LIST_LIMIT);
        assert!(detail.contains(&design::format::count(SEARCH_LIST_LIMIT)));
        assert!(!detail.contains(SEARCH_LIMIT_NEXT_STEP));
        assert!(!detail.contains("full resource name"));
        assert_eq!(
            search_limit_detail(0),
            "Search reached the resource limit before scanning resources."
        );
        assert!(SEARCH_LIMIT_NEXT_STEP.contains("more specific"));
    }

    #[test]
    fn status_helpers_distinguish_matches_from_scanned_resources() {
        assert_eq!(search_status(SearchPhase::Query, 0, 0, false), "Searching…");
        assert_eq!(search_status(SearchPhase::Cleared, 0, 0, false), "Ready");
        assert_eq!(
            search_status(SearchPhase::Results, 0, 0, false),
            "No results"
        );
        assert_eq!(
            search_status(SearchPhase::Results, 0, 12, false),
            "No results in 12 resources",
            "the status line carries how much was read"
        );
        assert_eq!(
            search_status(SearchPhase::Results, 2, 12, false),
            "2 results shown"
        );
        assert_eq!(
            search_status(SearchPhase::Results, 2, 5000, true),
            "2 results shown, 5,000 resources scanned"
        );
        assert_eq!(
            search_status(
                SearchPhase::Results,
                SEARCH_RESULT_LIMIT,
                SEARCH_LIST_LIMIT,
                true,
            ),
            "50 results shown, 5,000 resources scanned"
        );
        assert_eq!(
            search_status(SearchPhase::Results, 1, 1, false),
            "1 result shown",
            "a count of one is singular, everywhere"
        );
        assert_eq!(
            no_match_detail(0),
            "The search found no matching resources."
        );
        assert_eq!(no_match_detail(12), "The search checked 12 resources.");
        assert_eq!(no_match_detail(1), "The search checked 1 resource.");
        assert!(NO_MATCH_NEXT_STEP.starts_with("Try another name"));
        assert!(!NO_MATCH_NEXT_STEP.contains("cluster access"));
        let node = node_hit("node-a");
        let description = result_description(&node, "alpha");
        assert_eq!(description, "Node node-a on alpha");
        assert!(!description.contains("in namespace"));
        assert_eq!(namespace_label(&node), "Cluster");
        let web = hit_in_namespace("web", Some("team-a"));
        assert_eq!(
            result_description(&web, "alpha"),
            "Pod web in namespace team-a on alpha"
        );
        assert_eq!(namespace_label(&web), "team-a");
    }

    /// Every count a search shows is separated and pluralised the shared way.
    #[test]
    fn every_search_count_goes_through_the_shared_format() {
        assert_eq!(result_count(0), "0 results");
        assert_eq!(result_count(1), "1 result");
        assert_eq!(result_count(10_004), "10,004 results");
        assert_eq!(scanned_count(1), "1 resource");
        assert_eq!(scanned_count(10_010), "10,010 resources");
        // `results` is the word the command palette and both switchers use for
        // the same job; four nouns for one count is what D5 was about.
        for line in [
            result_count(42),
            search_status(SearchPhase::Results, 42, 0, false),
            search_status(SearchPhase::Results, 42, 10_004, true),
        ] {
            assert!(line.contains("results"), "{line}");
            assert!(!line.contains("matches"), "{line}");
        }
    }

    /// Every failure states what happened and what to do, in two sentences that
    /// each fit the popup, and none of them leaks the raw API error.
    #[test]
    fn error_helpers_split_the_outcome_from_the_repair() {
        let cases = [
            (
                SearchError::Unauthorized,
                SEARCH_UNAUTHORIZED,
                "Authentication failed",
            ),
            (SearchError::Forbidden, SEARCH_FORBIDDEN, "Access denied"),
            (SearchError::Timeout, SEARCH_TIMED_OUT, "Search timed out"),
            (
                SearchError::NoResources,
                SEARCH_NO_RESOURCES,
                "No resource types",
            ),
            (SearchError::Unavailable, SEARCH_FAILED, "Search failed"),
        ];
        let mut reasons: Vec<String> = Vec::new();
        for (error, outcome, title) in cases {
            let reason = search_error_reason(error);
            assert_eq!(reason, outcome);
            assert_eq!(error_title(Some(&reason)), title);
            assert_eq!(error_detail(Some(&reason)), outcome);
            let repair = error_repair(Some(&reason));
            assert!(repair.contains("then retry"), "{repair}");
            assert!(
                !outcome.contains("then retry"),
                "the outcome and the repair are separate lines: {outcome}"
            );
            assert!(outcome.chars().count() <= 60, "{outcome}");
            assert!(!reason.to_lowercase().contains("context"));
            reasons.push(reason);
        }
        let unique: std::collections::HashSet<&String> = reasons.iter().collect();
        assert_eq!(
            unique.len(),
            reasons.len(),
            "two failures must not share a reason, or the title cannot tell them apart"
        );
        assert_eq!(error_title(Some(SEARCH_UNAVAILABLE)), "Search unavailable");
        assert_eq!(error_detail(Some(SEARCH_UNAVAILABLE)), SEARCH_UNAVAILABLE);
        assert_eq!(error_detail(Some("raw kube error")), SEARCH_FAILED);
        assert!(error_repair(Some("raw kube error")).ends_with("then retry."));
    }

    /// Every repair is one actionable sentence, and none of them asks twice.
    #[test]
    fn every_failure_offers_exactly_one_repair_sentence() {
        for reason in [
            SEARCH_FAILED,
            SEARCH_UNAVAILABLE,
            SEARCH_UNAUTHORIZED,
            SEARCH_FORBIDDEN,
            SEARCH_TIMED_OUT,
            SEARCH_NO_RESOURCES,
        ] {
            let repair = error_repair(Some(reason));
            assert!(repair.contains("then retry"), "{repair}");
            assert_eq!(repair.matches('.').count(), 1, "{repair}");
            assert!(error_detail(Some(reason)).ends_with('.'), "{reason}");
        }
    }

    #[gpui_kit::test]
    fn popup_geometry_matches_shell_and_caps_long_results(cx: &mut TestAppContext) {
        init_app(cx);
        let query = format!("web-{}", "q".repeat(2_048));
        let name_suffix = "n".repeat(128);
        for shell_width in [280.0, 560.0] {
            let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
            view.update(cx, |view, cx| {
                assert!(view.open(ResourceSpec::pods(), cx));
            });
            cx.run_until_parked();
            view.update(cx, |view, cx| {
                view.inject_results_for_test_at_request(
                    (0..80)
                        .map(|index| hit(&format!("web-{index}-{name_suffix}")))
                        .collect(),
                    &query,
                    1,
                    80,
                    false,
                    false,
                );
                cx.notify();
            });
            assert_eq!(
                view.read_with(cx, |view, _| view.hits().len()),
                SEARCH_RESULT_LIMIT
            );
            cx.simulate_resize(gpui_kit::size(px(shell_width), px(640.0)));
            cx.run_until_parked();

            let popup = cx
                .debug_bounds("resource-search")
                .expect("resource search is laid out");
            let input = cx
                .debug_bounds("shared-text-input")
                .expect("resource search input is laid out");
            let results = cx
                .debug_bounds("resource-search-results")
                .expect("resource search results is laid out");
            assert_eq!(
                view.read_with(cx, |view, _| view.hits().len()),
                SEARCH_RESULT_LIMIT
            );
            assert_eq!(f32::from(popup.size.width), search_width(shell_width));
            assert!(f32::from(popup.size.height) <= SEARCH_MAX_HEIGHT);
            assert!(input.left() >= popup.left() && input.right() <= popup.right());
            assert!(f32::from(results.size.height) > 0.0);
            assert!(results.bottom() <= popup.bottom());
            assert!(
                f32::from(results.size.height)
                    < f32::from(design::size::ROW) * SEARCH_RESULT_LIMIT as f32
            );
        }
    }

    /// The modal header keeps one line, and keeps its text inside the card.
    ///
    /// The title is the shared `panel_title` role now, which wraps the label in a styled box
    /// rather than returning the label itself. A title that wraps is a different defect from a
    /// title at the wrong scale: in a 560px card the second line pushes what is beside it out of
    /// the card. The card's own title is a fixed short string, so the observable half of that
    /// chain is the status sentence beside it, which the same search can make arbitrarily long —
    /// if the ellipsis or the shrink chain stopped working, this row would grow to a paragraph.
    #[gpui_kit::test]
    fn the_modal_header_keeps_one_line_and_its_text_inside_the_card(cx: &mut TestAppContext) {
        init_app(cx);
        // A scanned count this large makes the status sentence longer than the row it lives in.
        let scanned = 1_000_000_000_000_usize;
        for shell_width in [280.0, 560.0] {
            let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
            view.update(cx, |view, cx| {
                assert!(view.open(ResourceSpec::pods(), cx));
                view.inject_results_for_test_at_request(
                    vec![hit("web")],
                    "web",
                    1,
                    scanned,
                    true,
                    false,
                );
                cx.notify();
            });
            cx.simulate_resize(gpui_kit::size(px(shell_width), px(640.0)));
            cx.run_until_parked();

            let card = cx
                .debug_bounds("resource-search")
                .expect("the modal card is laid out");
            let title = cx
                .debug_bounds("resource-search-title")
                .expect("the card title is laid out");
            let status = cx
                .debug_bounds("resource-search-status")
                .expect("the status sentence is laid out");
            assert!(
                f32::from(title.size.height) <= f32::from(design::text::TITLE_LINE_HEIGHT) + 1.0,
                "the title is one line of the panel-title role at {shell_width}px, not a wrapped \
                 paragraph: it drew {}px",
                f32::from(title.size.height)
            );
            assert!(
                f32::from(status.size.height) <= f32::from(design::text::CAPTION_LINE_HEIGHT) + 1.0,
                "a {scanned}-scanned status must be ellipsised onto one line, not wrapped: at \
                 {shell_width}px it drew {}px",
                f32::from(status.size.height)
            );
            // The fixture only proves something if the sentence really is longer than the row it
            // has to fit in, so that is checked rather than assumed.
            let sentence = search_status(SearchPhase::Results, 1, scanned, true);
            let needed = sentence.chars().count() as f32 * f32::from(design::text::CAPTION) * 0.6;
            assert!(
                needed > f32::from(status.size.width),
                "the status sentence has to outgrow its box for this test to cover the ellipsis: \
                 {sentence:?} needs about {needed}px of {status:?}"
            );
            for (name, bounds) in [("title", title), ("status", status)] {
                assert!(
                    bounds.left() >= card.left() - px(1.)
                        && bounds.right() <= card.right() + px(1.),
                    "the {name} left the card at {shell_width}px: {bounds:?} against {card:?}"
                );
            }
        }
    }

    /// Incomplete results are a statement about what the app could not read, so
    /// the notice carries the confidence marker, not a status hue.
    #[gpui_kit::test]
    fn partial_results_carry_the_confidence_marker_not_a_status_hue(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test_at_request(vec![hit("web")], "web", 1, 1, false, true);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.partial()));
        assert!(PARTIAL_RESULTS_NOTICE.contains("these results are incomplete"));
        assert!(PARTIAL_RESULTS_NOTICE.contains("Check list permission"));
        assert!(!PARTIAL_RESULTS_NOTICE.contains("unhealthy"));
        // The notice is a statement about coverage, so it wears the confidence
        // shape and its copy names the confidence, not a health state.
        assert_eq!(
            design::confidence::icon(Confidence::Unknown),
            gpui_kit::assets::IconName::CircleQuestionMark,
            "an incomplete answer is its own channel, with its own shape"
        );
        assert!(
            design::confidence_label(Confidence::Unknown).contains("did not answer"),
            "the marker says the cluster did not answer, not that anything is wrong"
        );
    }

    /// The state has to say what happened and what to do, and offer the repair as
    /// a control rather than describing it.
    #[gpui_kit::test]
    fn the_no_match_state_offers_clear_search(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(view.open(ResourceSpec::pods(), cx));
                view.input
                    .update(cx, |input, cx| input.set_text("zzz", window, cx));
                view.inject_results_for_test_at_request(Vec::new(), "zzz", 1, 4, false, false);
                cx.notify();
            })
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Results
        );
        assert!(
            view.read_with(cx, |view, _| view.state_clear_focus().is_some()),
            "a query that matched nothing offers a way out"
        );
        let clear = cx
            .debug_bounds("resource-search-clear")
            .expect("the state offers a control");
        let scope = cx
            .debug_bounds("resource-search-scope")
            .expect("the scope states what the search covers");
        let input = cx
            .debug_bounds("shared-text-input")
            .expect("the dialog has a search field");
        assert!(
            scope.top() >= input.bottom(),
            "the scope is its own line under the field, not part of it"
        );
        cx.simulate_click(clear.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(
                view.input.read(cx).text(),
                "",
                "the state's control clears the query"
            );
            let _ = window;
        });
        assert!(cx.debug_bounds("resource-search-clear").is_none());
    }

    /// A failed search offers Retry and not the state's own clear, so the dialog
    /// never offers two repairs for one state.
    #[gpui_kit::test]
    fn a_failed_search_offers_retry_instead_of_clear(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.set_query("web", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(201));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Error
        );
        assert!(view.read_with(cx, |view, _| view.state_clear_focus().is_none()));
        assert!(cx.debug_bounds("resource-search-retry").is_some());
        assert!(cx.debug_bounds("resource-search-clear").is_none());
    }

    #[gpui_kit::test]
    fn popup_geometry_uses_center_bounds(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test(vec![hit("web")]);
            view.set_container_bounds(
                Bounds::new(point(px(240.0), px(80.0)), size(px(320.0), px(260.0))),
                cx,
            );
        });
        cx.run_until_parked();

        let popup = cx
            .debug_bounds("resource-search")
            .expect("resource search is laid out");
        assert_eq!(f32::from(popup.size.width), search_width(320.0));
        assert!(f32::from(popup.size.height) <= search_max_height(260.0));
    }

    #[gpui_kit::test]
    fn input_waits_for_debounce_before_query(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.set_query("web", cx);
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Query
        );
        cx.executor().advance_clock(Duration::from_millis(199));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Query
        );
        cx.executor().advance_clock(Duration::from_millis(2));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Error
        );
    }

    #[gpui_kit::test]
    fn new_query_clears_old_results_before_a_new_response(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.machine.handle(&SearchEvent::QueryChanged {
                query: "old".to_owned(),
            });
            view.machine
                .handle(&SearchEvent::DebounceElapsed { request_id: 1 });
            view.machine.handle(&SearchEvent::Results {
                request_id: 1,
                hits: vec![hit("old")],
                scanned: 1,
                truncated: false,
                partial: false,
            });

            view.machine.handle(&SearchEvent::QueryChanged {
                query: "new".to_owned(),
            });
            view.machine.handle(&SearchEvent::Results {
                request_id: 1,
                hits: vec![hit("late")],
                scanned: 1,
                truncated: false,
                partial: false,
            });
        });
        assert!(view.read_with(cx, |view, _| view.hits().is_empty()));
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Query
        );
    }

    #[gpui_kit::test]
    fn queued_input_changes_coalesce_into_one_query(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(view.open(ResourceSpec::pods(), cx));
                view.input
                    .update(cx, |input, cx| input.set_text("we", window, cx));
                view.input
                    .update(cx, |input, cx| input.set_text("web", window, cx));
            })
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.query().to_owned()), "web");
        assert_eq!(
            view.read_with(cx, |view, _| view.debounce_epoch),
            1,
            "only the newest queued change reaches the machine"
        );
    }

    #[gpui_kit::test]
    fn injected_results_survive_an_input_change_queued_before_them(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(view.open(ResourceSpec::pods(), cx));
                view.input
                    .update(cx, |input, cx| input.set_text("we", window, cx));
                view.inject_results_for_test(vec![hit("web-1"), hit("web-2")]);
            })
        });
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.hits().len()), 2);
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(0));
    }

    #[gpui_kit::test]
    fn keyboard_selection_keeps_input_focus_and_selection_visible(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test(vec![hit("web-1"), hit("web-2")]);
        });
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("down"), window, cx);
            });
            assert!(focus.is_focused(window));
        });
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(1));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("up"), window, cx);
            });
        });
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(0));
    }

    #[gpui_kit::test]
    fn home_end_and_page_navigation_wrap_across_fifty_results(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test(
                (0..SEARCH_RESULT_LIMIT)
                    .map(|index| hit(&format!("web-{index}")))
                    .collect(),
            );
        });
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));

        for (key, expected) in [
            ("end", 49),
            ("pagedown", 9),
            ("home", 0),
            ("pageup", 40),
            ("down", 41),
            ("pageup", 31),
            ("pagedown", 41),
        ] {
            let stroke = if matches!(key, "home" | "end") {
                command_keystroke(key, false)
            } else {
                keystroke(key)
            };
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.handle_keystroke(&stroke, window, cx);
                });
            });
            assert_eq!(
                view.read_with(cx, |view, _| view.selected_index()),
                Some(expected),
                "{key}"
            );
        }
    }

    #[gpui_kit::test]
    fn home_end_reach_the_input_while_command_edges_navigate_results(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(view.open(ResourceSpec::pods(), cx));
                view.inject_results_for_test(vec![hit("web-1"), hit("web-2"), hit("web-3")]);
                view.input
                    .update(cx, |input, cx| input.set_text("abc", window, cx));
            })
        });
        cx.run_until_parked();
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("home"), window, cx)
            });
        });
        cx.simulate_input("x");
        assert_eq!(
            view.read_with(cx, |view, cx| view.input.read(cx).text().to_owned()),
            "xabc"
        );
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("end"), window, cx)
            });
        });
        cx.simulate_input("y");
        assert_eq!(
            view.read_with(cx, |view, cx| view.input.read(cx).text().to_owned()),
            "xabcy"
        );

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.input.update(cx, |input, cx| input.clear(window, cx));
                view.inject_results_for_test(vec![hit("web-1"), hit("web-2"), hit("web-3")]);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            view.update(cx, |view, cx| {
                view.handle_keystroke(&command_keystroke("end", true), window, cx)
            });
        });
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(2));
    }

    /// The advertised shortcuts must include Tab, so a keyboard user knows the
    /// field, Clear, and Close are reachable without a pointer.
    #[test]
    fn the_dialog_advertises_every_shortcut_it_answers() {
        for shortcut in [
            "Tab",
            "Shift+Tab",
            "ArrowUp",
            "ArrowDown",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Enter",
            "Control+Enter",
            "Control+Shift+Enter",
            "Escape",
        ] {
            assert!(
                SEARCH_KEYSHORTCUTS.contains(shortcut),
                "{shortcut} is answered but not advertised: {SEARCH_KEYSHORTCUTS}"
            );
        }
        for action in [
            SearchResultAction::Open,
            SearchResultAction::Exec,
            SearchResultAction::PortForward,
        ] {
            assert!(
                SEARCH_KEYSHORTCUTS.contains(action.dom_keystrokes()),
                "{} must be advertised: {SEARCH_KEYSHORTCUTS}",
                action.dom_keystrokes()
            );
        }
        assert!(!SEARCH_KEYSHORTCUTS.contains('\n'));
    }

    /// The empty state does not repeat the card's own title, and its two lines
    /// are told apart by type rather than by a gap.
    #[test]
    fn the_idle_state_adds_a_fact_and_its_two_lines_have_two_type_roles() {
        assert_ne!(SEARCH_IDLE_STATE_TITLE, SEARCH_CARD_TITLE);
        assert!(
            !SEARCH_IDLE_STATE_TITLE.contains(SEARCH_CARD_TITLE),
            "the card already says {:?}",
            SEARCH_CARD_TITLE
        );
        let (detail_size, detail_secondary) = StateLine::Detail.type_role();
        let (next_step_size, next_step_secondary) = StateLine::NextStep.type_role();
        assert_eq!(detail_size, design::text::CAPTION);
        assert!(
            detail_secondary,
            "what happened stays behind the instruction"
        );
        assert_eq!(next_step_size, design::text::BODY);
        assert!(
            !next_step_secondary,
            "what to do next is the line the reader acts on, so it is not secondary ink"
        );
        assert_ne!(
            (detail_size, detail_secondary),
            (next_step_size, next_step_secondary),
            "two lines in one type is not a split"
        );
    }

    /// The footer is one short item per key, so it wraps instead of truncating,
    /// and every key name is the one this platform uses.
    #[test]
    fn footer_hints_are_short_items_named_by_the_platform() {
        let all = [
            SearchResultAction::Open,
            SearchResultAction::Exec,
            SearchResultAction::PortForward,
        ];
        let hints = footer_hints(&all);
        let lines = hints.iter().map(SearchHint::line).collect::<Vec<_>>();
        for (action, suffix) in [
            (SearchResultAction::Open, "Enter"),
            (SearchResultAction::Exec, "+Enter"),
            (SearchResultAction::PortForward, "+Shift+Enter"),
        ] {
            let hint = hints
                .iter()
                .find(|hint| hint.verb == action.label())
                .unwrap_or_else(|| panic!("{action:?} must be documented: {lines:?}"));
            assert!(hint.key.ends_with(suffix), "{:?}", hint.key);
        }
        assert!(lines.iter().any(|line| line.contains("\u{2191}/\u{2193}")));
        assert!(lines.iter().any(|line| line == "Close: Esc"));
        // One key, one verb. The handler cancels an IME composition and then
        // closes, so a `Clear: Esc` next to `Close: Esc` advertised a step this
        // panel does not have. Clearing is the `Clear search` button and the
        // field's own `×`.
        assert!(
            !lines.iter().any(|line| line.starts_with("Clear:")),
            "Escape has one verb here: {lines:?}"
        );
        for line in &lines {
            let key = line
                .split_once(": ")
                .map(|(_, key)| key)
                .unwrap_or_default();
            assert_eq!(
                lines
                    .iter()
                    .filter(|other| other.split_once(": ").map(|(_, k)| k) == Some(key))
                    .count(),
                1,
                "one verb per key: {lines:?}"
            );
        }
        let command = command_modifier();
        for line in &lines {
            assert!(!line.contains('·'), "separator: {line}");
            assert!(!line.contains('\n'), "each item is one line: {line}");
            // A narrow popup must not hide a hint.
            assert!(line.chars().count() <= 32, "{line}");
        }
        if cfg!(target_os = "macos") {
            assert!(lines.iter().any(|line| line.contains("Cmd+Enter")));
        } else {
            assert!(lines.iter().any(|line| line.contains("Ctrl+Enter")));
            assert!(
                !lines.iter().any(|line| line.contains("Cmd")),
                "this platform has no Cmd key: {lines:?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|line| line == &format!("First: {command}+Home")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line == &format!("Last: {command}+End")),
            "{lines:?}"
        );

        let without_action_handler = footer_hints(&[SearchResultAction::Open]);
        assert!(
            !without_action_handler
                .iter()
                .any(|hint| hint.verb == SearchResultAction::Exec.label()),
            "an action the panel cannot run is not advertised"
        );
    }

    #[gpui_kit::test]
    fn results_are_grouped_by_kind_and_navigation_follows_the_groups(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test_at_request(
                vec![hit("web-a"), node_hit("node-a"), hit("web-b")],
                "a",
                1,
                3,
                false,
                false,
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.display_order()),
            vec![0, 2, 1],
            "a kind stays together, in the order the kinds first appear"
        );
        assert!(cx.debug_bounds("resource-search-group-Pod").is_some());
        assert!(cx.debug_bounds("resource-search-group-Node").is_some());

        // Down walks the display order, so it lands on the second Pod.
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("down"), window, cx)
            });
        });
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(2));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("down"), window, cx)
            });
        });
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(1));
    }

    #[gpui_kit::test]
    fn a_result_row_highlights_the_matched_name(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test_at_request(
                vec![hit("demo-web-canary")],
                "web",
                1,
                1,
                false,
                false,
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("resource-search-result-0").is_some());
        assert!(
            cx.debug_bounds("resource-search-match-0-0").is_some(),
            "the matched name is highlighted"
        );
    }

    #[gpui_kit::test]
    fn recent_searches_are_offered_and_re_run(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.machine.handle(&SearchEvent::Committed {
                query: "web".to_owned(),
            });
            view.machine.handle(&SearchEvent::Committed {
                query: "cache".to_owned(),
            });
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.recents().to_vec()),
            vec!["cache".to_owned(), "web".to_owned()]
        );
        assert!(
            cx.debug_bounds("resource-search-recent-0").is_some(),
            "an empty field offers the recent searches"
        );
        assert!(
            cx.debug_bounds("resource-search-recent-group").is_some(),
            "the recent searches carry a group heading"
        );

        let row = cx
            .debug_bounds("resource-search-recent-1")
            .expect("the web recent");
        cx.simulate_click(row.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.query().to_owned()),
            "web",
            "clicking a recent search re-runs it"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Query
        );
    }

    #[gpui_kit::test]
    fn exec_and_port_forward_reach_a_result_without_opening_it(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        let opened = Rc::new(Cell::new(0usize));
        let ran: Rc<std::cell::RefCell<Vec<SearchResultAction>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let opened_handler = opened.clone();
        let ran_handler = ran.clone();
        view.update(cx, |view, cx| {
            view.set_open_handler(Rc::new(move |_, _, _| {
                opened_handler.set(opened_handler.get() + 1);
            }));
            view.set_action_handler(Rc::new(move |_, action, _, _| {
                ran_handler.borrow_mut().push(action);
            }));
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test(vec![hit("web")]);
        });
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| window.focus(&focus, cx));

        let exec = command_keystroke("enter", false);
        let forward = Keystroke {
            modifiers: Modifiers {
                control: true,
                shift: true,
                ..Modifiers::none()
            },
            key: "enter".to_owned(),
            key_char: None,
        };
        for (stroke, expected) in [
            (exec, SearchResultAction::Exec),
            (forward, SearchResultAction::PortForward),
        ] {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| view.handle_keystroke(&stroke, window, cx));
            });
            assert_eq!(ran.borrow().last().copied(), Some(expected));
        }
        assert_eq!(
            opened.get(),
            0,
            "a result action must not also open the preview"
        );
    }

    #[gpui_kit::test]
    fn single_click_opens_a_result(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        let opened = Rc::new(Cell::new(0usize));
        let opened_handler = opened.clone();
        view.update(cx, |view, cx| {
            view.set_open_handler(Rc::new(move |_, _, _| {
                opened_handler.set(opened_handler.get() + 1);
            }));
            assert!(view.open(ResourceSpec::pods(), cx));
            view.inject_results_for_test(vec![hit("web")]);
        });
        cx.run_until_parked();

        let row = cx
            .debug_bounds("resource-search-result-0")
            .expect("search result is laid out");
        cx.simulate_click(row.center(), gpui_kit::Modifiers::none());

        assert_eq!(opened.get(), 1);
    }

    #[gpui_kit::test]
    fn retry_restarts_the_failed_query_and_restores_input_focus(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            assert!(view.open(ResourceSpec::pods(), cx));
            view.set_query("web", cx);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(201));
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Error
        );

        let retry = cx
            .debug_bounds("resource-search-retry")
            .expect("retry action is laid out");
        cx.simulate_click(retry.center(), gpui_kit::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Query
        );
        let input_focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        assert!(cx.update(|window, _| input_focus.is_focused(window)));
    }

    #[gpui_kit::test]
    fn tab_traps_focus_across_search_actions(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        view.update(cx, |view, cx| {
            view.set_close_handler(Rc::new(|_, _| {}));
            assert!(view.open(ResourceSpec::pods(), cx));
        });
        let input_focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        let close_focus = view.read_with(cx, |view, _| view.close_focus.clone());
        let retry_focus = view.read_with(cx, |view, _| view.retry_focus.clone());
        cx.update(|window, cx| window.focus(&input_focus, cx));

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("tab"), window, cx);
            });
        });
        assert!(cx.update(|window, _| close_focus.is_focused(window)));

        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("tab"), window, cx);
            });
        });
        assert!(cx.update(|window, _| input_focus.is_focused(window)));

        view.update(cx, |view, cx| view.set_query("web", cx));
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(201));
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&input_focus, cx));
        for expected in [&close_focus, &retry_focus, &input_focus] {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.handle_keystroke(&keystroke("tab"), window, cx);
                });
            });
            assert!(cx.update(|window, _| expected.is_focused(window)));
        }
    }

    #[gpui_kit::test]
    fn search_clear_button_is_a_real_tab_stop_and_returns_focus(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_close_handler(Rc::new(|_, _| {}));
                assert!(view.open(ResourceSpec::pods(), cx));
                view.input
                    .update(cx, |input, cx| input.set_text("web", window, cx));
            })
        });
        cx.run_until_parked();
        let input_focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        let clear_focus = view.read_with(cx, |view, cx| view.input.read(cx).clear_focus_handle());
        cx.update(|window, cx| window.focus(&input_focus, cx));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("tab"), window, cx)
            });
        });
        assert!(cx.update(|window, _| clear_focus.is_focused(window)));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("enter"), window, cx)
            });
        });
        assert_eq!(
            view.read_with(cx, |view, cx| view.input.read(cx).text().to_owned()),
            ""
        );
        assert!(cx.update(|window, _| input_focus.is_focused(window)));
    }

    #[gpui_kit::test]
    fn search_escape_cancels_ime_before_closing(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        let closed = Rc::new(Cell::new(false));
        let closed_handler = closed.clone();
        view.update(cx, |view, cx| {
            view.set_close_handler(Rc::new(move |_, _| {
                closed_handler.set(true);
            }));
            assert!(view.open(ResourceSpec::pods(), cx));
        });
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            view.update(cx, |view, cx| {
                view.input.update(cx, |input, input_cx| {
                    input.set_text("abc", window, input_cx);
                    // The preedit lives in gpui-kit's state, so that is where a
                    // test stages one.
                    let state = input.state().cloned().expect("rendered field");
                    state.update(input_cx, |state, state_cx| {
                        gpui_kit::EntityInputHandler::replace_and_mark_text_in_range(
                            state, None, "你", None, window, state_cx,
                        );
                    });
                });
                view.handle_keystroke(&keystroke("escape"), window, cx);
            });
        });
        assert!(!closed.get());
        // The field's preedit is part of the value it edits, so giving the
        // composition up leaves what the reader composed in the field.
        assert_eq!(
            view.read_with(cx, |view, cx| view.input.read(cx).text().to_owned()),
            "abc你"
        );
        assert!(!cx.update(|window, cx| {
            let state = view.read(cx).input.read(cx).state().cloned();
            state.is_some_and(|state| {
                state.update(cx, |state, cx| {
                    gpui_kit::EntityInputHandler::marked_text_range(state, window, cx).is_some()
                })
            })
        }));
    }

    /// The footer advertises `Close: Esc` and nothing else, so Escape must not
    /// also be a clear step here.
    ///
    /// The picker can list `Clear` and `Close` on one key because its handler is
    /// two steps. This panel's is one: it cancelled a composition, then closed.
    /// While the footer claimed a clear step the reader could press Escape once
    /// and be sure the query survived, and the panel would close instead.
    #[gpui_kit::test]
    fn escape_closes_and_leaves_the_query_alone(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        let closed = Rc::new(Cell::new(0usize));
        let closed_handler = closed.clone();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.set_close_handler(Rc::new(move |_, _| {
                    closed_handler.set(closed_handler.get() + 1);
                }));
                assert!(view.open(ResourceSpec::pods(), cx));
                view.input
                    .update(cx, |input, cx| input.set_text("web", window, cx));
            })
        });
        let focus = view.read_with(cx, |view, cx| view.focus_handle(cx));
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("escape"), window, cx)
            });
        });
        assert_eq!(closed.get(), 1);
        assert_eq!(
            view.read_with(cx, |view, cx| view.input.read(cx).text().to_owned()),
            "web",
            "Escape is not a clear step: the Clear button and the field's own x are"
        );
    }

    #[gpui_kit::test]
    fn enter_and_escape_use_the_configured_handlers(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| SearchView::new(None, cx));
        let opened = Rc::new(Cell::new(false));
        let closed = Rc::new(Cell::new(false));
        let opened_handler = opened.clone();
        let closed_handler = closed.clone();
        view.update(cx, |view, cx| {
            view.set_open_handler(Rc::new(move |_, _, _| {
                opened_handler.set(true);
            }));
            view.set_close_handler(Rc::new(move |_, _| {
                closed_handler.set(true);
            }));
            assert!(view.open(ResourceSpec::pods(), cx));
        });
        cx.run_until_parked();
        view.update(cx, |view, _| {
            view.machine.handle(&SearchEvent::QueryChanged {
                query: "web".to_owned(),
            });
            view.machine
                .handle(&SearchEvent::DebounceElapsed { request_id: 1 });
            view.machine.handle(&SearchEvent::Results {
                request_id: 1,
                hits: vec![hit("web")],
                scanned: 1,
                truncated: false,
                partial: false,
            });
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.phase()),
            SearchPhase::Results
        );
        assert_eq!(view.read_with(cx, |view, _| view.selected_index()), Some(0));
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("enter"), window, cx)
            });
            view.update(cx, |view, cx| {
                view.handle_keystroke(&keystroke("escape"), window, cx)
            });
        });
        assert!(opened.get());
        assert!(closed.get());
    }
}
