//! Norton / FAR style menus: Left, Files, Commands, Options, Right.
//!
//! The menus are built from the command registry: a command declares
//! `.menu(Menu::Files, "ops")` and appears in that section; sections are
//! separated by lines and keep the order below. The same model feeds the
//! native macOS menu bar (built by the app binary with muda) and the
//! in-window strip that F9 opens ([`MenuBar`]).

use oc_grid::{Frame, Style};

use crate::app::App;
use crate::commands::{Command, CommandId};
use crate::geom::Rect;
use crate::input::{Event, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::{DrawCtx, DropdownMenu, MenuEntry, MenuOutcome, fill, width};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Menu {
    Left,
    Files,
    Commands,
    Options,
    Right,
}

impl Menu {
    pub const ALL: [Menu; 5] = [Menu::Left, Menu::Files, Menu::Commands, Menu::Options, Menu::Right];

    /// The title with its `&` hotkey.
    pub fn title(self) -> &'static str {
        match self {
            Menu::Left => "&Left",
            Menu::Files => "&Files",
            Menu::Commands => "&Commands",
            Menu::Options => "&Options",
            Menu::Right => "&Right",
        }
    }

    /// Section names in display order. A feature puts its commands in one of
    /// these (adding a section here is the only shared edit a new kind of
    /// menu group needs).
    pub fn sections(self) -> &'static [&'static str] {
        match self {
            Menu::Left | Menu::Right => &["mode", "location"],
            Menu::Files => &["select", "ops", "rename", "find", "quit"],
            Menu::Commands => &["palette", "preview", "bookmarks", "servers", "panels", "terminal"],
            Menu::Options => &["settings", "view", "zoom"],
        }
    }
}

/// One menu's items: command ids, `None` = separator.
pub fn menu_items(app: &App, menu: Menu) -> Vec<Option<CommandId>> {
    let mut items: Vec<Option<CommandId>> = Vec::new();
    for section in menu.sections() {
        let cmds = app.registry.in_menu_section(menu, section);
        if cmds.is_empty() {
            continue;
        }
        if !items.is_empty() {
            items.push(None);
        }
        items.extend(cmds.iter().map(|c| Some(c.id)));
    }
    items
}

fn entry_for(app: &App, cmd: &Command) -> MenuEntry {
    MenuEntry::Item {
        label: cmd.label_in_menu().to_owned(),
        shortcut: cmd.shortcut_text(),
        checked: cmd.is_checked(app),
        enabled: cmd.is_enabled(app),
    }
}

/// The in-window menu strip: titles and where each starts.
pub fn strip_titles(row: Rect) -> Vec<(Menu, Rect)> {
    let mut x = row.x + 1;
    Menu::ALL
        .iter()
        .map(|&m| {
            let w = width(&crate::widgets::parse_hotkey(m.title()).0) + 2;
            let r = Rect::new(x, row.y, w, 1);
            x += w;
            (m, r)
        })
        .collect()
}

/// Draw the strip; `open` highlights the open menu's title.
pub fn draw_strip(f: &mut Frame, row: Rect, open: Option<Menu>, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    crate::widgets::cover(f, row, ctx);
    fill(f, row, c.footer_bg);
    for (m, r) in strip_titles(row) {
        let style = if open == Some(m) { Style::new(c.cursor_fg, c.cursor_bg) } else { Style::new(c.footer_fg, c.footer_bg) };
        fill(f, r, style.bg);
        let (text, hot) = crate::widgets::parse_hotkey(m.title());
        crate::widgets::put_hotkey_label(f, r.x + 1, r.y, &text, hot, style, if open == Some(m) { style.fg } else { c.footer_key }, r.w);
    }
}

/// The menu bar while one of its menus is open (F9, or a click on the strip).
/// A modal: it takes all input until a command is chosen or it is closed.
pub struct MenuBar {
    pub menu: Menu,
    pub items: Vec<Option<CommandId>>,
    pub dropdown: DropdownMenu,
    /// The strip's row on screen.
    pub row: Rect,
}

impl MenuBar {
    pub fn open(app: &App, menu: Menu, row: Rect) -> MenuBar {
        let items = menu_items(app, menu);
        let entries = items
            .iter()
            .map(|id| match id.and_then(|id| app.registry.get(id)) {
                Some(cmd) => entry_for(app, cmd),
                None => MenuEntry::Separator,
            })
            .collect();
        let x = strip_titles(row).iter().find(|(m, _)| *m == menu).map_or(0, |(_, r)| r.x);
        MenuBar { menu, items, dropdown: DropdownMenu::new(entries, x, row.y + 1), row }
    }

    fn switch(&mut self, app: &App, delta: isize) {
        let i = Menu::ALL.iter().position(|&m| m == self.menu).unwrap_or(0) as isize;
        let next = Menu::ALL[(i + delta).rem_euclid(Menu::ALL.len() as isize) as usize];
        *self = MenuBar::open(app, next, self.row);
    }

    fn run(&self, app: &mut App, i: usize) {
        if let Some(Some(id)) = self.items.get(i) {
            let id: CommandId = id;
            app.later(move |app| app.run_command(id));
        }
    }
}

impl Modal for MenuBar {
    fn title(&self) -> &str {
        "menu"
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        draw_strip(f, self.row, Some(self.menu), ctx);
        self.dropdown.draw(f, screen, ctx);
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        let outcome = match ev {
            Event::Key(k) => self.dropdown.handle_key(k),
            Event::Mouse(m) => {
                if self.row.contains(m.col, m.row) {
                    if let Some((menu, _)) = strip_titles(self.row).into_iter().find(|(_, r)| r.contains(m.col, m.row)) {
                        if matches!(m.kind, MouseKind::Down(_)) && menu == self.menu {
                            return ModalOutcome::Close;
                        }
                        if menu != self.menu && !matches!(m.kind, MouseKind::Scroll(_)) {
                            *self = MenuBar::open(app, menu, self.row);
                        }
                    }
                    return ModalOutcome::Consumed;
                }
                self.dropdown.handle_mouse(m, screen)
            }
            _ => MenuOutcome::Ignored,
        };
        match outcome {
            MenuOutcome::Activated(i) => {
                self.run(app, i);
                ModalOutcome::Close
            }
            MenuOutcome::Close => ModalOutcome::Close,
            MenuOutcome::Prev => {
                self.switch(app, -1);
                ModalOutcome::Consumed
            }
            MenuOutcome::Next => {
                self.switch(app, 1);
                ModalOutcome::Consumed
            }
            MenuOutcome::Consumed => ModalOutcome::Consumed,
            MenuOutcome::Ignored => ModalOutcome::Ignored,
        }
    }
}
