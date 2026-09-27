//! Terminal state, I/O transport, and session events.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermMode};
use alacritty_terminal::tty::{Options, Shell};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use anyhow::Context as _;
use tokio::sync::{mpsc, watch};

use crate::io::{LocalPty, TerminalIo};

/// Terminal grid size in columns and rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TermSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns: columns.max(1),
            screen_lines: screen_lines.max(1),
        }
    }

    pub fn try_new(columns: usize, screen_lines: usize) -> anyhow::Result<Self> {
        Self::new(columns, screen_lines).checked()
    }

    pub fn checked(self) -> anyhow::Result<Self> {
        self.to_u16().map(|_| self)
    }

    pub fn to_u16(self) -> anyhow::Result<(u16, u16)> {
        if self.columns == 0 || self.screen_lines == 0 {
            return Err(anyhow::anyhow!(
                "Terminal dimensions must be greater than zero."
            ));
        }
        let columns =
            u16::try_from(self.columns).context("Terminal columns exceed the transport limit.")?;
        let screen_lines = u16::try_from(self.screen_lines)
            .context("Terminal rows exceed the transport limit.")?;
        Ok((columns, screen_lines))
    }

    pub(crate) fn window_size(
        self,
        cell_width: u16,
        cell_height: u16,
    ) -> anyhow::Result<WindowSize> {
        let (columns, screen_lines) = self.to_u16()?;
        Ok(WindowSize {
            num_lines: screen_lines,
            num_cols: columns,
            cell_width,
            cell_height,
        })
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

pub(crate) fn viewport_cursor_row(
    cursor_line: i32,
    display_offset: usize,
    screen_lines: usize,
) -> Option<usize> {
    let display_offset = i32::try_from(display_offset).ok()?;
    let row = cursor_line.checked_add(display_offset)?;
    let row = usize::try_from(row).ok()?;
    (row < screen_lines).then_some(row)
}

fn is_accessible_char(ch: char) -> bool {
    !ch.is_control()
}

fn accessible_line(text: &str) -> String {
    let mut line: String = text.chars().filter(|ch| is_accessible_char(*ch)).collect();
    while line.ends_with(' ') {
        line.pop();
    }
    line
}

pub(crate) fn accessible_text(text: &str) -> String {
    text.split('\n')
        .map(accessible_line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl SessionExit {
    pub fn from_code(code: i32) -> Self {
        Self {
            code: Some(code),
            signal: None,
        }
    }
}

impl From<ExitStatus> for SessionExit {
    fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt as _;
            status.signal()
        };
        Self {
            code: status.code(),
            #[cfg(not(unix))]
            signal: None,
            #[cfg(unix)]
            signal,
        }
    }
}

/// Events sent from the transport to the UI.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// Terminal content changed. Updates are coalesced.
    Wakeup,
    /// The window title changed.
    Title(String),
    /// The child process exited.
    Exited(SessionExit),
    /// The terminal changed the cursor blink request.
    CursorBlinkingChanged,
    /// OSC 52 requested a clipboard write. The text is decoded.
    ClipboardStore(ClipboardType, String),
}

pub const SESSION_EVENT_CAPACITY: usize = 64;
const MAX_PENDING_PTY_WRITES: usize = 64 * 1024;
const RESIZE_FAILURE_MESSAGE: &str = "Terminal resize failed. Restart the session to reconnect.";

/// Converts Alacritty events into session events and sends terminal replies.
#[derive(Clone)]
pub struct SessionListener {
    events: mpsc::Sender<SessionEvent>,
    io: Arc<OnceLock<Weak<dyn TerminalIo>>>,
    pending_writes: Arc<Mutex<Vec<Vec<u8>>>>,
    pending_space: Arc<Condvar>,
    pending_wakeup: Arc<AtomicBool>,
    pending_title: Arc<Mutex<Option<String>>>,
    title_queued: Arc<AtomicBool>,
    pending_exit: Arc<Mutex<Option<SessionExit>>>,
    exit_sent: Arc<AtomicBool>,
    exit_retry_started: Arc<AtomicBool>,
    event_send_lock: Arc<Mutex<()>>,
}

impl SessionListener {
    pub fn new(events: mpsc::Sender<SessionEvent>) -> Self {
        Self {
            events,
            io: Arc::new(OnceLock::new()),
            pending_writes: Arc::new(Mutex::new(Vec::new())),
            pending_space: Arc::new(Condvar::new()),
            pending_wakeup: Arc::new(AtomicBool::new(false)),
            pending_title: Arc::new(Mutex::new(None)),
            title_queued: Arc::new(AtomicBool::new(false)),
            pending_exit: Arc::new(Mutex::new(None)),
            exit_sent: Arc::new(AtomicBool::new(false)),
            exit_retry_started: Arc::new(AtomicBool::new(false)),
            event_send_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Binds the transport after session startup and forwards terminal replies.
    pub fn set_io(&self, io: &Arc<dyn TerminalIo>) {
        if self.io.set(Arc::downgrade(io)).is_err() {
            return;
        }
        let pending = {
            let mut pending = self
                .pending_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *pending)
        };
        if let Some(io) = self.io.get().and_then(Weak::upgrade) {
            for bytes in pending {
                if !bytes.is_empty() {
                    io.write(Cow::Owned(bytes));
                }
            }
        }
        self.pending_space.notify_all();
    }

    pub fn child_exited(&self, code: i32) {
        self.queue_exit(SessionExit::from_code(code));
    }

    pub(crate) fn transport_closed(&self) {
        self.queue_exit(SessionExit {
            code: None,
            signal: None,
        });
    }

    fn try_send_optional(&self, event: SessionEvent) -> Result<(), SessionEvent> {
        let _guard = self
            .event_send_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.events.max_capacity() > 1 && self.events.capacity() <= 1 {
            return Err(event);
        }
        self.events
            .try_send(event)
            .map_err(|error| error.into_inner())
    }

    fn try_send_wakeup(&self) -> bool {
        let _guard = self
            .event_send_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.events.try_send(SessionEvent::Wakeup).is_ok()
    }

    fn try_send_exit(&self, status: SessionExit) -> Result<(), SessionEvent> {
        let _guard = self
            .event_send_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.events
            .try_send(SessionEvent::Exited(status))
            .map_err(|error| error.into_inner())
    }

    fn spawn_exit_retry(&self) {
        if self.exit_retry_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let events = self.events.clone();
        let pending_exit = self.pending_exit.clone();
        let exit_sent = self.exit_sent.clone();
        let retry_started = self.exit_retry_started.clone();
        let result = std::thread::Builder::new()
            .name("PTY exit retry".to_owned())
            .spawn(move || {
                let status = pending_exit
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(status) = status
                    && events.blocking_send(SessionEvent::Exited(status)).is_ok()
                {
                    exit_sent.store(true, Ordering::Release);
                }
                retry_started.store(false, Ordering::Release);
            });
        if result.is_err() {
            self.exit_retry_started.store(false, Ordering::Release);
        }
    }

    fn queue_exit(&self, status: SessionExit) {
        if !self.exit_sent.load(Ordering::Acquire) {
            let mut pending = self
                .pending_exit
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.is_none() {
                *pending = Some(status);
            }
        }
        self.flush_exit();
    }

    fn flush_exit(&self) {
        if self.exit_sent.load(Ordering::Acquire) {
            return;
        }
        let mut pending = self
            .pending_exit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(status) = pending.take() else {
            return;
        };
        let retry = match self.try_send_exit(status) {
            Ok(()) => {
                self.exit_sent.store(true, Ordering::Release);
                false
            }
            Err(SessionEvent::Exited(status)) => {
                *pending = Some(status);
                true
            }
            Err(_) => true,
        };
        drop(pending);
        if retry {
            self.spawn_exit_retry();
        }
    }

    fn send_wakeup(&self) {
        if !self.pending_wakeup.swap(true, Ordering::AcqRel) && !self.try_send_wakeup() {
            self.pending_wakeup.store(false, Ordering::Release);
        }
    }

    fn queue_title(&self, title: String) {
        let mut pending = self
            .pending_title
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *pending = Some(title);
        drop(pending);
        self.send_wakeup();
    }

    fn flush_title(&self) {
        if self.title_queued.load(Ordering::Acquire) {
            return;
        }
        let mut pending = self
            .pending_title
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(title) = pending.take() else {
            return;
        };
        match self.try_send_optional(SessionEvent::Title(title)) {
            Ok(()) => self.title_queued.store(true, Ordering::Release),
            Err(SessionEvent::Title(title)) => {
                *pending = Some(title);
                drop(pending);
                self.send_wakeup();
            }
            Err(_) => {}
        }
    }

    pub(crate) fn ack_title(&self) {
        self.title_queued.store(false, Ordering::Release);
        if !self.pending_wakeup.load(Ordering::Acquire) {
            self.flush_title();
        }
    }

    fn ack_wakeup(&self) {
        self.pending_wakeup.store(false, Ordering::Release);
        self.flush_exit();
        self.flush_title();
    }
}

impl EventListener for SessionListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::Bell | Event::MouseCursorDirty => {
                self.send_wakeup();
            }
            Event::CursorBlinkingChange => {
                let _ = self.try_send_optional(SessionEvent::CursorBlinkingChanged);
            }
            Event::ClipboardStore(kind, text) => {
                let _ = self.try_send_optional(SessionEvent::ClipboardStore(kind, text));
            }
            Event::Title(title) => {
                self.queue_title(title);
            }
            Event::ResetTitle => {
                self.queue_title(String::new());
            }
            Event::PtyWrite(text) => {
                let bytes = text.into_bytes();
                if let Some(io) = self.io.get().and_then(Weak::upgrade) {
                    io.write(Cow::Owned(bytes));
                    return;
                }
                let mut pending = self
                    .pending_writes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if let Some(io) = self.io.get().and_then(Weak::upgrade) {
                        drop(pending);
                        io.write(Cow::Owned(bytes));
                        return;
                    }
                    let pending_len = pending.iter().map(Vec::len).sum::<usize>();
                    if pending_len.saturating_add(bytes.len()) <= MAX_PENDING_PTY_WRITES {
                        pending.push(bytes);
                        return;
                    }
                    pending = self
                        .pending_space
                        .wait(pending)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
            Event::ChildExit(status) => {
                self.queue_exit(status.into());
            }
            _ => {}
        }
    }
}

pub struct TerminalOutput {
    term: Arc<FairMutex<Term<SessionListener>>>,
    listener: SessionListener,
    processor: Processor<StdSyncHandler>,
}

impl TerminalOutput {
    fn new(term: Arc<FairMutex<Term<SessionListener>>>, listener: SessionListener) -> Self {
        Self {
            term,
            listener,
            processor: Processor::new(),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let mut term = self.term.lock();
        self.processor.advance(&mut *term, bytes);
    }

    pub fn child_exited(&self, code: i32) {
        self.listener.child_exited(code);
    }

    pub(crate) fn into_local_parts(
        self,
    ) -> (Arc<FairMutex<Term<SessionListener>>>, SessionListener) {
        (self.term, self.listener)
    }
}

/// A terminal state machine and its transport.
pub struct TerminalSession {
    term: Arc<FairMutex<Term<SessionListener>>>,
    io: Arc<dyn TerminalIo>,
    listener: SessionListener,
    resize_failures: watch::Sender<Option<String>>,
    selection_revision: AtomicU64,
    shutdown_sent: AtomicBool,
}

fn selected_shell(
    explicit: Option<PathBuf>,
    environment: Option<OsString>,
    windows: bool,
) -> anyhow::Result<Option<String>> {
    if windows && explicit.is_none() {
        return Ok(None);
    }
    let environment = environment.and_then(|shell| shell.into_string().ok().map(OsString::from));
    let shell = explicit
        .map(PathBuf::into_os_string)
        .or(environment)
        .unwrap_or_else(|| OsString::from(if windows { "powershell" } else { "/bin/sh" }));
    if shell.is_empty() {
        return Err(anyhow::anyhow!(
            "The terminal program path is empty. Set a path and try again."
        ));
    }
    shell.into_string().map(Some).map_err(|_| {
        anyhow::anyhow!(
            "The terminal program path is not valid Unicode. Use a Unicode path and try again."
        )
    })
}

#[cfg(any(target_os = "windows", test))]
fn quote_windows_program(program: String) -> String {
    format!("\"{program}\"")
}

struct LocalTransport {
    options: Options,
    window_size: WindowSize,
}

#[allow(clippy::too_many_arguments)]
fn local_transport(
    program: Option<PathBuf>,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: HashMap<String, String>,
    escape_args: bool,
    initial_size: TermSize,
    cell_width: u16,
    cell_height: u16,
) -> anyhow::Result<LocalTransport> {
    let args = args
        .into_iter()
        .map(|arg| {
            arg.into_string().map_err(|_| {
                anyhow::anyhow!(
                    "A terminal argument is not valid Unicode. Use Unicode arguments and try again."
                )
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let program = selected_shell(
        program,
        std::env::var_os("SHELL"),
        cfg!(target_os = "windows"),
    )?;
    #[cfg(not(target_os = "windows"))]
    let _ = escape_args;
    let options = Options {
        shell: program.map(|program| {
            #[cfg(target_os = "windows")]
            let program = quote_windows_program(program);
            Shell::new(program, args)
        }),
        working_directory: cwd,
        env,
        ..Default::default()
    };
    #[cfg(target_os = "windows")]
    let options = Options {
        escape_args,
        ..options
    };
    Ok(LocalTransport {
        options,
        window_size: initial_size.window_size(cell_width, cell_height)?,
    })
}

fn legacy_event_receiver(
    events: mpsc::Receiver<SessionEvent>,
    session: Weak<TerminalSession>,
) -> anyhow::Result<mpsc::UnboundedReceiver<SessionEvent>> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("terminal-event-bridge".to_owned())
        .spawn(move || {
            let mut events = events;
            while let Some(event) = events.blocking_recv() {
                if let Some(session) = session.upgrade() {
                    match &event {
                        SessionEvent::Wakeup => session.ack_wakeup(),
                        SessionEvent::Title(_) => session.ack_title(),
                        _ => {}
                    }
                }
                if events_tx.send(event).is_err() {
                    break;
                }
            }
        })
        .context("The terminal event bridge failed to start.")?;
    Ok(events_rx)
}

impl TerminalSession {
    /// Starts a local PTY shell and returns its session and event receiver.
    pub fn local<S: AsRef<OsStr>>(
        shell: Option<S>,
        cwd: Option<PathBuf>,
        initial_size: TermSize,
        cell_width: u16,
        cell_height: u16,
        scrollback: usize,
    ) -> anyhow::Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionEvent>)> {
        Self::local_command(
            shell.map(|shell| PathBuf::from(shell.as_ref())),
            Vec::new(),
            cwd,
            HashMap::new(),
            true,
            initial_size,
            cell_width,
            cell_height,
            scrollback,
        )
    }

    pub fn local_bounded<S: AsRef<OsStr>>(
        shell: Option<S>,
        cwd: Option<PathBuf>,
        initial_size: TermSize,
        cell_width: u16,
        cell_height: u16,
        scrollback: usize,
    ) -> anyhow::Result<(Arc<Self>, mpsc::Receiver<SessionEvent>)> {
        Self::local_command_bounded(
            shell.map(|shell| PathBuf::from(shell.as_ref())),
            Vec::new(),
            cwd,
            HashMap::new(),
            true,
            initial_size,
            cell_width,
            cell_height,
            scrollback,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn local_command(
        program: Option<PathBuf>,
        args: Vec<OsString>,
        cwd: Option<PathBuf>,
        env: HashMap<String, String>,
        escape_args: bool,
        initial_size: TermSize,
        cell_width: u16,
        cell_height: u16,
        scrollback: usize,
    ) -> anyhow::Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionEvent>)> {
        let transport = local_transport(
            program,
            args,
            cwd,
            env,
            escape_args,
            initial_size,
            cell_width,
            cell_height,
        )?;
        Self::from_transport_facade(initial_size, scrollback, move |output| {
            let (term, listener) = output.into_local_parts();
            LocalPty::spawn(
                term,
                listener,
                transport.options,
                transport.window_size,
                false,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn local_command_bounded(
        program: Option<PathBuf>,
        args: Vec<OsString>,
        cwd: Option<PathBuf>,
        env: HashMap<String, String>,
        escape_args: bool,
        initial_size: TermSize,
        cell_width: u16,
        cell_height: u16,
        scrollback: usize,
    ) -> anyhow::Result<(Arc<Self>, mpsc::Receiver<SessionEvent>)> {
        let transport = local_transport(
            program,
            args,
            cwd,
            env,
            escape_args,
            initial_size,
            cell_width,
            cell_height,
        )?;
        Self::from_transport_facade_bounded(initial_size, scrollback, move |output| {
            let (term, listener) = output.into_local_parts();
            LocalPty::spawn(
                term,
                listener,
                transport.options,
                transport.window_size,
                false,
            )
        })
    }

    pub fn from_transport(
        initial_size: TermSize,
        scrollback: usize,
        start: impl FnOnce(
            Arc<FairMutex<Term<SessionListener>>>,
            SessionListener,
        ) -> anyhow::Result<Arc<dyn TerminalIo>>,
    ) -> anyhow::Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionEvent>)> {
        let (session, events) = Self::from_transport_bounded(initial_size, scrollback, start)?;
        let events = legacy_event_receiver(events, Arc::downgrade(&session))?;
        Ok((session, events))
    }

    pub fn from_transport_bounded(
        initial_size: TermSize,
        scrollback: usize,
        start: impl FnOnce(
            Arc<FairMutex<Term<SessionListener>>>,
            SessionListener,
        ) -> anyhow::Result<Arc<dyn TerminalIo>>,
    ) -> anyhow::Result<(Arc<Self>, mpsc::Receiver<SessionEvent>)> {
        Self::from_transport_with(initial_size, scrollback, start)
    }

    pub fn from_transport_facade(
        initial_size: TermSize,
        scrollback: usize,
        start: impl FnOnce(TerminalOutput) -> anyhow::Result<Arc<dyn TerminalIo>>,
    ) -> anyhow::Result<(Arc<Self>, mpsc::UnboundedReceiver<SessionEvent>)> {
        let (session, events) =
            Self::from_transport_facade_bounded(initial_size, scrollback, start)?;
        let events = legacy_event_receiver(events, Arc::downgrade(&session))?;
        Ok((session, events))
    }

    pub fn from_transport_facade_bounded(
        initial_size: TermSize,
        scrollback: usize,
        start: impl FnOnce(TerminalOutput) -> anyhow::Result<Arc<dyn TerminalIo>>,
    ) -> anyhow::Result<(Arc<Self>, mpsc::Receiver<SessionEvent>)> {
        Self::from_transport_with(initial_size, scrollback, move |term, listener| {
            start(TerminalOutput::new(term, listener))
        })
    }

    fn from_transport_with(
        initial_size: TermSize,
        scrollback: usize,
        start: impl FnOnce(
            Arc<FairMutex<Term<SessionListener>>>,
            SessionListener,
        ) -> anyhow::Result<Arc<dyn TerminalIo>>,
    ) -> anyhow::Result<(Arc<Self>, mpsc::Receiver<SessionEvent>)> {
        let initial_size = initial_size.checked()?;
        let (events_tx, events_rx) = mpsc::channel(SESSION_EVENT_CAPACITY);
        let listener = SessionListener::new(events_tx);

        let config = Config {
            scrolling_history: scrollback,
            osc52: Osc52::OnlyCopy,
            kitty_keyboard: true,
            ..Default::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &initial_size,
            listener.clone(),
        )));

        let io = start(term.clone(), listener.clone()).context(
            "The terminal session failed to start. Check the command and connection, then try again.",
        )?;
        listener.set_io(&io);
        let (resize_failures, _) = watch::channel(None);

        Ok((
            Arc::new(Self {
                term,
                io,
                listener,
                resize_failures,
                selection_revision: AtomicU64::new(0),
                shutdown_sent: AtomicBool::new(false),
            }),
            events_rx,
        ))
    }

    pub fn term(&self) -> &Arc<FairMutex<Term<SessionListener>>> {
        &self.term
    }

    pub(crate) fn subscribe_resize_failures(&self) -> watch::Receiver<Option<String>> {
        self.resize_failures.subscribe()
    }

    pub fn size(&self) -> TermSize {
        let term = self.term.lock();
        TermSize::new(term.columns(), term.screen_lines())
    }

    pub fn visible_text(&self) -> String {
        let term = self.term.lock();
        let mut text = String::new();
        let mut line = String::new();
        let mut current_line = None;
        for indexed in term.grid().display_iter() {
            let point = indexed.point;
            if current_line != Some(point.line.0) {
                if current_line.is_some() {
                    text.push_str(&accessible_line(&line));
                    text.push('\n');
                }
                current_line = Some(point.line.0);
                line.clear();
            }
            let cell = &indexed.cell;
            if cell.flags.contains(Flags::HIDDEN)
                || cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if is_accessible_char(cell.c) {
                line.push(cell.c);
            }
            for ch in cell.zerowidth().into_iter().flatten() {
                if is_accessible_char(*ch) {
                    line.push(*ch);
                }
            }
        }
        if current_line.is_some() {
            text.push_str(&accessible_line(&line));
        }
        text
    }

    pub fn cursor_viewport_position(&self) -> Option<(usize, usize)> {
        let term = self.term.lock();
        let row = viewport_cursor_row(
            term.grid().cursor.point.line.0,
            term.grid().display_offset(),
            term.screen_lines(),
        )?;
        Some((row, term.grid().cursor.point.column.0))
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.io.write(bytes.into());
    }

    /// Resizes the terminal grid and PTY.
    pub fn resize(&self, size: TermSize) {
        let result = self.try_resize_inner(size);
        self.resize_failures
            .send_if_modified(|failure| match &result {
                Ok(true) if failure.is_some() => {
                    *failure = None;
                    true
                }
                Ok(true) | Ok(false) => false,
                Err(_) if failure.is_some() => false,
                Err(_) => {
                    *failure = Some(RESIZE_FAILURE_MESSAGE.to_owned());
                    true
                }
            });
    }

    pub fn try_resize(&self, size: TermSize) -> anyhow::Result<()> {
        self.try_resize_inner(size).map(|_| ())
    }

    fn try_resize_inner(&self, size: TermSize) -> anyhow::Result<bool> {
        let size = size.checked()?;
        let mut term = self.term.lock();
        if term.columns() == size.columns && term.screen_lines() == size.screen_lines {
            return Ok(false);
        }
        if self.shutdown_sent.load(Ordering::Acquire) {
            // The transport is gone, so the grid keeps its last size and the user does not
            // see a resize error for a session that already ended.
            return Ok(false);
        }
        self.io.try_resize(size)?;
        term.resize(size);
        Ok(true)
    }

    pub fn scroll(&self, lines: i32) {
        let mut term = self.term.lock();
        let had_selection = term.selection.is_some();
        if had_selection {
            term.selection = None;
        }
        term.scroll_display(Scroll::Delta(lines));
        drop(term);
        if had_selection {
            self.bump_selection_revision();
        }
    }

    pub(crate) fn scroll_preserving_selection(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&self) {
        let mut term = self.term.lock();
        let had_selection = term.selection.is_some();
        if had_selection {
            term.selection = None;
        }
        term.scroll_display(Scroll::Bottom);
        drop(term);
        if had_selection {
            self.bump_selection_revision();
        }
    }

    pub fn scroll_to_top(&self) {
        let mut term = self.term.lock();
        let had_selection = term.selection.is_some();
        if had_selection {
            term.selection = None;
        }
        term.scroll_display(Scroll::Top);
        drop(term);
        if had_selection {
            self.bump_selection_revision();
        }
    }

    /// Scrolls a grid point into view for search navigation.
    pub fn scroll_to_point(&self, point: Point) {
        let mut term = self.term.lock();
        let had_selection = term.selection.is_some();
        if had_selection {
            term.selection = None;
        }
        term.scroll_to_point(point);
        drop(term);
        if had_selection {
            self.bump_selection_revision();
        }
    }

    pub fn ack_wakeup(&self) {
        self.listener.ack_wakeup();
    }

    pub fn ack_title(&self) {
        self.listener.ack_title();
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// Returns the cursor blink request from the terminal application.
    pub fn cursor_blinking(&self) -> bool {
        self.term.lock().cursor_style().blinking
    }

    pub fn display_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn history_size(&self) -> usize {
        let term = self.term.lock();
        term.grid().history_size()
    }

    fn bump_selection_revision(&self) {
        self.selection_revision.fetch_add(1, Ordering::AcqRel);
    }

    /// Captures the current selection as a copy plan, or `None` when nothing is selected.
    pub(crate) fn selection_copy(&self) -> Option<crate::copy::SelectionCopy> {
        crate::copy::plan_selection(&*self.term.lock())
    }

    /// Copies the selection, taking the grid lock once per group of lines.
    fn copy_selection_text(&self, copy: crate::copy::SelectionCopy) -> String {
        let mut text = String::new();
        for chunk in 0..copy.chunks() {
            let part = crate::copy::copy_chunk(&*self.term.lock(), copy, chunk);
            text.push_str(&part);
        }
        text
    }

    /// Copies the selection without holding the grid lock across the whole scrollback, so the
    /// terminal keeps painting and accepting output while a large selection is copied.
    pub async fn selection_text_in_chunks(&self) -> (u64, Option<String>) {
        let revision = self.selection_revision.load(Ordering::Acquire);
        let Some(copy) = self.selection_copy() else {
            return (revision, None);
        };
        let mut text = String::new();
        for chunk in 0..copy.chunks() {
            if chunk > 0 {
                tokio::task::yield_now().await;
            }
            let part = crate::copy::copy_chunk(&*self.term.lock(), copy, chunk);
            text.push_str(&part);
        }
        (revision, Some(text))
    }

    pub(crate) fn selection_is_current(&self, revision: u64) -> bool {
        self.selection_revision.load(Ordering::Acquire) == revision
    }

    /// Returns selected text, including wide characters and without trailing spaces.
    pub fn selection_to_string(&self) -> Option<String> {
        let copy = self.selection_copy()?;
        Some(self.copy_selection_text(copy))
    }

    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }

    pub fn clear_selection(&self) {
        {
            let mut term = self.term.lock();
            term.selection = None;
        }
        self.bump_selection_revision();
    }

    pub fn select_all(&self) {
        {
            let mut term = self.term.lock();
            let start = Point::new(term.topmost_line(), Column(0));
            let end = Point::new(term.bottommost_line(), term.last_column());
            let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
            selection.update(end, Side::Right);
            term.selection = Some(selection);
        }
        self.bump_selection_revision();
    }

    /// Starts a semantic selection for a double click.
    pub fn select_word(&self, point: Point, side: Side) {
        self.term.lock().selection = Some(Selection::new(SelectionType::Semantic, point, side));
        self.bump_selection_revision();
    }

    /// Starts a line selection for a triple click.
    pub fn select_line(&self, point: Point, side: Side) {
        self.term.lock().selection = Some(Selection::new(SelectionType::Lines, point, side));
        self.bump_selection_revision();
    }

    pub fn start_selection(&self, point: Point, side: Side) {
        self.term.lock().selection = Some(Selection::new(SelectionType::Simple, point, side));
        self.bump_selection_revision();
    }

    /// Starts a block selection for Alt-drag.
    pub fn start_block_selection(&self, point: Point, side: Side) {
        self.term.lock().selection = Some(Selection::new(SelectionType::Block, point, side));
        self.bump_selection_revision();
    }

    pub fn update_selection(&self, point: Point, side: Side) {
        let updated = {
            let mut term = self.term.lock();
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
                true
            } else {
                false
            }
        };
        if updated {
            self.bump_selection_revision();
        }
    }

    /// Pastes text and wraps it for bracketed paste mode.
    pub fn paste(&self, text: &str) {
        let bracketed = self.mode().contains(TermMode::BRACKETED_PASTE);
        let payload = if bracketed {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.write(payload.into_bytes());
    }

    /// Reports focus input when FOCUS_IN_OUT mode is active.
    pub fn focus_in(&self) {
        if self.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write(&b"\x1b[I"[..]);
        }
    }

    pub fn focus_out(&self) {
        if self.mode().contains(TermMode::FOCUS_IN_OUT) {
            self.write(&b"\x1b[O"[..]);
        }
    }

    /// Returns the OSC 8 hyperlink at a grid point.
    fn hyperlink_at(&self, point: Point) -> Option<crate::layout::HyperlinkSpan> {
        crate::layout::hyperlink_at(&*self.term.lock(), point)
    }

    /// Returns a plain-text URL at a grid point.
    fn url_at(&self, point: Point) -> Option<String> {
        self.url_span_at(point).map(|(_, _, url)| url)
    }

    /// Returns a plain-text URL and its grid range.
    pub(crate) fn url_span_at(&self, point: Point) -> Option<(Point, Point, String)> {
        crate::layout::url_span_at(&*self.term.lock(), point)
    }

    /// Returns an OSC 8 URI or plain-text URL at a grid point.
    pub fn link_at(&self, point: Point) -> Option<String> {
        if let Some(span) = self.hyperlink_at(point) {
            return Some(span.uri);
        }
        self.url_at(point)
    }

    pub(crate) fn hyperlink_range(&self, point: Point) -> Option<(Point, Point, String)> {
        self.hyperlink_at(point)
            .map(|span| (span.start, span.end, span.uri))
    }
    fn shutdown(&self) {
        if !self.shutdown_sent.swap(true, Ordering::AcqRel) {
            self.io.shutdown();
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::Line;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[derive(Default)]
    struct TestIo {
        writes: Mutex<Vec<u8>>,
        last_input: Mutex<Option<Cow<'static, [u8]>>>,
        resizes: Mutex<Vec<TermSize>>,
        resize_fails: AtomicBool,
        shutdowns: AtomicUsize,
    }

    impl TerminalIo for TestIo {
        fn write(&self, bytes: Cow<'static, [u8]>) {
            self.writes
                .lock()
                .unwrap()
                .extend_from_slice(bytes.as_ref());
            *self.last_input.lock().unwrap() = Some(bytes);
        }

        fn resize(&self, size: TermSize) {
            self.resizes.lock().unwrap().push(size);
        }

        fn try_resize(&self, size: TermSize) -> anyhow::Result<()> {
            if self.resize_fails.load(Ordering::Relaxed) {
                anyhow::bail!("resize failed");
            }
            self.resize(size);
            Ok(())
        }

        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn test_session() -> Arc<TerminalSession> {
        TerminalSession::from_transport_facade(TermSize::new(10, 3), 32, |mut output| {
            output.process(b"ready");
            Ok(Arc::new(TestIo::default()))
        })
        .expect("test session")
        .0
    }

    #[test]
    fn terminal_session_shutdown_is_idempotent_across_drop() {
        let io = Arc::new(TestIo::default());
        let session =
            TerminalSession::from_transport(TermSize::new(10, 3), 32, |_, _| Ok(io.clone()))
                .expect("test session")
                .0;

        session.shutdown();
        session.shutdown();
        drop(session);

        assert_eq!(io.shutdowns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn listener_flushes_pty_writes_when_io_binds() {
        let (events, _) = mpsc::channel(SESSION_EVENT_CAPACITY);
        let listener = SessionListener::new(events);
        let first = String::from("\x1b[c");
        let first_ptr = first.as_ptr();
        listener.send_event(Event::PtyWrite(first));
        let io = Arc::new(TestIo::default());
        let transport: Arc<dyn TerminalIo> = io.clone();

        listener.set_io(&transport);
        assert_eq!(
            io.last_input.lock().unwrap().as_ref().unwrap().as_ptr(),
            first_ptr
        );
        let text = String::from("\x1b[0c");
        let text_ptr = text.as_ptr();
        listener.send_event(Event::PtyWrite(text));
        assert_eq!(
            io.last_input.lock().unwrap().as_ref().unwrap().as_ptr(),
            text_ptr
        );

        assert_eq!(io.writes.lock().unwrap().as_slice(), b"\x1b[c\x1b[0c");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn chunked_copy_detects_changes_before_the_cut() {
        let session = test_session();
        session.start_selection(Point::new(Line(0), Column(0)), Side::Left);
        let (revision, _) = session.selection_text_in_chunks().await;
        assert!(session.selection_is_current(revision));

        session.update_selection(Point::new(Line(0), Column(1)), Side::Right);

        assert!(!session.selection_is_current(revision));
    }

    #[test]
    fn terminal_input_preserves_the_owned_buffer() {
        let io = Arc::new(TestIo::default());
        let session =
            TerminalSession::from_transport_facade(TermSize::new(10, 3), 32, |_| Ok(io.clone()))
                .expect("test session")
                .0;
        let mut input = Vec::with_capacity(4);
        input.extend_from_slice(b"data");
        let input_ptr = input.as_ptr();

        session.write(input);

        assert_eq!(
            io.last_input.lock().unwrap().as_ref().unwrap().as_ptr(),
            input_ptr
        );
    }

    #[test]
    fn session_event_channel_coalesces_wakeups_and_bounds_clipboard() {
        let (events, mut receiver) = mpsc::channel(SESSION_EVENT_CAPACITY);
        let listener = SessionListener::new(events);

        for _ in 0..(SESSION_EVENT_CAPACITY * 2) {
            listener.send_event(Event::Wakeup);
        }
        assert_eq!(receiver.len(), 1);
        assert!(matches!(receiver.try_recv(), Ok(SessionEvent::Wakeup)));
        listener.ack_wakeup();
        listener.send_event(Event::Wakeup);
        assert!(matches!(receiver.try_recv(), Ok(SessionEvent::Wakeup)));

        for index in 0..(SESSION_EVENT_CAPACITY + 8) {
            listener.send_event(Event::ClipboardStore(
                ClipboardType::Clipboard,
                index.to_string(),
            ));
        }
        assert!(receiver.len() <= SESSION_EVENT_CAPACITY);
    }

    #[test]
    fn title_events_keep_the_latest_value() {
        let (events, mut receiver) = mpsc::channel(SESSION_EVENT_CAPACITY);
        let listener = SessionListener::new(events);

        listener.send_event(Event::Title("first".to_owned()));
        listener.send_event(Event::Title("second".to_owned()));
        assert!(matches!(receiver.try_recv(), Ok(SessionEvent::Wakeup)));
        listener.ack_wakeup();
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionEvent::Title(title)) if title == "second"
        ));
        listener.ack_title();
        assert!(receiver.try_recv().is_err());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn local_pty_reports_shell_completion() {
        let (session, mut events) = TerminalSession::local_bounded(
            Some("/bin/sh"),
            Some(PathBuf::from("/tmp")),
            TermSize::new(80, 24),
            8,
            18,
            1000,
        )
        .expect("local PTY");
        session.write(b"exit\n".to_vec());

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await, Some(SessionEvent::Exited(_))) {
                    break;
                }
            }
        })
        .await
        .expect("PTY completion event");
    }

    #[test]
    fn exit_notification_reserves_a_bounded_slot() {
        let (events, mut receiver) = mpsc::channel(2);
        let listener = SessionListener::new(events);
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "one".to_owned(),
        ));
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "two".to_owned(),
        ));
        listener.transport_closed();

        assert_eq!(receiver.len(), 2);
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionEvent::ClipboardStore(..))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionEvent::Exited(SessionExit {
                code: None,
                signal: None
            }))
        ));
    }

    #[test]
    fn transport_completion_sends_one_exit_event() {
        let (events, mut receiver) = mpsc::channel(SESSION_EVENT_CAPACITY);
        let listener = SessionListener::new(events);

        listener.transport_closed();
        listener.child_exited(7);

        assert!(matches!(
            receiver.try_recv(),
            Ok(SessionEvent::Exited(SessionExit {
                code: None,
                signal: None
            }))
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn term_size_preserves_overflow_until_checked() {
        let max = usize::from(u16::MAX);
        let oversized = TermSize::new(max + 1, 0);
        assert_eq!(oversized.columns, max + 1);
        assert_eq!(oversized.screen_lines, 1);
        assert!(oversized.checked().is_err());
        assert!(TermSize::try_new(max + 1, 1).is_err());
        assert_eq!(TermSize::new(0, 0), TermSize::new(1, 1));
        assert!(
            TermSize {
                columns: 0,
                screen_lines: 1,
            }
            .checked()
            .is_err()
        );

        let io = Arc::new(TestIo::default());
        let session =
            TerminalSession::from_transport_facade(TermSize::new(10, 3), 32, |_| Ok(io.clone()))
                .expect("test session")
                .0;
        assert!(
            session
                .try_resize(TermSize {
                    columns: max + 1,
                    screen_lines: 3,
                })
                .is_err()
        );
        assert_eq!(session.size(), TermSize::new(10, 3));
        assert!(io.resizes.lock().unwrap().is_empty());
    }

    #[test]
    fn resize_failure_keeps_grid_and_publishes_latest_feedback() {
        let io = Arc::new(TestIo::default());
        io.resize_fails.store(true, Ordering::Relaxed);
        let session =
            TerminalSession::from_transport_facade(TermSize::new(10, 3), 32, |_| Ok(io.clone()))
                .expect("test session")
                .0;
        let mut failures = session.subscribe_resize_failures();

        session.resize(TermSize::new(12, 4));

        assert_eq!(session.size(), TermSize::new(10, 3));
        assert!(io.resizes.lock().unwrap().is_empty());
        assert_eq!(
            failures.borrow_and_update().as_deref(),
            Some(RESIZE_FAILURE_MESSAGE)
        );

        io.resize_fails.store(false, Ordering::Relaxed);
        session.resize(TermSize::new(12, 4));

        assert_eq!(session.size(), TermSize::new(12, 4));
        assert_eq!(
            io.resizes.lock().unwrap().as_slice(),
            &[TermSize::new(12, 4)]
        );
        assert!(failures.borrow_and_update().is_none());
    }

    #[test]
    fn resize_after_shutdown_keeps_the_grid_and_reports_no_failure() {
        let io = Arc::new(TestIo::default());
        let session =
            TerminalSession::from_transport_facade(TermSize::new(10, 3), 32, |_| Ok(io.clone()))
                .expect("test session")
                .0;
        let mut failures = session.subscribe_resize_failures();
        session.shutdown();

        session.resize(TermSize::new(20, 6));

        assert_eq!(session.size(), TermSize::new(10, 3));
        assert!(io.resizes.lock().unwrap().is_empty());
        assert!(failures.borrow_and_update().is_none());
        assert!(session.try_resize(TermSize::new(20, 6)).is_ok());
    }

    #[test]
    fn viewport_cursor_row_has_no_off_by_one() {
        assert_eq!(viewport_cursor_row(0, 0, 3), Some(0));
        assert_eq!(viewport_cursor_row(-2, 2, 3), Some(0));
        assert_eq!(viewport_cursor_row(2, 0, 3), Some(2));
        assert_eq!(viewport_cursor_row(3, 0, 3), None);
        assert_eq!(viewport_cursor_row(-1, 0, 3), None);
    }

    #[test]
    fn session_cursor_position_uses_viewport_offset() {
        let session = test_session();
        assert_eq!(session.cursor_viewport_position(), Some((0, 5)));
    }

    #[test]
    fn session_cursor_position_matches_rendered_viewport() {
        let session =
            TerminalSession::from_transport_facade(TermSize::new(10, 3), 32, |mut output| {
                output.process(b"one\r\ntwo");
                Ok(Arc::new(TestIo::default()))
            })
            .expect("test session")
            .0;
        session.scroll(1);
        let (row, column) = {
            let term = session.term().lock();
            let first = term.grid().display_iter().next().expect("visible cell");
            let cursor = term.grid().cursor.point;
            (
                (cursor.line.0 - first.point.line.0) as usize,
                cursor.column.0,
            )
        };
        assert_eq!(session.cursor_viewport_position(), Some((row, column)));
    }

    #[test]
    fn accessible_text_preserves_line_spacing_and_removes_padding() {
        assert_eq!(accessible_text("  a  b  \n\tc\u{7f}  "), "  a  b\nc");
    }

    #[test]
    fn visible_text_preserves_line_spacing_and_filters_padding() {
        let session = test_session();
        {
            let mut term = session.term().lock();
            term.grid_mut()[Point::new(Line(0), Column(0))].c = 'a';
            term.grid_mut()[Point::new(Line(0), Column(1))].c = ' ';
            term.grid_mut()[Point::new(Line(0), Column(2))].c = '\n';
            term.grid_mut()[Point::new(Line(0), Column(3))].c = '\u{7f}';
            term.grid_mut()[Point::new(Line(0), Column(4))].c = '界';
            term.grid_mut()[Point::new(Line(0), Column(5))].c = 'h';
            term.grid_mut()[Point::new(Line(0), Column(5))]
                .flags
                .insert(Flags::HIDDEN);
        }
        assert_eq!(session.visible_text(), "a 界\n\n");
    }

    #[test]
    fn visible_text_keeps_line_breaks_and_wide_base_once() {
        let session =
            TerminalSession::from_transport_facade(TermSize::new(10, 2), 32, |mut output| {
                output.process("a  b\r\n界".as_bytes());
                Ok(Arc::new(TestIo::default()))
            })
            .expect("test session")
            .0;
        assert_eq!(session.visible_text(), "a  b\n界");
    }

    #[test]
    fn scrolling_clears_selection_once_before_viewport_moves() {
        let session = test_session();
        session.start_selection(Point::new(Line(0), Column(0)), Side::Left);
        assert!(session.has_selection());
        session.scroll(1);
        assert!(!session.has_selection());
        session.scroll(1);
        assert!(!session.has_selection());
        session.start_selection(Point::new(Line(0), Column(0)), Side::Left);
        session.scroll_to_top();
        assert!(!session.has_selection());
        session.start_selection(Point::new(Line(0), Column(0)), Side::Left);
        session.scroll_to_bottom();
        assert!(!session.has_selection());
    }

    #[test]
    fn selection_drag_auto_scroll_preserves_selection() {
        let session = test_session();
        session.start_selection(Point::new(Line(0), Column(0)), Side::Left);
        session.scroll_preserving_selection(1);
        assert!(session.has_selection());
    }

    #[test]
    fn scroll_to_point_clears_selection() {
        let session = test_session();
        session.start_selection(Point::new(Line(0), Column(0)), Side::Left);
        session.scroll_to_point(Point::new(Line(0), Column(0)));
        assert!(!session.has_selection());
    }

    #[test]
    fn terminal_output_can_cross_a_tokio_task_boundary() {
        fn assert_send<T: Send>() {}
        assert_send::<TerminalOutput>();
    }

    #[test]
    fn windows_program_path_is_quoted_for_create_process() {
        assert_eq!(
            quote_windows_program(r"C:\Program Files\PowerShell\powershell.exe".to_owned()),
            r#""C:\Program Files\PowerShell\powershell.exe""#
        );
    }

    #[test]
    fn local_shell_selection_uses_explicit_environment_and_fallback() {
        assert_eq!(
            selected_shell(
                Some(PathBuf::from("/bin/zsh")),
                Some(OsString::from("/bin/fish")),
                false,
            )
            .unwrap(),
            Some("/bin/zsh".to_owned())
        );
        assert_eq!(
            selected_shell(None, Some(OsString::from("/bin/fish")), false).unwrap(),
            Some("/bin/fish".to_owned())
        );
        assert_eq!(
            selected_shell(None, None, false).unwrap(),
            Some("/bin/sh".to_owned())
        );
        assert_eq!(
            selected_shell(None, Some(OsString::from("bad")), true).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_shell_rejects_non_unicode_program_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt as _;

        let program = OsString::from_vec(vec![b'/', b'b', 0xff]);
        assert!(selected_shell(Some(PathBuf::from(program.clone())), None, false).is_err());
        assert_eq!(
            selected_shell(None, Some(program), false).unwrap(),
            Some("/bin/sh".to_owned())
        );
    }
}
