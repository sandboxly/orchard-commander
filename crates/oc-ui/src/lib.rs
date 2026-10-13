//! The Orchard Commander UI: state, widgets, the main window and its
//! features, drawn into `oc_grid` frames. No window system code lives here
//! except the winit key conversion; the app binary owns the event loop.
//!
//! See `docs/ARCHITECTURE.md` for how the pieces fit and how a feature
//! module plugs in. In short:
//!
//! * [`App`] owns all UI state. The platform layer calls
//!   [`App::handle_event`], [`App::process_tasks`], [`App::tick`] and
//!   [`App::draw`], and acts on [`App::take_requests`].
//! * Drawing is a pure function of the state into a [`oc_grid::Frame`];
//!   redraws happen only when [`App::take_redraw`] says something changed.
//! * Blocking work runs through [`Tasks`] ([`tasks`]); results come back on
//!   the UI thread as closures over `&mut App`.
//! * Features ([`features`]) register commands ([`commands`]), side views
//!   and hooks from one function each.

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod connections;
pub mod dialogs;
pub mod features;
pub mod geom;
pub mod images;
pub mod input;
pub mod main_window;
pub mod menus;
pub mod modal;
pub mod panel;
pub mod shortcut;
pub mod tasks;
pub mod testing;
pub mod theme;
pub mod widgets;

pub use app::{App, AppConfig, MAIN_WINDOW, Outcome, PlatformRequest, Severity, UiWindow, WinId, WindowMenu, WindowMenuItem};
pub use commands::{Command, CommandId, MenuRole, Registry};
pub use geom::Rect;
pub use input::{Event, Key, KeyEvent, Mods, MouseButton, MouseEvent, MouseKind, Pointer};
pub use menus::Menu;
pub use modal::{Modal, ModalId, ModalOutcome};
pub use shortcut::Shortcut;
pub use tasks::{CancelToken, Reply, Tasks};
pub use theme::{Colors, Theme};
pub use widgets::DrawCtx;
