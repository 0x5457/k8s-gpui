//! Terminal and port-forward services for the GPUI shell.
//! Local sessions use a private kubeconfig. Exec and port-forward services use the cluster client.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Context, ElementId, Entity, FocusHandle, Font,
    Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, Render, Role, SharedString,
    Styled, Task, TextAlign, Window, div, prelude::*, px, relative,
};
use k8s_core::cluster::{Cluster, ClusterRegistry};
use k8s_core::ops::{self, ExecOptions, ExecSession};
use k8s_term::io::TerminalIo;
use k8s_term::session::SessionExit;
use k8s_term::{
    Palette, SessionEvent, TERMINAL_CELL_PADDING, TermSize, TerminalOutput, TerminalSession,
    TerminalTheme, TerminalView,
};
use k8s_ui::design::{self, INCREASED_CONTRAST_TEXT_MIN, TEXT_MIN_CONTRAST};
use k8s_ui::panels::terminal::{
    ALL_NAMESPACES, ForwardRequest, PortForwardFactory, StartedForward, TerminalEvent,
    TerminalEventSink, TerminalFactory, TerminalInstance, TerminalKind, TerminalRequest,
    TerminalServices,
};
use k8s_ui::table_view::ClusterSession;
use settings::Settings as _;
use theme::ActiveTheme as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc};
use ui::{
    // `ButtonCommon` is what carries `style` and `size`; `ui::prelude` re-exports it but this
    // module imports the component by name, so the trait has to come in explicitly.
    Button,
    ButtonCommon,
    ButtonSize,
    ButtonStyle,
    Clickable,
    Color,
    Icon,
    IconSize,
    TintColor,
    Tooltip,
    rems_from_px,
    v_flex,
};

/// Initial local-shell grid. The view resizes it on the first frame.
const INITIAL_COLUMNS: usize = 120;
const INITIAL_LINES: usize = 30;
const MONO_ADVANCE_RATIO: f32 = 0.6;
const SCROLLBACK: usize = 10_000;
const EXEC_PUMP_CHANNEL_CAPACITY: usize = 64;
const EXEC_PUMP_MAX_INPUT_BYTES: usize = 64 * 1024;
const EXEC_PUMP_IO_TIMEOUT: Duration = Duration::from_millis(16);
/// Default shell for exec sessions in a container.
const EXEC_SHELL: &str = "/bin/sh";
/// Reading measure for the state panel reason, so a long error wraps instead of spanning the
/// window.
const TERMINAL_REASON_MEASURE: f32 = 720.0;
/// Accessibility id shared by the connecting and failure panels, so the change of state is one
/// update on a single live node.
const TERMINAL_PHASE_NODE: &str = "terminal-session-state";
/// Focus order of the `Restart` control in a failed session's state panel. The panel is the only
/// focusable thing in that state, and it takes the stop right after the Dock's own header.
const TERMINAL_RESTART_TAB_INDEX: isize = 4;
/// Element id of the loading sweep, so the animation phase survives re-renders.
const TERMINAL_LOADING_BAR_ID: &str = "terminal-progress-fill";

/// The three states the session panel can report.
///
/// They were three lines of the same muted text, and `Failed` and `Disconnected` were identical
/// apart from two words. `DESIGN.md` §4 Terminal keeps "not connected", "no sessions",
/// "connecting", "session failed", and "exited" as separate boundaries, so each state now has its
/// own name, its own spoken description, and its own recovery path.
const TERMINAL_CONNECTING_TITLE: &str = "Connecting terminal session";
const TERMINAL_CONNECTING_DESCRIPTION: &str =
    "The terminal session is connecting. Wait for the shell to attach.";
const TERMINAL_FAILED_TITLE: &str = "Terminal session failed";
const TERMINAL_FAILED_DESCRIPTION: &str = "The session did not start.";
const TERMINAL_ENDED_TITLE: &str = "Terminal session ended";
const TERMINAL_ENDED_DESCRIPTION: &str = "The session stopped.";

/// Smallest fill, so the bar is still visible at the start of a sweep.
const LOADING_FILL_MIN: f32 = 0.04;
/// Alpha of the unfilled part of the bar.
const LOADING_TRACK_ALPHA: f32 = 0.22;

static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Builds terminal services. Offline sessions return clear errors for exec and port forward.
pub fn services(session: &ClusterSession) -> TerminalServices {
    let context = session.cluster_name().map(str::to_owned);
    let registry = session.registry().cloned();
    let handle = session.tokio_handle().cloned();

    let terminals: TerminalFactory = {
        let registry = registry.clone();
        let handle = handle.clone();
        std::rc::Rc::new(move |request, sink, cx| {
            spawn_terminal(registry.as_ref(), handle.as_ref(), request, sink, cx)
        })
    };
    let forwards: PortForwardFactory = {
        let registry = registry.clone();
        let handle = handle.clone();
        std::rc::Rc::new(move |request, cx| {
            spawn_forward(registry.as_ref(), handle.as_ref(), request, cx)
        })
    };
    TerminalServices {
        terminals,
        forwards,
        context,
        namespace: None,
    }
}

fn cluster_for<'a>(
    registry: Option<&'a Arc<ClusterRegistry>>,
    context: Option<&str>,
) -> Result<&'a Cluster, String> {
    let registry = registry
        .ok_or_else(|| "Not connected to a cluster. Connect to a cluster first.".to_owned())?;
    let name = context
        .ok_or_else(|| "Not connected to a cluster. Connect to a cluster first.".to_owned())?;
    registry
        .clusters()
        .iter()
        .find(|cluster| cluster.name() == name)
        .ok_or_else(|| {
            format!(
                "Cluster `{name}` is no longer available. Refresh the cluster list and try again."
            )
        })
}

fn local_namespace(request: &TerminalRequest) -> &str {
    request
        .namespace
        .as_deref()
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or(ALL_NAMESPACES)
}

fn spawn_terminal(
    registry: Option<&Arc<ClusterRegistry>>,
    handle: Option<&Handle>,
    request: TerminalRequest,
    sink: TerminalEventSink,
    cx: &mut App,
) -> Result<TerminalInstance, String> {
    match &request.kind {
        TerminalKind::Local => {
            let registry = registry.ok_or_else(|| {
                "Not connected to a cluster. Connect to a cluster first.".to_owned()
            })?;
            let cluster = cluster_for(Some(registry), request.context.as_deref())?;
            let snapshot = registry.kubeconfig();
            spawn_local_terminal(cluster, &snapshot, local_namespace(&request), sink, cx)
        }
        TerminalKind::Exec {
            namespace,
            pod,
            container,
        } => {
            eprintln!(
                "k8s-gpui: exec session starting: {}/{} container {}",
                namespace,
                pod,
                container.as_deref().unwrap_or("default"),
            );
            let cluster = cluster_for(registry, request.context.as_deref())?;
            let handle = handle.cloned().ok_or_else(|| {
                "Not connected to a cluster. Connect to a cluster first.".to_owned()
            })?;
            let client = cluster.client().clone();
            let namespace = namespace.clone();
            let pod = pod.clone();
            let container = container.clone();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            handle.spawn(async move {
                let resource = k8s_ui::table_view::pods_resource();
                let options = ExecOptions {
                    container,
                    tty: true,
                    command: vec![EXEC_SHELL.to_owned()],
                    env: Vec::new(),
                };
                let result =
                    ops::exec(&client, &resource, Some(namespace.as_str()), &pod, &options).await;
                let _ = sender.send(result);
            });

            let exec_handle = handle.clone();
            let host = cx.new(|cx| TerminalHost::connecting(sink, false, cx));
            host.update(cx, |host, cx| {
                let task = cx.spawn(async move |this, cx| {
                    let result = match receiver.await {
                        Ok(result) => result.map_err(|error| error.to_string()),
                        Err(_) => Err("Exec connection was canceled.".to_owned()),
                    };
                    this.update(cx, |host, cx| host.attach_exec(result, &exec_handle, cx))
                        .ok();
                });
                host.events_task = Some(task);
            });
            Ok(instance_for(host))
        }
    }
}

fn local_shell_args(unix_login: bool) -> Vec<OsString> {
    if unix_login {
        vec![OsString::from("-l")]
    } else {
        Vec::new()
    }
}

fn initial_cell_size(font_size: Pixels, line_height: Pixels) -> (u16, u16) {
    let width = f32::from(font_size) * MONO_ADVANCE_RATIO + f32::from(TERMINAL_CELL_PADDING);
    (
        width.clamp(1.0, f32::from(u16::MAX)) as u16,
        f32::from(line_height).clamp(1.0, f32::from(u16::MAX)) as u16,
    )
}

fn spawn_local_terminal(
    cluster: &Cluster,
    snapshot: &kube::config::Kubeconfig,
    namespace: &str,
    sink: TerminalEventSink,
    cx: &mut App,
) -> Result<TerminalInstance, String> {
    let files = SessionFiles::prepare(cluster, snapshot, Some(namespace))?;
    eprintln!(
        "k8s-gpui: {} starting in namespace {}",
        files.trace_label(),
        namespace,
    );
    let args = local_shell_args(cfg!(unix));
    let env = files.env();
    let cwd = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let settings = theme_settings::ThemeSettings::get_global(cx);
    let font_size = settings.buffer_font_size(cx);
    let line_height = px(f32::from(font_size) * settings.line_height());
    let (cell_width, cell_height) = initial_cell_size(font_size, line_height);
    let (session, events) = TerminalSession::local_command_bounded(
        None,
        args,
        cwd,
        env,
        true,
        TermSize::new(INITIAL_COLUMNS, INITIAL_LINES),
        cell_width,
        cell_height,
        SCROLLBACK,
    )
    .map_err(|error| {
        eprintln!("k8s-gpui: local terminal start failed: {error:#}");
        "The local terminal failed to start. Check that your default shell is available, then try again."
            .to_owned()
    })?;

    let host = cx.new(|cx| TerminalHost::connecting(sink, false, cx));
    host.update(cx, |host, cx| {
        host.attach(session, events.into(), Some(files), cx)
    });
    Ok(instance_for(host))
}

fn instance_for(host: Entity<TerminalHost>) -> TerminalInstance {
    let weak = host.downgrade();
    TerminalInstance {
        view: host.into(),
        activate: Box::new(move |window, cx| {
            if let Some(host) = weak.upgrade() {
                host.update(cx, |host, cx| host.focus(window, cx));
            }
        }),
    }
}

/// Terminal host. It shows a connection state before attaching the terminal view.
struct TerminalHost {
    phase: Phase,
    sink: std::rc::Rc<TerminalEventSink>,
    events_task: Option<Task<()>>,
    /// Focus handle of the attached terminal view.
    focus: Option<FocusHandle>,
    /// Focus handle of the connection and failure panels. It gives the window a real focus
    /// target after a session ends, so keyboard navigation keeps working.
    panel_focus: FocusHandle,
    focus_requested: bool,
    palette: Option<Palette>,
    /// Font, size, and line height the attached view is currently rendering with. A settings
    /// change reaches an open session instead of waiting for the next one.
    font: Option<(Font, Pixels, Pixels)>,
    allow_osc_title: bool,
    exec_shutdown: Option<tokio::sync::watch::Sender<bool>>,
}

enum Phase {
    Connecting,
    Ready(Entity<TerminalView>),
    Failed(String),
    Disconnected(String),
}

/// Which element owns the focus for a phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FocusOwner {
    /// The attached terminal view.
    View,
    /// The connection or failure panel. It keeps the window on a rendered focus target
    /// after the terminal view is gone.
    Panel,
    /// Nothing yet, so the focus stays where it is until the session attaches.
    Pending,
}

fn focus_owner(phase: &Phase) -> FocusOwner {
    match phase {
        Phase::Ready(_) => FocusOwner::View,
        Phase::Failed(_) | Phase::Disconnected(_) => FocusOwner::Panel,
        Phase::Connecting => FocusOwner::Pending,
    }
}

/// Phase name for a diagnostic line. A failure has to say whether it happened while the session was
/// still connecting or after it attached: the two need different next steps, and the reason alone
/// does not tell them apart.
fn phase_label(phase: &Phase) -> &'static str {
    match phase {
        Phase::Connecting => "connecting",
        Phase::Ready(_) => "attached",
        Phase::Failed(_) => "already failed",
        Phase::Disconnected(_) => "already ended",
    }
}

enum EventReceiver {
    Bounded(mpsc::Receiver<SessionEvent>),
    Legacy(mpsc::UnboundedReceiver<SessionEvent>),
}

impl EventReceiver {
    async fn recv(&mut self) -> Option<SessionEvent> {
        match self {
            Self::Bounded(events) => events.recv().await,
            Self::Legacy(events) => events.recv().await,
        }
    }
}

impl From<mpsc::Receiver<SessionEvent>> for EventReceiver {
    fn from(events: mpsc::Receiver<SessionEvent>) -> Self {
        Self::Bounded(events)
    }
}

impl From<mpsc::UnboundedReceiver<SessionEvent>> for EventReceiver {
    fn from(events: mpsc::UnboundedReceiver<SessionEvent>) -> Self {
        Self::Legacy(events)
    }
}

impl TerminalHost {
    fn connecting(sink: TerminalEventSink, allow_osc_title: bool, cx: &mut Context<Self>) -> Self {
        Self {
            phase: Phase::Connecting,
            sink: std::rc::Rc::new(sink),
            events_task: None,
            focus: None,
            panel_focus: cx.focus_handle().tab_stop(true),
            focus_requested: true,
            palette: None,
            font: None,
            allow_osc_title,
            exec_shutdown: None,
        }
    }

    fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_requested = true;
        self.focus_now(window, cx);
    }

    /// Returns the handle that the window should focus for the current phase.
    fn focus_target(&self) -> Option<FocusHandle> {
        match focus_owner(&self.phase) {
            FocusOwner::View => self.focus.clone(),
            FocusOwner::Panel => Some(self.panel_focus.clone()),
            FocusOwner::Pending => None,
        }
    }

    fn focus_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.focus_target() {
            self.focus_requested = false;
            window.focus(&handle, cx);
        }
    }

    fn stop_exec(&mut self) {
        if let Some(shutdown) = self.exec_shutdown.take() {
            let _ = shutdown.send(true);
        }
    }

    fn session_exited(&mut self, status: SessionExit) {
        self.stop_exec();
        if matches!(&self.phase, Phase::Ready(_)) {
            let reason = exit_reason(status);
            eprintln!("k8s-gpui: terminal session ended: {reason}");
            self.phase = Phase::Disconnected(reason);
            // The view is replaced, so its focus handle is gone. Move the focus to the
            // panel instead of leaving the window on a handle that is no longer rendered.
            self.focus = None;
            self.focus_requested = true;
        }
    }

    fn fail(&mut self, reason: String, cx: &mut Context<Self>) {
        eprintln!(
            "k8s-gpui: terminal session failed while {}: {reason}",
            phase_label(&self.phase),
        );
        self.phase = Phase::Failed(reason);
        self.focus = None;
        self.focus_requested = true;
        cx.notify();
    }

    /// Attaches a local session directly to the view.
    fn attach(
        &mut self,
        session: Arc<TerminalSession>,
        events: EventReceiver,
        files: Option<SessionFiles>,
        cx: &mut Context<Self>,
    ) {
        let kind = if self.allow_osc_title {
            "session"
        } else {
            "shell"
        };
        match &files {
            Some(files) => eprintln!(
                "k8s-gpui: {kind} session attached: {}, {}x{}, {SCROLLBACK} lines of scrollback",
                files.trace_label(),
                INITIAL_COLUMNS,
                INITIAL_LINES,
            ),
            None => eprintln!(
                "k8s-gpui: {kind} session attached: exec stream, {INITIAL_COLUMNS}x{INITIAL_LINES}, \
                 {SCROLLBACK} lines of scrollback"
            ),
        }
        let view = build_view(session, events, self, files, self.allow_osc_title, cx);
        self.phase = Phase::Ready(view);
        cx.notify();
    }

    /// Attaches an exec session or keeps its error in the current tab.
    fn attach_exec(
        &mut self,
        result: Result<ExecSession, String>,
        handle: &Handle,
        cx: &mut Context<Self>,
    ) {
        self.stop_exec();
        let exec = match result {
            Ok(exec) => exec,
            Err(reason) => {
                self.fail(reason, cx);
                return;
            }
        };
        let handle = handle.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let host_shutdown = shutdown_tx.clone();
        self.exec_shutdown = Some(host_shutdown);
        let started = TerminalSession::from_transport_facade_bounded(
            TermSize::new(INITIAL_COLUMNS, INITIAL_LINES),
            SCROLLBACK,
            move |output| {
                spawn_exec_pump_with_shutdown(&handle, exec, output, shutdown_tx, shutdown_rx)
                    .map_err(|reason| anyhow::anyhow!(reason))
            },
        );
        match started {
            Ok((session, events)) => {
                self.attach(session, events.into(), None, cx);
            }
            Err(error) => {
                eprintln!("k8s-gpui: terminal attach failed: {error:#}");
                self.stop_exec();
                self.fail(
                    "The terminal session failed to attach to the Pod.".to_owned(),
                    cx,
                );
            }
        }
    }

    fn sync_palette(&mut self, cx: &mut Context<Self>) {
        let palette = palette_from_theme(cx);
        if self.palette.as_ref() == Some(&palette) {
            return;
        }
        let view = match &self.phase {
            Phase::Ready(view) => Some(view.clone()),
            Phase::Connecting | Phase::Failed(_) | Phase::Disconnected(_) => None,
        };
        self.palette = Some(palette.clone());
        if let Some(view) = view {
            view.update(cx, |view, cx| view.set_palette(palette, cx));
        }
    }

    /// Pushes a font or font-size change into the attached session. Without this the setting
    /// would only reach the next terminal the user opens.
    fn sync_font(&mut self, cx: &mut Context<Self>) {
        let (font, font_size, line_height) = terminal_font_metrics(cx);
        if self.font.as_ref() == Some(&(font.clone(), font_size, line_height)) {
            return;
        }
        let view = match &self.phase {
            Phase::Ready(view) => Some(view.clone()),
            Phase::Connecting | Phase::Failed(_) | Phase::Disconnected(_) => None,
        };
        self.font = Some((font.clone(), font_size, line_height));
        if let Some(view) = view {
            view.update(cx, |view, cx| {
                view.set_font(font, font_size, line_height, cx)
            });
        }
    }
}

impl Drop for TerminalHost {
    fn drop(&mut self) {
        self.stop_exec();
    }
}

fn terminal_minimum_contrast(increased: bool) -> f32 {
    if increased {
        INCREASED_CONTRAST_TEXT_MIN
    } else {
        TEXT_MIN_CONTRAST
    }
}

fn palette_from_theme(cx: &App) -> Palette {
    let theme = cx.theme();
    let colors = theme.colors();
    let player = theme.players().local();
    TerminalTheme {
        ansi: [
            colors.terminal_ansi_black,
            colors.terminal_ansi_red,
            colors.terminal_ansi_green,
            colors.terminal_ansi_yellow,
            colors.terminal_ansi_blue,
            colors.terminal_ansi_magenta,
            colors.terminal_ansi_cyan,
            colors.terminal_ansi_white,
            colors.terminal_ansi_bright_black,
            colors.terminal_ansi_bright_red,
            colors.terminal_ansi_bright_green,
            colors.terminal_ansi_bright_yellow,
            colors.terminal_ansi_bright_blue,
            colors.terminal_ansi_bright_magenta,
            colors.terminal_ansi_bright_cyan,
            colors.terminal_ansi_bright_white,
        ],
        dim_ansi: [
            colors.terminal_ansi_dim_black,
            colors.terminal_ansi_dim_red,
            colors.terminal_ansi_dim_green,
            colors.terminal_ansi_dim_yellow,
            colors.terminal_ansi_dim_blue,
            colors.terminal_ansi_dim_magenta,
            colors.terminal_ansi_dim_cyan,
            colors.terminal_ansi_dim_white,
        ],
        foreground: colors.terminal_foreground,
        bright_foreground: colors.terminal_bright_foreground,
        dim_foreground: colors.terminal_dim_foreground,
        background: design::surface::terminal(cx),
        cursor: player.cursor,
        selection: player.selection,
        // A search hit is a role, not a terminal invention. The palette used to
        // rebuild both washes out of ANSI yellow and red at one fixed lightness
        // picked by the background's polarity, so a terminal on a theme the
        // product does not ship still highlighted with colours that theme never
        // agreed to.
        search_match: design::search_match::background(cx),
        search_match_active: design::search_match::active_background(cx),
        focus: design::focus::border(cx),
        scrollbar_thumb: colors.scrollbar_thumb_background,
        scrollbar_thumb_hover: colors.scrollbar_thumb_hover_background,
        minimum_contrast: terminal_minimum_contrast(k8s_ui::settings::increase_contrast_enabled(
            cx,
        )),
    }
    .into()
}

/// The reason a session stopped. It is the only part of the state that can be long, so it gets
/// its own measure instead of spanning the window, and it never carries the recovery action: the
/// action is a control, and a sentence telling the reader where to find a control is not one.
fn exit_reason(status: SessionExit) -> String {
    match (status.code, status.signal) {
        (Some(code), _) => format!("The shell exited with code {code}."),
        (None, Some(signal)) => format!("The shell was stopped by signal {signal}."),
        (None, None) => "The shell ended.".to_owned(),
    }
}

/// Font, size, and line height the terminal grid is drawn with. The Dock reads the same values,
/// so a settings change reaches an open session and the next one alike.
fn terminal_font_metrics(cx: &App) -> (Font, Pixels, Pixels) {
    let settings = theme_settings::ThemeSettings::get_global(cx);
    let font_size = settings.buffer_font_size(cx);
    (
        settings.buffer_font.clone(),
        font_size,
        px(f32::from(font_size) * settings.line_height()),
    )
}

/// Builds the terminal view and forwards events to the Dock.
fn build_view(
    session: Arc<TerminalSession>,
    mut events: EventReceiver,
    host: &mut TerminalHost,
    files: Option<SessionFiles>,
    allow_osc_title: bool,
    cx: &mut Context<TerminalHost>,
) -> Entity<TerminalView> {
    let (font, font_size, line_height) = terminal_font_metrics(cx);
    let (view_tx, view_rx) = mpsc::unbounded_channel();
    let session_for_events: Weak<TerminalSession> = Arc::downgrade(&session);
    let palette = palette_from_theme(cx);
    let view = k8s_term::local_terminal_view(
        session,
        view_rx,
        font.clone(),
        font_size,
        line_height,
        palette.clone(),
        cx,
    );
    host.palette = Some(palette);
    host.font = Some((font, font_size, line_height));
    host.focus = Some(view.read(cx).focus_handle());

    let sink = std::rc::Rc::clone(&host.sink);
    let task = cx.spawn(async move |this, cx| {
        while let Some(event) = events.recv().await {
            let view_open = match event {
                SessionEvent::Title(title) => {
                    if !allow_osc_title {
                        if let Some(session) = session_for_events.upgrade() {
                            session.ack_title();
                        }
                        continue;
                    }
                    cx.update(|cx| sink(TerminalEvent::Title(title.clone()), cx));
                    let open = view_tx.send(SessionEvent::Title(title)).is_ok();
                    if let Some(session) = session_for_events.upgrade() {
                        session.ack_title();
                    }
                    open
                }
                SessionEvent::Exited(status) => {
                    let event = TerminalEvent::Exited {
                        code: status.code,
                        signal: status.signal,
                    };
                    cx.update(|cx| sink(event, cx));
                    this.update(cx, |host, cx| {
                        host.session_exited(status);
                        cx.notify();
                    })
                    .ok();
                    view_tx.send(SessionEvent::Exited(status)).is_ok()
                }
                SessionEvent::Wakeup => {
                    let open = view_tx.send(SessionEvent::Wakeup).is_ok();
                    if !open && let Some(session) = session_for_events.upgrade() {
                        session.ack_wakeup();
                    }
                    open
                }
                SessionEvent::CursorBlinkingChanged => {
                    view_tx.send(SessionEvent::CursorBlinkingChanged).is_ok()
                }
                SessionEvent::ClipboardStore(kind, text) => view_tx
                    .send(SessionEvent::ClipboardStore(kind, text))
                    .is_ok(),
            };
            if !view_open {
                break;
            }
        }
        // The session ended or was canceled. Release its temporary files.
        drop(files);
    });
    host.events_task = Some(task);
    view
}

type ExecReader = Box<dyn AsyncRead + Send + Unpin>;
type ExecWriter = Box<dyn AsyncWrite + Send + Unpin>;

enum ExecPumpAction {
    Shutdown,
    Resize,
    Read(Option<std::io::Result<usize>>),
    Input(Cow<'static, [u8]>),
    InputClosed,
}

enum ExecWriteState {
    Shutdown,
    Pending(Vec<u8>),
    Missing,
    Failed,
    Written,
}

struct ExecInputState {
    queue: VecDeque<Cow<'static, [u8]>>,
    bytes: usize,
    closed: bool,
}

struct ExecInput {
    state: Mutex<ExecInputState>,
    notify: Notify,
    capacity: usize,
    max_bytes: usize,
}

impl ExecInput {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ExecInputState {
                queue: VecDeque::with_capacity(capacity),
                bytes: 0,
                closed: false,
            }),
            notify: Notify::new(),
            capacity,
            max_bytes: EXEC_PUMP_MAX_INPUT_BYTES,
        })
    }

    fn push(&self, bytes: Cow<'static, [u8]>) {
        if bytes.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return;
        }
        // The caller is the UI thread and the pump may be slow, so a full queue evicts the
        // oldest pending input instead of waiting. Waiting would freeze the window whenever
        // the remote process stops reading.
        while !state.queue.is_empty()
            && (state.queue.len() >= self.capacity
                || state.bytes.saturating_add(bytes.len()) > self.max_bytes)
        {
            if let Some(evicted) = state.queue.pop_front() {
                state.bytes = state.bytes.saturating_sub(evicted.len());
            }
        }
        state.bytes = state.bytes.saturating_add(bytes.len());
        state.queue.push_back(bytes);
        drop(state);
        self.notify.notify_one();
    }

    async fn recv(&self) -> Option<Cow<'static, [u8]>> {
        loop {
            let notified = self.notify.notified();
            let (bytes, closed) = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if let Some(bytes) = state.queue.pop_front() {
                    state.bytes = state.bytes.saturating_sub(bytes.len());
                    (Some(bytes), false)
                } else {
                    (None, state.closed)
                }
            };
            if let Some(bytes) = bytes {
                return Some(bytes);
            }
            if closed {
                return None;
            }
            notified.await;
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        drop(state);
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .bytes
    }
}

#[cfg(test)]
fn spawn_exec_pump(
    handle: &Handle,
    exec: ExecSession,
    output: TerminalOutput,
) -> Result<Arc<dyn TerminalIo>, String> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    spawn_exec_pump_with_shutdown(handle, exec, output, shutdown_tx, shutdown_rx)
}

fn spawn_exec_pump_with_shutdown(
    handle: &Handle,
    mut exec: ExecSession,
    output: TerminalOutput,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<Arc<dyn TerminalIo>, String> {
    let input = ExecInput::new(EXEC_PUMP_CHANNEL_CAPACITY);
    let pump_input = input.clone();
    let (resize_tx, mut resize_rx) = tokio::sync::watch::channel(None);

    handle.spawn(async move {
        let mut output = output;
        let mut buffer = vec![0u8; 8192];
        let mut stdin = exec.take_stdin();
        let mut stdout = exec.take_stdout();
        let mut pending_input = None;
        loop {
            if *shutdown_rx.borrow() {
                input.close();
                exec.cancel();
                return;
            }
            // Applied before the input write so a new grid reaches the remote process even
            // while a large paste is still being written.
            apply_latest_resize(&mut exec, &mut resize_rx);
            if let Some(bytes) = pending_input.take() {
                match write_stdin(&mut stdin, bytes, &mut shutdown_rx).await {
                    ExecWriteState::Pending(bytes) => {
                        pending_input = Some(bytes);
                        continue;
                    }
                    ExecWriteState::Shutdown => {
                        input.close();
                        exec.cancel();
                        return;
                    }
                    ExecWriteState::Failed => stdin = None,
                    ExecWriteState::Missing | ExecWriteState::Written => {}
                }
            }
            let action = tokio::select! {
                biased;
                _ = shutdown_rx.changed() => ExecPumpAction::Shutdown,
                // A resize interrupts a pending input write so the remote process sees the
                // newest grid instead of the one it had when the write started.
                _ = resize_rx.changed() => ExecPumpAction::Resize,
                action = next_exec_action(&input, &mut stdout, &mut buffer) => action,
            };
            match action {
                ExecPumpAction::Shutdown => {
                    input.close();
                    exec.cancel();
                    return;
                }
                ExecPumpAction::Resize => {
                    apply_latest_resize(&mut exec, &mut resize_rx);
                }
                ExecPumpAction::Read(Some(Ok(0))) | ExecPumpAction::Read(Some(Err(_))) => {
                    break;
                }
                ExecPumpAction::Read(Some(Ok(read))) => output.process(&buffer[..read]),
                ExecPumpAction::Read(None) => {}
                ExecPumpAction::Input(bytes) => {
                    let bytes = bytes.into_owned();
                    match write_stdin(&mut stdin, bytes, &mut shutdown_rx).await {
                        ExecWriteState::Pending(bytes) => pending_input = Some(bytes),
                        ExecWriteState::Shutdown => {
                            input.close();
                            exec.cancel();
                            return;
                        }
                        ExecWriteState::Failed => stdin = None,
                        ExecWriteState::Missing | ExecWriteState::Written => {}
                    }
                }
                ExecPumpAction::InputClosed => break,
            }
        }
        let status = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => None,
            status = exec.status() => Some(status),
        };
        let code = match status {
            Some(Ok(status)) => status.code,
            Some(Err(_)) => 1,
            None => {
                exec.cancel();
                return;
            }
        };
        output.child_exited(code);
    });

    Ok(Arc::new(ExecPump {
        input: pump_input,
        resize: resize_tx,
        shutdown: shutdown_tx,
    }))
}

/// Sends the newest requested grid to the remote process. The channel only keeps the
/// latest value, so a burst of window resizes collapses into one update.
fn apply_latest_resize(
    exec: &mut ExecSession,
    resize_rx: &mut tokio::sync::watch::Receiver<Option<TermSize>>,
) {
    if let Some(size) = *resize_rx.borrow_and_update()
        && let Ok(size) = alacritty_size(size)
    {
        let _ = exec.resize(size);
    }
}

async fn next_exec_action(
    input: &ExecInput,
    stdout: &mut Option<ExecReader>,
    buffer: &mut [u8],
) -> ExecPumpAction {
    tokio::select! {
        input = input.recv() => match input {
            Some(bytes) => ExecPumpAction::Input(bytes),
            None => ExecPumpAction::InputClosed,
        },
        read = read_stdout_with_timeout(stdout, buffer) => ExecPumpAction::Read(read),
    }
}

async fn read_stdout_with_timeout(
    stdout: &mut Option<ExecReader>,
    buffer: &mut [u8],
) -> Option<std::io::Result<usize>> {
    tokio::time::timeout(EXEC_PUMP_IO_TIMEOUT, read_stdout(stdout, buffer))
        .await
        .ok()
}

async fn read_stdout(stdout: &mut Option<ExecReader>, buffer: &mut [u8]) -> std::io::Result<usize> {
    match stdout.as_mut() {
        Some(stdout) => tokio::io::AsyncReadExt::read(&mut **stdout, buffer).await,
        None => std::future::pending().await,
    }
}

async fn write_stdin(
    stdin: &mut Option<ExecWriter>,
    mut bytes: Vec<u8>,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> ExecWriteState {
    let Some(stdin) = stdin.as_mut() else {
        return ExecWriteState::Missing;
    };
    while !bytes.is_empty() {
        if *shutdown_rx.borrow() {
            return ExecWriteState::Shutdown;
        }
        let write = tokio::time::timeout(
            EXEC_PUMP_IO_TIMEOUT,
            tokio::io::AsyncWriteExt::write(&mut **stdin, &bytes),
        );
        tokio::pin!(write);
        let result = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => return ExecWriteState::Shutdown,
            result = &mut write => result,
        };
        match result {
            Ok(Ok(0)) | Ok(Err(_)) => return ExecWriteState::Failed,
            Ok(Ok(written)) => {
                let written = written.min(bytes.len());
                bytes.drain(..written);
            }
            Err(_) => return ExecWriteState::Pending(bytes),
        }
    }
    ExecWriteState::Written
}

struct ExecPump {
    input: Arc<ExecInput>,
    resize: tokio::sync::watch::Sender<Option<TermSize>>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl TerminalIo for ExecPump {
    fn write(&self, bytes: Cow<'static, [u8]>) {
        self.input.push(bytes);
    }

    fn resize(&self, size: TermSize) {
        if size.to_u16().is_ok() {
            self.resize.send_replace(Some(size));
        }
    }

    fn try_resize(&self, size: TermSize) -> anyhow::Result<()> {
        size.to_u16()?;
        self.resize(size);
        Ok(())
    }

    fn shutdown(&self) {
        self.input.close();
        let _ = self.shutdown.send(true);
    }
}

impl Drop for ExecPump {
    fn drop(&mut self) {
        self.input.close();
        let _ = self.shutdown.send(true);
    }
}

fn alacritty_size(size: TermSize) -> anyhow::Result<kube::api::TerminalSize> {
    let (width, height) = size.to_u16()?;
    Ok(kube::api::TerminalSize { width, height })
}

/// Renders the connection and failure states for the Dock.
///
/// The three states used to be three hand-written blocks of muted text, all at the same size,
/// weight and colour, with no glyph and no control: `Failed` and `Disconnected` were
/// byte-identical apart from two words, and the recovery path was a sentence telling the reader to
/// go and find a menu somewhere else. All three now share one frame - a severity glyph from the
/// shared health vocabulary, a body title, one metadata line of reason, and, where a retry exists,
/// a real button.
///
/// `panels::common::empty_state_with_action` is the frame the rest of the app uses, but it is
/// `pub(super)` inside `k8s-ui` and this panel is implemented in `k8s-app`, so the structure is
/// mirrored here rather than shared. The vocabulary is shared: the glyph and the severity come
/// from `k8s_ui::design`, and the progress bar uses the same `Role::ProgressIndicator` contract as
/// the panels that use the shared empty state.
impl Render for TerminalHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_palette(cx);
        self.sync_font(cx);
        if self.focus_requested && self.focus_target().is_some() {
            self.focus_requested = false;
            cx.on_next_frame(window, |host, window, cx| host.focus_now(window, cx));
        }
        let panel_focus = self.panel_focus.clone();
        match &self.phase {
            Phase::Ready(view) => div().size_full().child(view.clone()).into_any_element(),
            // `Role::Status` is a polite live region, so the wait is announced when the phase
            // changes. The accessibility id is shared with the failure panel below, so the
            // transition is one update on one node instead of a node that appears and vanishes.
            Phase::Connecting => phase_panel(
                "terminal-connecting",
                Role::Status,
                PhaseView::Waiting,
                &panel_focus,
                cx,
            ),
            Phase::Failed(reason) => phase_panel(
                "terminal-failure",
                Role::Alert,
                PhaseView::Failed {
                    reason: reason.clone(),
                    severity: design::Severity::Error,
                },
                &panel_focus,
                cx,
            ),
            Phase::Disconnected(reason) => phase_panel(
                "terminal-disconnected",
                Role::Status,
                PhaseView::Ended {
                    reason: reason.clone(),
                    severity: design::Severity::Warning,
                },
                &panel_focus,
                cx,
            ),
        }
    }
}

/// What a phase panel says, and what it can offer.
///
/// The three states share a frame but not their copy: a wait is not a failure, and a session that
/// exited is not one that could not start. The two need different next steps, so they are two
/// variants rather than one string with two words swapped.
enum PhaseView {
    /// A real wait, so it gets a progress indicator. `loading.md > Best practices` asks a wait that
    /// lasts a moment or two to show that it is one, and a line of text does not.
    Waiting,
    /// A session that could not be opened.
    Failed {
        reason: String,
        severity: design::Severity,
    },
    /// A session that ran and stopped. Same frame as a failure and one severity apart: an exit is
    /// not an error.
    Ended {
        reason: String,
        severity: design::Severity,
    },
}

impl PhaseView {
    /// The accessible name of the state.
    fn label(&self) -> &'static str {
        match self {
            Self::Waiting => TERMINAL_CONNECTING_TITLE,
            Self::Failed { .. } => TERMINAL_FAILED_TITLE,
            Self::Ended { .. } => TERMINAL_ENDED_TITLE,
        }
    }

    /// Whether the state can be recovered from where it is shown.
    ///
    /// A wait cannot: nothing has gone wrong yet, and offering Restart would invite a reader to
    /// cancel a connection that is about to succeed. A failure and an exit both can, and both run
    /// the same restart.
    fn offers_restart(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Ended { .. })
    }

    /// The state, the reason, and the recovery path, in the order a reader needs them.
    fn aria_description(&self) -> String {
        match self {
            Self::Waiting => TERMINAL_CONNECTING_DESCRIPTION.to_owned(),
            Self::Failed { reason, .. } => {
                format!("{TERMINAL_FAILED_DESCRIPTION} {reason}")
            }
            Self::Ended { reason, .. } => format!("{TERMINAL_ENDED_DESCRIPTION} {reason}"),
        }
    }
}

/// One connection or failure state.
///
/// The reason is the only part that can be long, so it keeps its own measure instead of spanning
/// the window, and it is the only line that changes with the phase.
fn phase_panel(
    id: &'static str,
    role: Role,
    view: PhaseView,
    focus: &FocusHandle,
    cx: &mut Context<TerminalHost>,
) -> AnyElement {
    let colors = cx.theme().colors();
    let label = view.label();
    let description = view.aria_description();
    // Read before the match, because the arms move the reason out of `view`.
    let offers_restart = view.offers_restart();
    let mut panel = v_flex()
        .id(id)
        .debug_selector(|| id.to_owned())
        .accessibility_id(TERMINAL_PHASE_NODE)
        .size_full()
        .min_h(px(0.))
        .flex_col()
        .items_center()
        .justify_center()
        .gap(design::space::SM)
        .px(design::space::XL)
        .py(design::space::LG)
        .track_focus(focus)
        .tab_stop(true)
        .role(role)
        .aria_label(label)
        .aria_description(description);
    match view {
        PhaseView::Waiting => {
            panel = panel
                .child(loading_bar(cx))
                .child(state_title(label, colors.text));
        }
        PhaseView::Failed { reason, severity } | PhaseView::Ended { reason, severity } => {
            panel = panel
                .child(health_marker(severity, cx))
                .child(state_title(label, colors.text))
                .child(state_reason(&reason, colors.text_muted));
            if offers_restart {
                panel = panel.child(restart_button());
            }
        }
    }
    panel.into_any_element()
}

/// The severity glyph, solved against the surface the state panel sits on.
///
/// The state panel is inside the Dock, whose background is `panel_background`, not the canvas the
/// default marker colour assumes.
fn health_marker(severity: design::Severity, cx: &App) -> AnyElement {
    let background = cx.theme().colors().panel_background.alpha(1.0);
    Icon::new(design::health_icon(severity))
        .size(IconSize::Custom(rems_from_px(f32::from(
            design::size::ICON_LARGE,
        ))))
        .color(Color::Custom(severity.marker_on(cx, background)))
        .into_any_element()
}

/// The body-size line that names the state.
fn state_title(text: &'static str, color: Hsla) -> AnyElement {
    div()
        .flex_none()
        .text_size(rems_from_px(f32::from(design::text::BODY)))
        .line_height(rems_from_px(f32::from(design::text::BODY_LINE_HEIGHT)))
        .text_color(color)
        .child(SharedString::from(text))
        .into_any_element()
}

/// The one metadata line of reason. It keeps the shared reading measure so a long reason wraps
/// instead of spanning the window.
fn state_reason(reason: &str, color: Hsla) -> AnyElement {
    div()
        .flex_none()
        .max_w(px(TERMINAL_REASON_MEASURE))
        .whitespace_normal()
        .text_align(TextAlign::Center)
        .text_size(rems_from_px(f32::from(design::text::METADATA)))
        .line_height(rems_from_px(f32::from(design::text::METADATA_LINE_HEIGHT)))
        .text_color(color)
        .child(SharedString::from(reason.to_owned()))
        .into_any_element()
}

/// The recovery control.
///
/// It dispatches the same restart as the `Restart` entry in the Dock's Terminal actions menu. That
/// handler lives in the Dock, in the other crate, so the panel cannot call it; the action is the
/// one channel that crosses the boundary without either side reaching into the other, and the Dock
/// runs the same code path the menu does.
fn restart_button() -> AnyElement {
    div()
        .id("terminal-restart-control")
        .debug_selector(|| "terminal-restart".to_owned())
        .flex_none()
        .child(
            Button::new("terminal-restart", "Restart")
                .style(ButtonStyle::Tinted(TintColor::Accent))
                .size(ButtonSize::Medium)
                .tab_index(TERMINAL_RESTART_TAB_INDEX)
                .tooltip(Tooltip::text("Reconnect this terminal session."))
                .aria_label("Restart terminal session")
                .on_click(|_, window, cx| {
                    window.dispatch_action(
                        Box::new(k8s_ui::panels::dock::RestartTerminalSession),
                        cx,
                    );
                }),
        )
        .into_any_element()
}

/// The progress indicator for a real wait.
///
/// `Role::ProgressIndicator` rather than a rotating glyph: a 32px spinner needs a 32px box next to
/// two lines of text, and a short Dock clips it to a sliver, while a bar is a hairline tall. The
/// sweep is repeated motion, so it stops when the user asked for less motion, and the static frame
/// keeps the bar's shape and its start - the same contract `panels::common` keeps.
fn loading_bar(cx: &App) -> AnyElement {
    let accent = Color::Accent.color(cx);
    let fill: AnyElement = if k8s_ui::settings::reduce_motion_enabled(cx) {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .rounded_full()
            .bg(accent)
            .w(relative(LOADING_FILL_MIN))
            .into_any_element()
    } else {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .rounded_full()
            .bg(accent)
            .w(relative(LOADING_FILL_MIN))
            .with_animation(
                ElementId::Name(SharedString::from(TERMINAL_LOADING_BAR_ID)),
                Animation::new(design::motion::LOADING).repeat(),
                move |fill, phase| fill.w(relative(phase.clamp(LOADING_FILL_MIN, 1.))),
            )
            .into_any_element()
    };
    div()
        .id("terminal-progress")
        .debug_selector(|| "terminal-progress".to_owned())
        .role(Role::ProgressIndicator)
        .flex_none()
        .relative()
        .w(design::size::UPDATE_PROGRESS)
        .max_w_full()
        .h(design::space::XS)
        .child(
            div()
                .absolute()
                .inset_0()
                .rounded_full()
                .bg(accent)
                .opacity(LOADING_TRACK_ALPHA),
        )
        .child(fill)
        .into_any_element()
}

// Port forward

fn spawn_forward(
    registry: Option<&Arc<ClusterRegistry>>,
    handle: Option<&Handle>,
    request: ForwardRequest,
    _cx: &mut App,
) -> Result<StartedForward, String> {
    let cluster = cluster_for(registry, request.context.as_deref())?;
    let handle = handle
        .cloned()
        .ok_or_else(|| "Not connected to a cluster. Connect to a cluster first.".to_owned())?;
    let client = cluster.client().clone();
    let namespace = request.namespace.clone();
    let name = request.name.to_string();
    let remote_port = request.remote_port;
    // The port the user asked for, so a taken port is bound to something else and reported
    // instead of being dropped.
    let local_port = request.local_port;
    let resource = k8s_ui::table_view::pods_resource();
    let (binding_tx, binding) = tokio::sync::oneshot::channel();
    let (error_tx, errors) = mpsc::unbounded_channel();
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel::<()>();

    handle.spawn(async move {
        // The requested port outlives the select below: `start_with_local_ports` keeps the slice
        // across its own awaits, so a temporary array in the call would not live long enough.
        let requested_local_ports = [local_port];
        let result = tokio::select! {
            result = ops::PortForwardSession::start_with_local_ports(
                &client,
                &resource,
                namespace.as_deref(),
                &name,
                vec![remote_port],
                &requested_local_ports,
            ) => result,
            _ = stop_rx.recv() => {
                let _ = binding_tx.send(Err("Port forward was canceled.".to_owned()));
                return;
            }
        };
        let mut session = match result {
            Ok(session) => session,
            Err(error) => {
                let _ = binding_tx.send(Err(error.to_string()));
                return;
            }
        };
        // The dialog shows the substitution, so the process log names the port that answers now.
        for fallback in session.fallbacks() {
            eprintln!("k8s-gpui: port forward: {}", fallback.notice());
        }
        let Some(local_port) = session.local_ports().first().copied() else {
            let _ = binding_tx.send(Err(
                "The port forward did not bind a local port. Check the port-forward request."
                    .to_owned(),
            ));
            return;
        };
        if binding_tx.send(Ok(local_port)).is_err() {
            return;
        }
        tokio::select! {
            _ = stop_rx.recv() => {}
            error = session.next_error() => if let Some(error) = error {
                let _ = error_tx.send(format!(
                    "Port {} failed: {}. Check the port-forward request.",
                    error.port, error.reason
                ));
            },
        }
    });

    Ok(StartedForward {
        handle: Box::new(KubeForward {
            stop: Some(stop_tx),
        }),
        binding,
        errors,
    })
}

struct KubeForward {
    stop: Option<mpsc::UnboundedSender<()>>,
}

impl KubeForward {
    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl k8s_ui::panels::terminal::ForwardHandle for KubeForward {
    fn stop(&mut self) {
        self.stop();
    }
}

impl Drop for KubeForward {
    fn drop(&mut self) {
        self.stop();
    }
}

// Local shell files

/// Temporary files for one local session. Dropping it removes credentials.
struct SessionFiles {
    dir: PathBuf,
    /// Sequence number of this session, for diagnostic lines. The directory path is not one of
    /// them: it names the file that holds the credentials, and a path in a log outlives the
    /// session that made it.
    sequence: u64,
    env: Vec<(String, String)>,
    /// Keeps the temporary kubeconfig alive until the session ends.
    _kubeconfig: k8s_core::kubectl_shell::SessionKubeconfig,
}

impl SessionFiles {
    /// Creates a private kubeconfig and session environment.
    fn prepare(
        cluster: &Cluster,
        snapshot: &kube::config::Kubeconfig,
        namespace: Option<&str>,
    ) -> Result<Self, String> {
        let sequence = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("k8s-gpui-term-{}-{sequence}", std::process::id()));
        ensure_private_dir(&dir).map_err(|error| error.to_string())?;
        let kubeconfig = match namespace {
            None => k8s_core::kubectl_shell::write_session_kubeconfig_from(cluster, snapshot, &dir),
            Some(namespace) if namespace == ALL_NAMESPACES => {
                k8s_core::kubectl_shell::write_session_kubeconfig_from_all_namespaces(
                    cluster, snapshot, &dir,
                )
            }
            Some(namespace) => {
                k8s_core::kubectl_shell::write_session_kubeconfig_from_with_namespace(
                    cluster,
                    snapshot,
                    &dir,
                    Some(namespace),
                )
            }
        };
        let kubeconfig = match kubeconfig {
            Ok(kubeconfig) => kubeconfig,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(error.to_string());
            }
        };
        if kubeconfig.path().to_str().is_none() {
            let error = "The temporary kubeconfig path is not valid Unicode.".to_owned();
            drop(kubeconfig);
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        let env_namespace = namespace.unwrap_or_else(|| cluster.client().default_namespace());
        let env = k8s_core::kubectl_shell::shell_env(cluster, &kubeconfig, env_namespace);
        Ok(Self {
            dir,
            sequence,
            env,
            _kubeconfig: kubeconfig,
        })
    }

    fn env(&self) -> HashMap<String, String> {
        self.env.iter().cloned().collect()
    }

    /// Names this session for a diagnostic line.
    fn trace_label(&self) -> String {
        session_label(self.sequence)
    }
}

impl Drop for SessionFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        eprintln!("k8s-gpui: {} files removed", session_label(self.sequence));
    }
}

/// Names a local session for a diagnostic line.
///
/// The session directory holds a private kubeconfig, so a diagnostic line carries the sequence
/// number and nothing else. No path, and nothing from the file behind it: cluster credentials do
/// not belong in a terminal that outlives the session.
fn session_label(sequence: u64) -> String {
    format!("local session {sequence}")
}

#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    k8s_core::atomic_file::create_private_dir_all(dir)
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).create(dir)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use k8s_core::cluster::{Cluster, ClusterRegistry};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[test]
    fn terminal_minimum_contrast_matches_accessibility_modes() {
        assert_eq!(terminal_minimum_contrast(false), 4.5);
        assert_eq!(terminal_minimum_contrast(true), 7.0);
    }

    #[test]
    fn initial_cell_size_tracks_font_size() {
        assert_eq!(initial_cell_size(px(12.), px(18.)), (8, 18));
    }

    #[test]
    fn exec_size_rejects_u16_overflow() {
        let size = TermSize::new(80, 24);
        let converted = alacritty_size(size).expect("valid terminal size");
        assert_eq!(converted.width, 80);
        assert_eq!(converted.height, 24);
        assert!(
            alacritty_size(TermSize {
                columns: usize::from(u16::MAX) + 1,
                screen_lines: 24,
            })
            .is_err()
        );
    }

    #[test]
    fn focus_moves_to_the_panel_after_the_session_ends() {
        assert_eq!(focus_owner(&Phase::Connecting), FocusOwner::Pending);
        assert_eq!(
            focus_owner(&Phase::Failed("boom".to_owned())),
            FocusOwner::Panel
        );
        assert_eq!(
            focus_owner(&Phase::Disconnected("boom".to_owned())),
            FocusOwner::Panel,
            "a finished session must leave a rendered focus target"
        );
    }

    /// A failure says whether it happened before or after the session attached. The two need
    /// different next steps, and the reason on its own does not tell them apart.
    #[test]
    fn a_failure_line_says_which_phase_failed() {
        assert_eq!(phase_label(&Phase::Connecting), "connecting");
        assert_eq!(
            phase_label(&Phase::Failed("x".to_owned())),
            "already failed"
        );
        assert_eq!(
            phase_label(&Phase::Disconnected("x".to_owned())),
            "already ended"
        );
    }

    /// The session directory holds a private kubeconfig, so a diagnostic line names the session and
    /// nothing else. A path, or any part of the file behind it, would put cluster credentials in a
    /// terminal that outlives the session.
    #[test]
    fn a_session_diagnostic_line_carries_no_kubeconfig_material() {
        let label = session_label(7);
        assert_eq!(label, "local session 7");
        for secret in [
            "/",
            "apiVersion",
            "clusters:",
            "client-certificate",
            "token",
            "password",
        ] {
            assert!(
                !label.contains(secret),
                "a session diagnostic line must not carry {secret}: {label}"
            );
        }
    }

    #[test]
    fn terminal_exit_reason_states_what_happened() {
        let code = exit_reason(SessionExit::from_code(1));
        assert!(code.contains("shell exited with code 1"));
        assert!(
            exit_reason(SessionExit {
                code: None,
                signal: Some(9)
            })
            .contains("signal 9")
        );
        let ended = exit_reason(SessionExit {
            code: None,
            signal: None,
        });
        assert_eq!(ended, "The shell ended.");
    }

    /// The three states are three different facts. `DESIGN.md` §4 Terminal keeps "connecting",
    /// "session failed", and "exited" as separate boundaries, and the panel used to draw the last
    /// two identically apart from the words "failed." and "ended.".
    #[test]
    fn every_session_phase_states_its_own_boundary() {
        let failed = PhaseView::Failed {
            reason: "The cluster refused the exec stream.".to_owned(),
            severity: design::Severity::Error,
        };
        let ended = PhaseView::Ended {
            reason: "The shell exited with code 1.".to_owned(),
            severity: design::Severity::Warning,
        };
        let waiting = PhaseView::Waiting;

        let names = [waiting.label(), failed.label(), ended.label()];
        assert_eq!(names[0], TERMINAL_CONNECTING_TITLE);
        assert_eq!(names[1], TERMINAL_FAILED_TITLE);
        assert_eq!(names[2], TERMINAL_ENDED_TITLE);
        for pair in [(0, 1), (0, 2), (1, 2)] {
            assert_ne!(
                names[pair.0], names[pair.1],
                "two phases share one name, so a reader cannot tell them apart"
            );
        }
        assert_ne!(failed.aria_description(), ended.aria_description());
        assert!(
            failed
                .aria_description()
                .contains("The cluster refused the exec stream."),
            "the spoken description carries the reason, because the sentence the reader sees is \
             the only one that says what happened"
        );
        assert!(
            ended
                .aria_description()
                .contains("The shell exited with code 1.")
        );
    }

    /// An exit is not an error. The two states share a frame and differ by one severity, and the
    /// severity is what says whether the session could not start or ran and stopped.
    #[test]
    fn a_session_that_exited_is_not_the_same_severity_as_one_that_failed() {
        let failed = PhaseView::Failed {
            reason: "boom".to_owned(),
            severity: design::Severity::Error,
        };
        let ended = PhaseView::Ended {
            reason: "boom".to_owned(),
            severity: design::Severity::Warning,
        };
        let (failed_severity, ended_severity) = match (failed, ended) {
            (
                PhaseView::Failed {
                    severity: failed, ..
                },
                PhaseView::Ended {
                    severity: ended, ..
                },
            ) => (failed, ended),
            _ => unreachable!("the two states under test are a failure and an exit"),
        };
        assert_ne!(failed_severity, ended_severity);
        assert_ne!(
            design::health_icon(failed_severity),
            design::health_icon(ended_severity),
            "the two states must not share a glyph either"
        );
    }

    /// A wait cannot be recovered from, because nothing has gone wrong yet. A failure and an exit
    /// both can, and both offer the same control, so the two states are one frame.
    #[test]
    fn only_the_states_that_ended_offer_a_restart() {
        assert!(!PhaseView::Waiting.offers_restart());
        assert!(
            PhaseView::Failed {
                reason: "boom".to_owned(),
                severity: design::Severity::Error,
            }
            .offers_restart()
        );
        assert!(
            PhaseView::Ended {
                reason: "boom".to_owned(),
                severity: design::Severity::Warning,
            }
            .offers_restart()
        );
    }

    /// The recovery path is a control, not a sentence. The old copy told the reader to open a menu
    /// somewhere else; a reader mid-incident has to be able to act from where the failure is, and
    /// every word the state speaks has to be about the state.
    #[test]
    fn no_phase_sends_the_reader_looking_for_the_control() {
        for view in [
            PhaseView::Waiting,
            PhaseView::Failed {
                reason: "boom".to_owned(),
                severity: design::Severity::Error,
            },
            PhaseView::Ended {
                reason: "boom".to_owned(),
                severity: design::Severity::Warning,
            },
        ] {
            for line in [view.label(), view.aria_description().as_str()] {
                assert!(
                    !line.contains("Terminal actions")
                        && !line.contains("choose Restart")
                        && !line.contains("reconnect this session"),
                    "the state panel points at a menu instead of offering the action: {line}"
                );
            }
        }
    }

    #[test]
    fn login_shell_flag_is_unix_only() {
        assert_eq!(local_shell_args(true), vec![OsString::from("-l")]);
        assert!(local_shell_args(false).is_empty());
    }

    #[test]
    fn local_terminal_resolution_fails_closed() {
        let registry = Arc::new(ClusterRegistry::default());
        assert_eq!(
            cluster_for(None, Some("missing")).err().as_deref(),
            Some("Not connected to a cluster. Connect to a cluster first.")
        );
        assert_eq!(
            cluster_for(Some(&registry), None).err().as_deref(),
            Some("Not connected to a cluster. Connect to a cluster first.")
        );
        assert_eq!(
            cluster_for(Some(&registry), Some("missing"))
                .err()
                .as_deref(),
            Some(
                "Cluster `missing` is no longer available. Refresh the cluster list and try again."
            )
        );
    }

    #[test]
    fn local_namespace_prefers_request_and_falls_back_to_all_namespaces() {
        let selected = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("kind-dev".to_owned()),
            namespace: Some("team-a".to_owned()),
        };
        let all = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("kind-dev".to_owned()),
            namespace: None,
        };
        let empty = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("kind-dev".to_owned()),
            namespace: Some(String::new()),
        };

        assert_eq!(local_namespace(&selected), "team-a");
        assert_eq!(local_namespace(&all), ALL_NAMESPACES);
        assert_eq!(local_namespace(&empty), ALL_NAMESPACES);
    }

    #[tokio::test]
    async fn local_terminal_environment_uses_request_namespace() {
        let kubeconfig = kube::config::Kubeconfig::from_yaml(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: test
  cluster:
    server: http://127.0.0.1:65535
contexts:
- name: test
  context:
    cluster: test
    user: test
    namespace: default
users:
- name: test
  user: {}
current-context: test
"#,
        )
        .expect("parse kubeconfig");
        let registry = ClusterRegistry::from_kubeconfig(kubeconfig).await;
        let cluster = registry.clusters().first().expect("test cluster");
        let snapshot = registry.kubeconfig();
        let selected = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("test".to_owned()),
            namespace: Some("team-a".to_owned()),
        };
        let files = SessionFiles::prepare(cluster, &snapshot, Some(local_namespace(&selected)))
            .expect("selected namespace environment");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&files.dir)
                .expect("session directory metadata")
                .permissions()
                .mode()
                & 0o777;
            let file_mode = std::fs::metadata(files._kubeconfig.path())
                .expect("session kubeconfig metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
        assert_eq!(
            files
                .env
                .iter()
                .find(|(key, _)| key == "K8S_GPUI_NAMESPACE")
                .map(|(_, value)| value.as_str()),
            Some("team-a")
        );
        let selected_written =
            kube::config::Kubeconfig::read_from(files._kubeconfig.path()).expect("read selected");
        assert_eq!(
            selected_written.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.namespace.as_deref()),
            Some("team-a")
        );
        drop(files);

        let all = TerminalRequest {
            kind: TerminalKind::Local,
            context: Some("test".to_owned()),
            namespace: None,
        };
        let files = SessionFiles::prepare(cluster, &snapshot, Some(local_namespace(&all)))
            .expect("all namespaces environment");
        assert_eq!(
            files
                .env
                .iter()
                .find(|(key, _)| key == "K8S_GPUI_NAMESPACE")
                .map(|(_, value)| value.as_str()),
            Some(ALL_NAMESPACES)
        );
        let all_written =
            kube::config::Kubeconfig::read_from(files._kubeconfig.path()).expect("read all");
        assert_eq!(
            all_written.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.namespace.as_deref()),
            None
        );
        drop(files);

        let files = SessionFiles::prepare(cluster, &snapshot, None)
            .expect("preserve context namespace environment");
        let preserved =
            kube::config::Kubeconfig::read_from(files._kubeconfig.path()).expect("read preserved");
        assert_eq!(
            preserved.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.namespace.as_deref()),
            Some("default")
        );
        assert_eq!(
            files
                .env
                .iter()
                .find(|(key, _)| key == "K8S_GPUI_NAMESPACE")
                .map(|(_, value)| value.as_str()),
            Some("default")
        );
    }

    #[tokio::test]
    async fn exec_pump_input_stays_bounded_and_keeps_order() {
        let input = ExecInput::new(2);
        for index in 0..6 {
            input.push(Cow::Owned(index.to_string().into_bytes()));
        }

        assert!(input.queued_bytes() <= EXEC_PUMP_MAX_INPUT_BYTES);
        let mut received = Vec::new();
        while let Some(bytes) = input.recv().await {
            received.push(bytes.into_owned());
            if received.len() == 2 {
                break;
            }
        }

        assert_eq!(
            received,
            vec![b"4".to_vec(), b"5".to_vec()],
            "the newest input is kept and the queue stays bounded"
        );
    }

    #[tokio::test]
    async fn exec_pump_input_applies_backpressure_and_preserves_order() {
        let input = ExecInput::new(1);
        let (resize_tx, mut resize_rx) = tokio::sync::watch::channel(None);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let pump = ExecPump {
            input: input.clone(),
            resize: resize_tx,
            shutdown: shutdown_tx,
        };

        pump.write(Cow::Borrowed(b"first"));
        let receiver = input.clone();
        let first = tokio::spawn(async move { receiver.recv().await });
        tokio::task::yield_now().await;
        pump.write(Cow::Borrowed(b"second"));
        pump.resize(TermSize::new(80, 24));
        pump.resize(TermSize::new(100, 30));

        let first = first.await.expect("first input").expect("input queue open");
        assert_eq!(first.as_ref(), b"first");
        assert_eq!(first.as_ptr(), b"first".as_ptr());
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), input.recv())
                .await
                .expect("second input")
                .expect("input queue open")
                .as_ref(),
            b"second"
        );
        assert_eq!(
            resize_rx.borrow_and_update().clone(),
            Some(TermSize::new(100, 30))
        );
    }

    #[test]
    fn exec_pump_resize_only_keeps_the_latest_size() {
        let (resize_tx, mut resize_rx) = tokio::sync::watch::channel(None);
        let (shutdown_tx, _shutdown_rx) = tokio::sync::watch::channel(false);
        let pump = ExecPump {
            input: ExecInput::new(1),
            resize: resize_tx,
            shutdown: shutdown_tx,
        };

        pump.resize(TermSize::new(80, 24));
        pump.resize(TermSize::new(120, 40));

        assert_eq!(
            resize_rx.borrow_and_update().clone(),
            Some(TermSize::new(120, 40))
        );
        assert!(
            pump.try_resize(TermSize {
                columns: usize::from(u16::MAX) + 1,
                screen_lines: 40,
            })
            .is_err(),
            "an unrepresentable grid never reaches the remote process"
        );
    }

    #[tokio::test]
    async fn exec_stdin_write_has_a_bounded_wait_without_losing_bytes() {
        let (writer, _reader) = tokio::io::duplex(1);
        let mut stdin = Some(Box::new(writer) as ExecWriter);
        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        let state = write_stdin(&mut stdin, vec![0; 64], &mut shutdown_rx).await;

        assert!(matches!(state, ExecWriteState::Pending(bytes) if bytes.len() == 63));
    }

    #[test]
    fn exec_pump_drop_signals_shutdown() {
        let input = ExecInput::new(1);
        let (resize_tx, _resize_rx) = tokio::sync::watch::channel(None);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        {
            let _pump = ExecPump {
                input,
                resize: resize_tx,
                shutdown: shutdown_tx,
            };
        }
        assert!(*shutdown_rx.borrow());
    }

    #[test]
    fn forward_handle_stops_once() {
        let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
        let mut handle = KubeForward {
            stop: Some(stop_tx),
        };

        handle.stop();
        handle.stop();

        assert!(stop_rx.try_recv().is_ok());
        assert!(stop_rx.try_recv().is_err());
    }

    /// Returns the kind development cluster when it is available.
    async fn dev_registry() -> Option<Arc<ClusterRegistry>> {
        let registry = Arc::new(ClusterRegistry::load_default().await.ok()?);
        registry
            .clusters()
            .iter()
            .any(|cluster| cluster.name() == "kind-k8s-gpui-dev")
            .then_some(registry)
    }

    fn dev_cluster(registry: &ClusterRegistry) -> &Cluster {
        registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == "kind-k8s-gpui-dev")
            .expect("dev cluster")
    }

    /// Polls terminal text until it contains the expected text.
    async fn wait_for_text(session: &TerminalSession, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            session.select_all();
            let text = session.selection_to_string().unwrap_or_default();
            if text.contains(needle) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }

    /// Runs a local PTY with a temporary kubeconfig and leaves the global file unchanged.
    #[tokio::test]
    #[ignore = "requires a kind cluster and kubectl. It starts a login shell and runs kubectl get pods"]
    async fn kind_local_pty_runs_kubectl_with_session_kubeconfig() {
        let Some(registry) = dev_registry().await else {
            return;
        };
        let cluster = dev_cluster(&registry);
        let global = std::env::var_os("HOME").map(|home| {
            let path = PathBuf::from(home).join(".kube").join("config");
            std::fs::read(path).ok()
        });
        let snapshot = registry.kubeconfig();
        let files = SessionFiles::prepare(cluster, &snapshot, Some("default"))
            .expect("temporary kubeconfig");
        let env = files.env();
        let (cell_width, cell_height) = initial_cell_size(px(12.), px(18.));
        let (session, _events) = TerminalSession::local_command(
            None,
            local_shell_args(cfg!(unix)),
            std::env::var_os("HOME").map(PathBuf::from),
            env,
            true,
            TermSize::new(120, 30),
            cell_width,
            cell_height,
            SCROLLBACK,
        )
        .expect("local PTY");

        session.write(b"kubectl get pods -n default --no-headers\n".to_vec());
        let seen = wait_for_text(&session, "q3-nginx", Duration::from_secs(30)).await;
        if !seen {
            session.select_all();
            eprintln!(
                "[pty] q3-nginx was not found in the output:\n{}",
                session.selection_to_string().unwrap_or_default()
            );
        }
        assert!(
            seen,
            "The local shell must run kubectl with the temporary kubeconfig"
        );

        // Keep the global kubeconfig read-only.
        if let (Some(home), Some(before)) = (std::env::var_os("HOME"), global) {
            let path = PathBuf::from(home).join(".kube").join("config");
            let after = std::fs::read(&path).expect("read the global kubeconfig");
            assert_eq!(
                Some(after),
                before,
                "The global kubeconfig must not change (read-only source file)"
            );
        }
    }

    /// Runs an exec command through the terminal transport.
    #[tokio::test]
    #[ignore = "requires a kind cluster and the default/q3-nginx Pod"]
    async fn kind_exec_session_runs_command_through_terminal() {
        let Some(registry) = dev_registry().await else {
            return;
        };
        let cluster = dev_cluster(&registry);
        let resource = k8s_ui::table_view::pods_resource();
        let options = ExecOptions {
            container: None,
            tty: true,
            command: vec![EXEC_SHELL.to_owned()],
            env: Vec::new(),
        };
        let exec = ops::exec(
            cluster.client(),
            &resource,
            Some("default"),
            "q3-nginx",
            &options,
        )
        .await
        .expect("exec session");
        let handle = Handle::current();
        let (session, _events) = TerminalSession::from_transport_facade(
            TermSize::new(80, 24),
            SCROLLBACK,
            move |output| {
                spawn_exec_pump(&handle, exec, output).map_err(|reason| anyhow::anyhow!(reason))
            },
        )
        .expect("terminal session");

        session.write(b"echo q3-exec-ok\n".to_vec());
        let seen = wait_for_text(&session, "q3-exec-ok", Duration::from_secs(20)).await;
        assert!(seen, "The exec terminal must show command output");
    }

    /// Verifies a port-forward response from the test pod.
    #[tokio::test]
    #[ignore = "requires a kind cluster and the default/q3-nginx Pod"]
    async fn kind_port_forward_serves_nginx() {
        let Some(registry) = dev_registry().await else {
            return;
        };
        let cluster = dev_cluster(&registry);
        let resource = k8s_ui::table_view::pods_resource();
        let session = ops::PortForwardSession::start(
            cluster.client(),
            &resource,
            Some("default"),
            "q3-nginx",
            vec![80],
        )
        .await
        .expect("port forward");
        let local = session.local_ports()[0];

        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", local))
            .await
            .expect("connect to the local forward port");
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(15), stream.read_to_end(&mut response))
            .await
            .expect("read response without timeout")
            .expect("read response");
        let text = String::from_utf8_lossy(&response);
        eprintln!("[forward] localhost:{local} -> {} bytes", response.len());
        assert!(
            text.contains("HTTP/1.1 200") || text.contains("HTTP/1.0 200"),
            "nginx must return 200: {text}"
        );
        session.stop();
    }
}
