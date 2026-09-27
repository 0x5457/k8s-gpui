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

pub use io::TerminalIo;
pub use palette::{Palette, TerminalTheme};
pub use session::{SessionEvent, TermSize, TerminalOutput, TerminalSession};
pub use view::{TERMINAL_CELL_PADDING, TerminalView, local_terminal_view};
