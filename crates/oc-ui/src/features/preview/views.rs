//! The Preview pane's content views: one per kind of [`PreviewData`].
//!
//! A preview loads in two steps:
//!
//! 1. **worker**: `oc_preview::Registry::preview` → [`PreviewData`] →
//!    [`prepare`] → [`Prepared`] (anything expensive a view needs — line
//!    widths, frames as shared buffers — is computed here, off the UI thread);
//! 2. **UI thread**: [`make`] turns the [`Prepared`] value into a
//!    `Box<dyn ContentView>`, the pane shows it and calls
//!    [`ContentView::shown`] (upload images, start timers, render a PDF page
//!    at the pane's size).
//!
//! Adding a view for a new kind of data (the media player is the next one):
//! add a [`Prepared`] variant if the data needs work on the worker, an arm in
//! [`prepare`] and in [`make`], and a module with a type implementing
//! [`ContentView`]. Views reach themselves later (timers, task results,
//! player events) through their [`Handle`].

use std::any::Any;

use oc_grid::Frame;
use oc_preview::{MarkdownData, MediaData, PdfData, PreviewData};
use oc_vfs::VPath;

use super::{ID, PreviewView};
use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::input::Event;
use crate::main_window::with_view_as;
use crate::widgets::DrawCtx;

/// What the pane shows below its "View as" row.
pub trait ContentView: Any {
    /// "text", "markdown", "image", "pdf", "message", "media" (tests, logs).
    fn kind(&self) -> &'static str;

    /// Draw into `area` (inside the pane's frame, below the chips).
    /// `focused`: the preview side has the keyboard focus.
    fn draw(&self, app: &App, f: &mut Frame, area: Rect, ctx: &DrawCtx, focused: bool);

    /// Keys while the preview side is focused; mouse events inside the pane.
    fn handle_event(&mut self, _app: &mut App, _h: Handle, _ev: &Event, _area: Rect) -> Outcome {
        Outcome::Ignored
    }

    /// Became the pane's content (`area`: where it will be drawn).
    fn shown(&mut self, _app: &mut App, _h: Handle, _area: Rect) {}

    /// The pane changed size (window resized, font zoomed, chips row shown).
    fn resized(&mut self, _app: &mut App, _h: Handle, _area: Rect) {}

    /// The pane started loading another file (or this one again); the
    /// view stays shown until the next one is ready. Stop the sound.
    fn leaving(&mut self, _app: &mut App) {}

    /// Start playing (the pane moved on to this file at the end of the
    /// previous one).
    fn play(&mut self, _app: &mut App, _h: Handle) {}

    /// Replaced or the pane closed: remove images from the store, cancel
    /// timers, stop playback. The view is dropped right after.
    fn clear(&mut self, _app: &mut App) {}

    /// The text shown, where there is one (tests).
    fn text(&self) -> Option<String> {
        None
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// How a content view finds itself again from a timer or a task result:
/// the side its pane is on and which content it is (a later file's view
/// gets a new serial, so stale callbacks find nothing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle {
    pub side: usize,
    pub serial: u64,
}

impl Handle {
    /// Run `f` with the view (and its current area) if it is still shown.
    pub fn with<T: ContentView, R>(self, app: &mut App, f: impl FnOnce(&mut T, &mut App, Rect) -> R) -> Option<R> {
        with_view_as::<PreviewView, _>(app, self.side, ID, |pane, app| pane.with_content(app, self, f)).flatten()
    }
}

/// [`PreviewData`] made ready for a view, on the worker thread.
pub enum Prepared {
    Text(super::text::TextDoc),
    Markdown(MarkdownData),
    Image(super::image::ImagePrep),
    Pdf(PdfData),
    Message(String),
    /// Audio and video: the player ([`super::media`]) opens it when shown.
    Media(MediaData),
}

/// Worker thread: the expensive part of turning data into a view.
pub fn prepare(data: PreviewData) -> Prepared {
    match data {
        PreviewData::StyledText(t) => Prepared::Text(super::text::TextDoc::styled(t)),
        PreviewData::Hex(h) => Prepared::Text(super::text::TextDoc::plain(&h.text())),
        PreviewData::Listing(l) => Prepared::Text(super::text::TextDoc::plain(&l.text())),
        PreviewData::Markdown(m) => Prepared::Markdown(m),
        PreviewData::Image(i) => Prepared::Image(super::image::ImagePrep::new(i)),
        PreviewData::Pdf(p) => Prepared::Pdf(p),
        PreviewData::Message(m) => Prepared::Message(m),
        PreviewData::Media(m) => Prepared::Media(m),
    }
}

/// UI thread: the view for prepared data of `path`.
pub fn make(prepared: Prepared, path: &VPath) -> Box<dyn ContentView> {
    match prepared {
        Prepared::Text(doc) => Box::new(super::text::TextView::new(doc)),
        Prepared::Markdown(m) => Box::new(super::markdown::MarkdownView::new(m)),
        Prepared::Image(i) => Box::new(super::image::ImageView::new(i, path.clone())),
        Prepared::Pdf(p) => Box::new(super::pdf::PdfView::new(p)),
        Prepared::Message(m) => Box::new(super::message::MessageView::new(m, None)),
        Prepared::Media(m) => Box::new(super::media::MediaView::new(m, path.clone())),
    }
}
