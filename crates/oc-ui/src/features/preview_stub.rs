//! The Preview side view, as a placeholder: it follows the other panel's
//! cursor and shows what it would preview. The real previews (oc-preview's
//! registry, images, media…) replace [`PreviewView`]'s body later; the tab,
//! F3 and the mode persistence are already wired.

use oc_grid::{Frame, Style};
use oc_vfs::VPath;

use crate::app::App;
use crate::commands::{Command, Registry};
use crate::geom::Rect;
use crate::main_window::{self, SideMode, SideView};
use crate::menus::Menu;
use crate::widgets::{BoxFrame, DrawCtx, put};

pub const ID: &str = "preview";

pub fn register(reg: &mut Registry) {
    reg.add_side_view(|| Box::new(PreviewView::default()));
    reg.add(
        Command::new("preview.toggle", "Toggle preview", toggle_preview)
            .help("Preview the file under the cursor in the other panel")
            .keys(&["F3"])
            .fkey("F3", "Preview")
            .menu(Menu::Commands, "preview"),
    );
    for side in 0..2 {
        let (id, menu, title) =
            if side == 0 { ("left.preview", Menu::Left, "Left: Preview") } else { ("right.preview", Menu::Right, "Right: Preview") };
        reg.add(
            Command::new(id, title, move |app| main_window::toggle_view(app, side, ID))
                .help("Preview the other panel's file on this side")
                .menu(menu, "mode")
                .menu_label("Preview")
                .checked(move |app| app.main.sides[side].mode_id() == ID),
        );
    }
}

/// F3: preview the active panel's file in the other side, or close the preview.
pub fn toggle_preview(app: &mut App) {
    let active = app.main.active;
    let target = if app.main.sides[active].mode == SideMode::Files { 1 - active } else { active };
    main_window::toggle_view(app, target, ID);
}

#[derive(Default)]
pub struct PreviewView {
    pub target: Option<VPath>,
}

impl SideView for PreviewView {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> String {
        "Preview".into()
    }

    fn draw(&self, _app: &App, _side: usize, f: &mut Frame, frame: Rect, ctx: &DrawCtx, active: bool) {
        let c = &ctx.theme.colors;
        let name = self.target.as_ref().map(|t| t.name()).unwrap_or_default();
        let inner = BoxFrame { title: &name, subtitle: "", active }.draw(f, frame, ctx);
        if let Some(t) = &self.target {
            let muted = Style::new(c.muted, c.surface);
            put(f, inner.x + 1, inner.y + 1, &t.display(), muted, inner.w.saturating_sub(2));
            put(f, inner.x + 1, inner.y + 2, &t.fs.type_name(), muted, inner.w.saturating_sub(2));
        }
    }

    fn target_changed(&mut self, app: &mut App, _side: usize, target: Option<VPath>) {
        self.target = target;
        app.redraw();
    }

    fn deactivated(&mut self, _app: &mut App, _side: usize) {
        self.target = None;
    }
}
