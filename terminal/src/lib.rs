//! A GPU-accelerated terminal emulator component for [GPUI].
//!
//! The emulation layer is `alacritty_terminal`; this crate wires it up to GPUI
//! rendering, input, selection, search, hyperlinks and vi mode.
//!
//! [GPUI]: https://github.com/zed-industries/zed

mod agent_status;
pub mod clipboard;
pub mod colors;
pub mod diagnostics;
pub mod event;
pub mod hyperlink;
pub mod input;
pub mod mouse;
pub mod pty_session;
mod pty_writer;
pub mod render;
pub mod search;
pub mod shell_integration;
pub mod terminal;
mod theme_protocol;
pub mod view;

pub use agent_status::{AgentReportState, AgentStatusReport};
pub use alacritty_terminal;
pub use clipboard::Clipboard;
pub use colors::{ColorPalette, ColorPaletteBuilder, hsla_to_rgb};
pub use event::{
    ClipboardFormatter, GpuiEventProxy, PtyWriter, TerminalEvent, TerminalNotifier,
    TerminalNotifierHandle,
};
pub use hyperlink::{HyperlinkKind, HyperlinkMatch, RegexSearches};
pub use pty_writer::WriteCompletion;
pub use render::{PaintContext, TerminalRenderer};
pub use search::SearchState;
pub use shell_integration::{
    BlockLine, BlockRun, BlockSnapshot, MAX_BLOCK_BYTES, ShellEvent, render_output_lines,
};
pub use terminal::{TermDimensions, TerminalState};
pub use view::{
    BellCallback, ClipboardStoreCallback, ExitCallback, KeyHandler, LinkCallback, ResizeCallback,
    TerminalConfig, TerminalView, TitleCallback,
};
