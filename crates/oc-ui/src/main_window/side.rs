//! One half of the main window: mode tabs above the file panel or a side
//! view (Preview, …).
//!
//! A side view is a feature's alternative to the file panel on a side,
//! registered with [`Registry::add_side_view`](crate::commands::Registry::add_side_view).
//! Each side gets its own instance; its tab appears after "Files".

use std::any::Any;

use oc_grid::Frame;
use oc_vfs::VPath;

use crate::app::{App, Outcome};
use crate::geom::Rect;
use crate::input::Event;
use crate::panel::FilePanel;
use crate::widgets::DrawCtx;

/// Makes a view for one side.
pub type SideViewFactory = fn() -> Box<dyn SideView>;

pub trait SideView {
    /// Stable id, also what `settings.extra["left_mode"]` stores ("preview").
    fn id(&self) -> &'static str;

    /// The tab label.
    fn title(&self) -> String;

    /// Draw into `frame` (the area a file panel's frame would take).
    /// `active`: this side has the focus.
    fn draw(&self, app: &App, side: usize, f: &mut Frame, frame: Rect, ctx: &DrawCtx, active: bool);

    /// Input while this side is active and shows this view.
    fn handle_event(&mut self, _app: &mut App, _side: usize, _ev: &Event, _frame: Rect) -> Outcome {
        Outcome::Ignored
    }

    /// Like Preview: showing it puts the focus on the other side's panel and
    /// the view follows that panel's cursor ([`target_changed`](Self::target_changed)).
    fn follows_other_panel(&self) -> bool {
        true
    }

    /// The other side's cursor moved (only while shown, and when shown).
    fn target_changed(&mut self, _app: &mut App, _side: usize, _target: Option<VPath>) {}

    /// The side switched away from this view (stop playback, free memory…).
    fn deactivated(&mut self, _app: &mut App, _side: usize) {}

    /// The window (or the font) changed size while shown; `frame` is the
    /// new area (render a page or image again at the new size…).
    fn resized(&mut self, _app: &mut App, _side: usize, _frame: Rect) {}

    /// The view as its own type, for [`with_view_as`] / [`view_as`] (timers
    /// and task results reaching their view; tests). `Some(self)` to opt in.
    fn as_any(&self) -> Option<&dyn Any> {
        None
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideMode {
    Files,
    /// Index into the side's views.
    View(usize),
}

pub struct Side {
    /// "left" / "right" (settings keys).
    pub name: &'static str,
    pub panel: FilePanel,
    pub mode: SideMode,
    /// Taken out while they handle events.
    pub views: Vec<Option<Box<dyn SideView>>>,
    /// Tab labels after "Files" (kept so drawing never needs a taken-out view).
    pub view_titles: Vec<String>,
    pub view_ids: Vec<&'static str>,
}

impl Side {
    pub fn new(name: &'static str, panel: FilePanel, factories: &[SideViewFactory]) -> Side {
        let views: Vec<Box<dyn SideView>> = factories.iter().map(|f| f()).collect();
        let view_titles = views.iter().map(|v| v.title()).collect();
        let view_ids = views.iter().map(|v| v.id()).collect();
        Side { name, panel, mode: SideMode::Files, views: views.into_iter().map(Some).collect(), view_titles, view_ids }
    }

    pub fn tab_labels(&self) -> Vec<&str> {
        std::iter::once("Files").chain(self.view_titles.iter().map(String::as_str)).collect()
    }

    pub fn mode_index(&self) -> usize {
        match self.mode {
            SideMode::Files => 0,
            SideMode::View(i) => i + 1,
        }
    }

    pub fn mode_id(&self) -> &'static str {
        match self.mode {
            SideMode::Files => "files",
            SideMode::View(i) => self.view_ids[i],
        }
    }

    pub fn view_index(&self, id: &str) -> Option<usize> {
        self.view_ids.iter().position(|v| *v == id)
    }

    /// The current view (None while showing files).
    pub fn view(&self) -> Option<&dyn SideView> {
        match self.mode {
            SideMode::View(i) => self.views.get(i).and_then(|v| v.as_deref()),
            SideMode::Files => None,
        }
    }
}

/// Run `f` with view `i` of `side` taken out of the app (so it may use `&mut App`).
pub fn with_view<R>(app: &mut App, side: usize, i: usize, f: impl FnOnce(&mut dyn SideView, &mut App) -> R) -> Option<R> {
    let mut view = app.main.sides[side].views.get_mut(i)?.take()?;
    let r = f(view.as_mut(), app);
    app.main.sides[side].views[i] = Some(view);
    Some(r)
}

/// Run `f` with `side`'s view `id` as a `T` (see [`SideView::as_any_mut`]),
/// taken out of the app meanwhile. None if there is no such view (or it is
/// taken out already, i.e. `f` would re-enter it).
pub fn with_view_as<T: 'static, R>(app: &mut App, side: usize, id: &str, f: impl FnOnce(&mut T, &mut App) -> R) -> Option<R> {
    let i = app.main.sides[side].view_index(id)?;
    let mut view = app.main.sides[side].views.get_mut(i)?.take()?;
    let r = view.as_any_mut().and_then(|a| a.downcast_mut::<T>()).map(|v| f(v, app));
    app.main.sides[side].views[i] = Some(view);
    r
}

/// `side`'s view `id` as a `T`, shown or not (drawing helpers, tests).
pub fn view_as<'a, T: 'static>(app: &'a App, side: usize, id: &str) -> Option<&'a T> {
    let s = &app.main.sides[side];
    let i = s.view_index(id)?;
    s.views.get(i)?.as_ref()?.as_any()?.downcast_ref::<T>()
}
