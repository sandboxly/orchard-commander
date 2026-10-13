//! The terminal panel's engine: the user's shell in a pseudo-terminal, its
//! screen, and how keys and mouse events become bytes for it — no UI.
//!
//! Port of `terminal.py` plus the toolkit-independent parts of
//! `ui/terminal_view.py`. The screen is alacritty_terminal's `Term` (which,
//! unlike pyte, reflows wrapped lines on resize); [`Screen`] adds what the
//! file manager needs on top (OSC 7 folder reports, every xterm mouse mode,
//! legacy alternate screens) and hands the renderer a [`Snapshot`] with
//! colors resolved to RGB. [`Session`] runs the shell (portable-pty: openpty,
//! ConPTY on Windows) and feeds its output into the screen on a reader thread.

pub mod input;
pub mod keys;
pub mod mouse;
mod scan;
pub mod screen;
pub mod session;
pub mod shell;

pub use input::{MouseAction, MouseInput, PointerEvent};
pub use keys::{Key, KeyOptions, Modifiers, encode_focus, encode_key, encode_paste};
pub use mouse::{Button, MouseModes, MouseReport, MouseTracking, encode_mouse, encode_mouse_into};
pub use screen::{
    Cell, CellFlags, Cursor, CursorShape, DARK_ANSI, LIGHT_ANSI, Palette, Rgb, SCROLLBACK, Screen,
    SelectionKind, Snapshot,
};
pub use session::{Notify, Session, SessionEvent, StartError, ended_message, start_failed_message};
pub use shell::{
    POWERSHELL_INTEGRATION, cd_command, default_shell, is_powershell, path_from_osc7, process_cwd,
    shell_command, shell_environment, shell_quote, with_integration,
};
