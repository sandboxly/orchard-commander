//! The bookmark menu (⌘D): bookmarks with their hotkey column, folders as
//! submenus, then "+ Add current folder…" / "- Remove current folder (…)"
//! and "* Organize bookmarks…" (port of `build_menu` and `HotkeyMenu`).
//!
//! Typing an entry's hotkey opens it; if several entries share the letter,
//! it moves between them and Enter opens the highlighted one (like Total
//! Commander, the same on every platform). Hovering a folder opens its
//! submenu, as in a Qt menu.

use oc_config::Spec;
use oc_config::bookmarks::{self as store, Bookmark, Item};
use oc_grid::Frame;

use crate::app::App;
use crate::geom::Rect;
use crate::input::{Event, Key, MouseKind};
use crate::modal::{Modal, ModalOutcome};
use crate::widgets::{DrawCtx, DropdownMenu, MenuEntry, MenuOutcome};

use super::menu_text;

/// The widest a bookmark's path column gets (cells).
const DETAIL_MAX: usize = 56;

/// What choosing an entry does.
enum Action {
    Open(Bookmark),
    Folder(Vec<Item>),
    Add,
    Remove(String),
    Organize,
    Nothing,
}

/// One open menu: the top one or a folder's submenu.
struct Level {
    actions: Vec<Action>,
    /// Each entry's hotkey (lower case).
    hotkeys: Vec<Option<char>>,
    /// What the entry shows (hotkey column + title), for tests.
    labels: Vec<Option<String>>,
    menu: DropdownMenu,
    /// The user has moved the highlight (like a Qt menu's active action,
    /// which is unset when the menu opens): the first hotkey press goes to
    /// the first entry with that key.
    moved: bool,
}

impl Level {
    fn new(x: u16, y: u16) -> Self {
        Level { actions: Vec::new(), hotkeys: Vec::new(), labels: Vec::new(), menu: DropdownMenu::new(Vec::new(), x, y), moved: false }
    }

    fn push(&mut self, label: String, detail: String, hotkey: Option<char>, action: Action) {
        let enabled = !matches!(action, Action::Nothing);
        self.menu.entries.push(MenuEntry::Item {
            // The dropdown reads "&" as a hotkey marker; ours are drawn as a column.
            label: label.replace('&', "&&"),
            // Long paths are shortened at the front so the menu stays a menu.
            shortcut: oc_grid::elide_left(&detail, DETAIL_MAX).into_owned(),
            checked: None,
            enabled,
        });
        self.labels.push(Some(label));
        self.hotkeys.push(hotkey);
        self.actions.push(action);
    }

    fn separator(&mut self) {
        self.menu.entries.push(MenuEntry::Separator);
        self.labels.push(None);
        self.hotkeys.push(None);
        self.actions.push(Action::Nothing);
    }

    /// The entries of `items` (a folder's children or the top level).
    fn fill(&mut self, items: &[Item]) {
        for item in items {
            match item {
                Item::Separator { .. } => self.separator(),
                Item::Folder(f) => {
                    let key = hotkey_char(&f.title);
                    self.push(menu_text(&f.title, None), "▸".into(), key, Action::Folder(f.items.clone()));
                }
                Item::Bookmark(b) => {
                    let detail = match &b.target {
                        Some(t) => {
                            let half = (DETAIL_MAX - 5) / 2;
                            let loc = oc_grid::elide_left(&b.location.describe(), half).into_owned();
                            format!("{loc}  ⇄  {}", oc_grid::elide_left(&t.describe(), half))
                        }
                        None => b.location.describe(),
                    };
                    self.push(menu_text(&b.title, None), detail, hotkey_char(&b.title), Action::Open(b.clone()));
                }
            }
        }
        // Start on the first entry that can be chosen.
        self.menu.cursor = self.menu.entries.iter().position(|e| matches!(e, MenuEntry::Item { enabled: true, .. })).unwrap_or(0);
    }
}

fn hotkey_char(title: &str) -> Option<char> {
    store::hotkey(title).and_then(|k| k.chars().next())
}

pub struct BookmarkMenu {
    levels: Vec<Level>,
    /// The side bookmarks open on.
    side: usize,
}

impl BookmarkMenu {
    /// The menu for `items`, opening below (x, y). `current`: the side's
    /// folder (offers to add or remove it).
    pub fn new(items: &[Item], current: Option<&Spec>, side: usize, x: u16, y: u16) -> Self {
        let mut top = Level::new(x, y);
        top.fill(items);
        if !items.is_empty() {
            top.separator();
        }
        let existing = current.and_then(|c| store::find_location(items, c.as_ref()));
        // Built-in entries use symbols, so every letter stays free for the user's bookmarks.
        if current.is_some() && existing.is_none() {
            top.push(menu_text("Add current folder…", Some("+")), String::new(), Some('+'), Action::Add);
        }
        if let Some(b) = existing {
            let label = menu_text(&format!("Remove current folder ({})", store::plain(&b.title).replace('&', "&&")), Some("-"));
            top.push(label, String::new(), Some('-'), Action::Remove(b.id.clone()));
        }
        top.push(menu_text("Organize bookmarks…", Some("*")), String::new(), Some('*'), Action::Organize);
        if top.menu.entries.first().is_some_and(|e| !matches!(e, MenuEntry::Item { enabled: true, .. })) {
            top.menu.cursor = top.menu.entries.iter().position(|e| matches!(e, MenuEntry::Item { enabled: true, .. })).unwrap_or(0);
        }
        BookmarkMenu { levels: vec![top], side }
    }

    /// The entries of the innermost open menu as shown ("S  Src"), without separators.
    pub fn labels(&self) -> Vec<String> {
        self.levels.last().map(|l| l.labels.iter().flatten().cloned().collect()).unwrap_or_default()
    }

    /// The highlighted entry of the innermost open menu.
    pub fn highlighted(&self) -> Option<String> {
        let l = self.levels.last()?;
        l.labels.get(l.menu.cursor).cloned().flatten()
    }

    /// How many menus are open (1 = no submenu).
    pub fn depth(&self) -> usize {
        self.levels.len()
    }

    fn selectable(level: &Level, i: usize) -> bool {
        matches!(level.menu.entries.get(i), Some(MenuEntry::Item { enabled: true, .. }))
    }

    /// Choose entry `i` of the innermost menu.
    fn activate(&mut self, app: &mut App, i: usize, screen: Rect) -> ModalOutcome {
        let Some(level) = self.levels.last() else { return ModalOutcome::Close };
        if !Self::selectable(level, i) {
            return ModalOutcome::Consumed;
        }
        let side = self.side;
        match &level.actions[i] {
            Action::Folder(children) => {
                let r = level.menu.rect(screen);
                let mut sub = Level::new(r.right(), r.y + i as u16);
                sub.fill(children);
                if children.is_empty() {
                    sub.push("   (empty)".into(), String::new(), None, Action::Nothing);
                }
                // No room on the right: open on the left of the parent, like Qt.
                let w = sub.menu.rect(screen).w;
                if r.right() + w > screen.right() && r.x >= w {
                    sub.menu.x = r.x - w;
                }
                self.levels.push(sub);
                return ModalOutcome::Consumed;
            }
            Action::Open(b) => {
                let b = b.clone();
                app.later(move |app| super::open_bookmark(app, &b, side));
            }
            Action::Add => app.later(move |app| super::add_bookmark(app, Some(side))),
            Action::Remove(id) => {
                let id = id.clone();
                app.later(move |app| super::remove_bookmark(app, &id));
            }
            Action::Organize => app.later(super::organize),
            Action::Nothing => return ModalOutcome::Consumed,
        }
        ModalOutcome::Close
    }

    /// A typed hotkey: the only entry with it opens; several: move between them.
    fn hotkey(&mut self, app: &mut App, ch: char, screen: Rect) -> ModalOutcome {
        let Some(level) = self.levels.last_mut() else { return ModalOutcome::Close };
        let matches: Vec<usize> =
            (0..level.hotkeys.len()).filter(|&i| level.hotkeys[i] == Some(ch) && Self::selectable(level, i)).collect();
        match matches.as_slice() {
            [] => ModalOutcome::Consumed,
            [one] => {
                level.menu.cursor = *one;
                self.activate(app, *one, screen)
            }
            several => {
                let next = match several.iter().position(|&i| i == level.menu.cursor) {
                    Some(p) if level.moved => several[(p + 1) % several.len()],
                    _ => several[0],
                };
                level.menu.cursor = next;
                level.moved = true;
                ModalOutcome::Consumed
            }
        }
    }

    /// The pointer moved over menu `j`: highlight the entry; another entry
    /// closes the submenus below, a folder opens its submenu (Qt opens it
    /// after a short delay).
    fn hover(&mut self, app: &mut App, j: usize, m: &crate::input::MouseEvent, screen: Rect) -> ModalOutcome {
        let level = &mut self.levels[j];
        let before = level.menu.cursor;
        let over = level.menu.handle_mouse(m, screen) == MenuOutcome::Consumed;
        let now = level.menu.cursor;
        level.moved |= now != before;
        if !over {
            return ModalOutcome::Consumed;
        }
        if now != before {
            self.levels.truncate(j + 1);
        }
        let folder = matches!(self.levels[j].actions.get(now), Some(Action::Folder(_)));
        if folder && self.levels.len() == j + 1 {
            return self.activate(app, now, screen);
        }
        ModalOutcome::Consumed
    }

    fn close_level(&mut self) -> ModalOutcome {
        if self.levels.len() > 1 {
            self.levels.pop();
            ModalOutcome::Consumed
        } else {
            ModalOutcome::Close
        }
    }
}

impl Modal for BookmarkMenu {
    fn title(&self) -> &str {
        "Bookmarks"
    }

    fn text(&self) -> String {
        self.labels().join("\n")
    }

    fn draw(&self, _app: &App, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        for level in &self.levels {
            level.menu.draw(f, screen, ctx);
        }
    }

    fn handle_event(&mut self, app: &mut App, ev: &Event, screen: Rect) -> ModalOutcome {
        match ev {
            Event::Key(k) => {
                if let Some(text) = k.typed_text() {
                    let mut chars = text.chars();
                    if let (Some(c), None) = (chars.next(), chars.next())
                        && !c.is_whitespace()
                    {
                        let ch = c.to_lowercase().next().unwrap_or(c);
                        return self.hotkey(app, ch, screen);
                    }
                }
                let plain = k.mods.is_empty();
                match k.key {
                    Key::Escape => return self.close_level(),
                    Key::Left if plain => return self.close_level(),
                    Key::Right | Key::Enter | Key::Space if plain => {
                        let level = self.levels.last().expect("a menu is open");
                        let i = level.menu.cursor;
                        if k.key == Key::Right && !matches!(level.actions.get(i), Some(Action::Folder(_))) {
                            return ModalOutcome::Consumed;
                        }
                        return self.activate(app, i, screen);
                    }
                    _ => {}
                }
                let level = self.levels.last_mut().expect("a menu is open");
                let before = level.menu.cursor;
                level.menu.handle_key(k);
                level.moved |= level.menu.cursor != before;
                ModalOutcome::Consumed
            }
            Event::Mouse(m) => {
                // The innermost menu under the pointer gets it; submenus below it close.
                let hit = self.levels.iter().rposition(|l| l.menu.rect(screen).contains(m.col, m.row));
                let Some(j) = hit else {
                    return match m.kind {
                        MouseKind::Down(_) | MouseKind::DoubleClick(_) => ModalOutcome::Close,
                        _ => ModalOutcome::Consumed,
                    };
                };
                if matches!(m.kind, MouseKind::Down(_) | MouseKind::Up(_)) {
                    self.levels.truncate(j + 1);
                }
                if m.kind == MouseKind::Move {
                    return self.hover(app, j, m, screen);
                }
                if j + 1 != self.levels.len() {
                    // Hovering a parent menu while a submenu is open.
                    let _ = self.levels[j].menu.handle_mouse(m, screen);
                    return ModalOutcome::Consumed;
                }
                let before = self.levels[j].menu.cursor;
                let outcome = self.levels[j].menu.handle_mouse(m, screen);
                self.levels[j].moved |= self.levels[j].menu.cursor != before;
                match outcome {
                    MenuOutcome::Activated(i) => self.activate(app, i, screen),
                    MenuOutcome::Close => ModalOutcome::Close,
                    _ => ModalOutcome::Consumed,
                }
            }
            _ => ModalOutcome::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(p: &str) -> Spec {
        Spec::local(p)
    }

    fn sample() -> Vec<Item> {
        vec![
            Bookmark::new("&Src", local("/t/src"), None).into(),
            Bookmark::new("&Dst", local("/t/dst"), Some(local("/t/src"))).into(),
            Bookmark::new("&Sub", local("/t/src/sub"), None).into(),
            Item::separator(),
            oc_config::Folder::new("&Work", vec![Bookmark::new("&Inside", local("/t"), None).into()]).into(),
            oc_config::Folder::new("&Empty", vec![]).into(),
            Bookmark::new("Fish && Chips", local("/t/f"), None).into(),
        ]
    }

    #[test]
    fn entries_and_builtins() {
        let items = sample();
        let m = BookmarkMenu::new(&items, Some(&local("/t/src")), 0, 2, 1);
        assert_eq!(
            m.labels(),
            [
                "S  Src",
                "D  Dst",
                "S  Sub",
                "W  Work",
                "E  Empty",
                "   Fish & Chips",
                "-  Remove current folder (Src)",
                "*  Organize bookmarks…"
            ]
        );
        let level = &m.levels[0];
        assert!(matches!(&level.menu.entries[1], MenuEntry::Item { shortcut, .. } if shortcut.contains("  ⇄  ")));
        // A folder not bookmarked yet: "+" instead of "-".
        let m = BookmarkMenu::new(&items, Some(&local("/elsewhere")), 0, 2, 1);
        assert!(m.labels().contains(&"+  Add current folder…".to_owned()));
        assert!(!m.labels().iter().any(|l| l.starts_with('-')));
        // A place that can't be bookmarked: neither.
        let m = BookmarkMenu::new(&[], None, 0, 2, 1);
        assert_eq!(m.labels(), ["*  Organize bookmarks…"]);
    }
}
