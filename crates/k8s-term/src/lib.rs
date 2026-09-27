#![forbid(unsafe_code)]

//! Embedded terminal state, input, search, and GPUI rendering.

pub mod blink;
pub mod contrast;
pub mod copy;
mod element;
pub mod ime;
pub mod io;
pub mod keys;
mod layout;
pub mod mouse;
pub mod palette;
pub mod scrollbar;
pub mod search;
pub mod selection;
pub mod session;
pub mod view;

pub use alacritty_terminal::index::{Column, Line, Point, Side};
pub use blink::{BLINK_INTERVAL, BlinkState};
pub use ime::ImeState;
pub use io::{LocalPty, TerminalIo};
pub use palette::{Palette, TerminalTheme};
pub use search::{SearchMatch, SearchState};
pub use session::{SessionEvent, TermSize, TerminalOutput, TerminalSession};
pub use view::{
    HoveredLink, MouseInputMode, TERMINAL_CELL_PADDING, TerminalView, local_terminal_view,
};
