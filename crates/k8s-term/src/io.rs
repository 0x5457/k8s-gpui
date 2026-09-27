//! Defines the terminal byte transport and local PTY implementation.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::tty::{self, Options};
use anyhow::Context as _;

use crate::session::{SessionListener, TermSize};

/// Transport for terminal input, resize, and shutdown requests.
pub trait TerminalIo: Send + Sync + 'static {
    /// Sends keyboard, paste, and mouse input to the child process.
    fn write(&self, bytes: Cow<'static, [u8]>);
    /// Sends a terminal grid resize to the PTY or remote session.
    fn resize(&self, size: TermSize);
    fn try_resize(&self, size: TermSize) -> anyhow::Result<()> {
        self.resize(size);
        Ok(())
    }
    /// Requests session shutdown. Calls can repeat.
    fn shutdown(&self);
}

const LOCAL_COMMAND_CAPACITY: usize = 64;
const LOCAL_COMMAND_MAX_BYTES: usize = 64 * 1024;

struct LocalCommandState {
    input: VecDeque<Cow<'static, [u8]>>,
    bytes: usize,
    resize: Option<WindowSize>,
    shutdown: bool,
    shutdown_sent: bool,
}

struct LocalCommandQueue {
    state: Mutex<LocalCommandState>,
    available: Condvar,
    capacity: usize,
    max_bytes: usize,
}

impl LocalCommandQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(LocalCommandState {
                input: VecDeque::with_capacity(LOCAL_COMMAND_CAPACITY),
                bytes: 0,
                resize: None,
                shutdown: false,
                shutdown_sent: false,
            }),
            available: Condvar::new(),
            capacity: LOCAL_COMMAND_CAPACITY,
            max_bytes: LOCAL_COMMAND_MAX_BYTES,
        })
    }

    fn push_input(&self, bytes: Cow<'static, [u8]>) {
        if bytes.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.shutdown {
            return;
        }
        // The queue is bounded and the caller is the UI thread, so a full queue evicts the
        // oldest pending input instead of waiting. Blocking here would freeze the window
        // whenever the child process stops reading.
        while !state.input.is_empty()
            && (state.input.len() >= self.capacity
                || state.bytes.saturating_add(bytes.len()) > self.max_bytes)
        {
            if let Some(evicted) = state.input.pop_front() {
                state.bytes = state.bytes.saturating_sub(evicted.len());
            }
        }
        state.bytes = state.bytes.saturating_add(bytes.len());
        state.input.push_back(bytes);
        drop(state);
        self.available.notify_one();
    }

    fn request_resize(&self, size: WindowSize) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.shutdown {
            return false;
        }
        state.resize = Some(size);
        drop(state);
        self.available.notify_one();
        true
    }

    fn pop(&self) -> Option<Msg> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(size) = state.resize.take() {
                drop(state);
                return Some(Msg::Resize(size));
            }
            if let Some(bytes) = state.input.pop_front() {
                state.bytes = state.bytes.saturating_sub(bytes.len());
                drop(state);
                self.available.notify_one();
                return Some(Msg::Input(bytes));
            }
            if state.shutdown {
                if state.shutdown_sent {
                    return None;
                }
                state.shutdown_sent = true;
                return Some(Msg::Shutdown);
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.shutdown = true;
        state.resize = None;
        drop(state);
        self.available.notify_all();
    }
}

/// Local PTY backed by the Alacritty event loop.
pub struct LocalPty {
    commands: Arc<LocalCommandQueue>,
    cell_width: u16,
    cell_height: u16,
    shutdown_sent: AtomicBool,
}

impl LocalPty {
    pub fn spawn(
        term: Arc<FairMutex<Term<SessionListener>>>,
        listener: SessionListener,
        options: Options,
        window_size: WindowSize,
        drain_on_exit: bool,
    ) -> anyhow::Result<Arc<dyn TerminalIo>> {
        let cell_width = window_size.cell_width;
        let cell_height = window_size.cell_height;
        let pty = tty::new(&options, window_size, 0).context(
            "The local terminal failed to start. Check the shell and permissions, then try again.",
        )?;
        let event_loop = EventLoop::new(term, listener.clone(), pty, drain_on_exit, false)
            .context("The local terminal event loop failed to start. Try again.")?;
        let commands = LocalCommandQueue::new();
        let relay_commands = commands.clone();
        let completion_commands = commands.clone();
        let relay_sender = event_loop.channel();
        let completion_listener = listener;
        let event_loop_thread = event_loop.spawn();
        std::thread::Builder::new()
            .name("PTY command relay".to_owned())
            .spawn(move || {
                while let Some(message) = relay_commands.pop() {
                    if relay_sender.send(message).is_err() {
                        relay_commands.shutdown();
                        break;
                    }
                }
            })
            .context("The local terminal command relay failed to start.")?;
        std::thread::Builder::new()
            .name("PTY completion".to_owned())
            .spawn(move || {
                let _ = event_loop_thread.join();
                completion_commands.shutdown();
                completion_listener.transport_closed();
            })
            .context("The local terminal completion watcher failed to start.")?;
        Ok(Arc::new(Self {
            commands,
            cell_width,
            cell_height,
            shutdown_sent: AtomicBool::new(false),
        }))
    }
}

impl TerminalIo for LocalPty {
    fn write(&self, bytes: Cow<'static, [u8]>) {
        self.commands.push_input(bytes);
    }

    fn resize(&self, size: TermSize) {
        let _ = self.try_resize(size);
    }

    fn try_resize(&self, size: TermSize) -> anyhow::Result<()> {
        let window_size = size.window_size(self.cell_width, self.cell_height)?;
        if !self.commands.request_resize(window_size) {
            anyhow::bail!("The local terminal is shutting down.");
        }
        Ok(())
    }

    fn shutdown(&self) {
        if !self.shutdown_sent.swap(true, Ordering::AcqRel) {
            self.commands.shutdown();
        }
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        <Self as TerminalIo>::shutdown(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(queue: &LocalCommandQueue) -> Vec<Vec<u8>> {
        queue.shutdown();
        let mut messages = Vec::new();
        while let Some(message) = queue.pop() {
            match message {
                Msg::Input(bytes) => messages.push(bytes.into_owned()),
                Msg::Shutdown | Msg::Resize(_) => break,
            }
        }
        messages
    }

    #[test]
    fn local_input_queue_keeps_owned_buffer() {
        let queue = LocalCommandQueue::new();
        let mut bytes = Vec::with_capacity(4);
        bytes.extend_from_slice(b"data");
        let bytes_ptr = bytes.as_ptr();
        queue.push_input(Cow::Owned(bytes));

        let message = queue.pop().expect("queued input");
        let Msg::Input(input) = message else {
            panic!("unexpected PTY message");
        };
        assert_eq!(input.as_ptr(), bytes_ptr);
    }

    #[test]
    fn local_input_queue_bounds_memory_without_blocking_the_caller() {
        let queue = LocalCommandQueue::new();
        let capacity = LOCAL_COMMAND_CAPACITY;
        for index in 0..(capacity * 2) {
            queue.push_input(Cow::Owned(index.to_string().into_bytes()));
        }
        let messages = drain(&queue);

        assert_eq!(messages.len(), capacity);
        assert_eq!(
            *messages.last().expect("newest input"),
            (capacity * 2 - 1).to_string().into_bytes(),
            "the newest input is never dropped"
        );
        let queued = messages.iter().map(|bytes| bytes.len()).sum::<usize>();
        assert!(queued <= LOCAL_COMMAND_MAX_BYTES);
    }

    #[test]
    fn local_input_queue_keeps_only_the_latest_resize() {
        let queue = LocalCommandQueue::new();
        assert!(queue.request_resize(WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 8,
            cell_height: 18,
        }));
        assert!(queue.request_resize(WindowSize {
            num_lines: 40,
            num_cols: 120,
            cell_width: 8,
            cell_height: 18,
        }));

        let Msg::Resize(size) = queue.pop().expect("latest resize") else {
            panic!("the newest resize must be delivered first");
        };
        assert_eq!(size.num_cols, 120);
        assert_eq!(size.num_lines, 40);
    }

    #[test]
    fn local_input_queue_refuses_work_after_shutdown() {
        let queue = LocalCommandQueue::new();
        queue.shutdown();

        queue.push_input(Cow::Owned(b"late".to_vec()));
        assert!(!queue.request_resize(WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 8,
            cell_height: 18,
        }));
        assert!(matches!(queue.pop(), Some(Msg::Shutdown)));
        assert!(queue.pop().is_none());
    }
}
