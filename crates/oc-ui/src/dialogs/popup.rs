//! A context menu: a [`DropdownMenu`] at a point (right-click on a preview
//! image…) that reports the chosen entry and closes.

use oc_grid::Frame;

use crate::app::App;
use crate::geom::Rect;
use crate::input::Event;
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::{DrawCtx, DropdownMenu, MenuEntry, MenuOutcome};

type OnChoice = Box<dyn FnOnce(&mut App, usize)>;

pub struct PopupMenu {
    menu: DropdownMenu,
    on_choice: Option<OnChoice>,
}

impl PopupMenu {
    /// A menu with its top-left corner at (`col`, `row`) (moved to stay on
    /// screen); `on_choice` gets the index of the chosen entry.
    pub fn new(entries: Vec<MenuEntry>, col: u16, row: u16, on_choice: impl FnOnce(&mut App, usize) + 'static) -> Self {
        PopupMenu { menu: DropdownMenu::new(entries, col, row), on_choice: Some(Box::new(on_choice)) }
    }
}

impl Modal for PopupMenu {
    fn title(&self) -> &str {
        "popup menu"
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        self.menu.draw(f, screen, ctx);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let outcome = match ev {
            Event::Key(k) => self.menu.handle_key(k),
            Event::Mouse(m) => self.menu.handle_mouse(m, screen),
            _ => MenuOutcome::Ignored,
        };
        match outcome {
            MenuOutcome::Activated(i) => {
                if let Some(f) = self.on_choice.take() {
                    app.later(move |app| f(app, i));
                }
                ModalOutcome::Close
            }
            MenuOutcome::Close => ModalOutcome::Close,
            MenuOutcome::Ignored => ModalOutcome::Ignored,
            _ => ModalOutcome::Consumed,
        }
    }

    fn text(&self) -> String {
        self.menu
            .entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item { label, .. } => Some(crate::widgets::parse_hotkey(label).0),
                MenuEntry::Separator => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
