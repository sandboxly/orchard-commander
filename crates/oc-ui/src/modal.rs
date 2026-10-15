//! Modal dialogs: a stack per window. The top one gets all input of its
//! window; Esc/Enter handling is the dialog's own (see [`crate::dialogs`]).
//!
//! A dialog delivers its result by calling a callback it was built with
//! (it gets `&mut App` in `handle_event`), so it can start work, push
//! another dialog or answer a worker waiting in `Tasks::ask_ui`.

use orchard_grid::Frame;

use crate::app::{App, WinId};
use crate::geom::Rect;
use crate::input::Event;
use crate::widgets::DrawCtx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModalOutcome {
    /// Not for this dialog (it still blocks the window below).
    Ignored,
    Consumed,
    /// Remove this dialog.
    Close,
}

pub trait Modal {
    /// Shown in the frame; also how tests find a dialog.
    fn title(&self) -> &str;

    /// Draw over the window (`screen` is the whole grid).
    fn draw(&self, app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx);

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome;

    /// A text field has the focus: the platform enables the input method.
    fn wants_text_input(&self) -> bool {
        false
    }

    /// For tests and diagnostics: the dialog's text content.
    fn text(&self) -> String {
        String::new()
    }
}

/// Identifies a pushed dialog (to close it from outside, e.g. a progress
/// dialog when its operation ends).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModalId(pub u64);

pub(crate) struct ModalEntry {
    pub id: ModalId,
    pub window: WinId,
    /// Taken out while it handles an event.
    pub modal: Option<Box<dyn Modal>>,
}
