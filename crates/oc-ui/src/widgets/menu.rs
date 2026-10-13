//! A dropdown menu: a box of items with shortcuts, check marks and
//! separators, opened below a point (menu strip, panel title, …).

use oc_grid::{Frame, Style};

use super::{DrawCtx, fill, parse_hotkey, put, put_hotkey_label, put_right, width};
use crate::geom::Rect;
use crate::input::{Key, KeyEvent, MouseEvent, MouseKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Item {
        /// May mark a hotkey with `&`.
        label: String,
        /// Shown right-aligned ("⌘R").
        shortcut: String,
        /// Some: a checkable item.
        checked: Option<bool>,
        enabled: bool,
    },
    Separator,
}

impl MenuEntry {
    pub fn item(label: &str, shortcut: &str) -> Self {
        MenuEntry::Item { label: label.into(), shortcut: shortcut.into(), checked: None, enabled: true }
    }

    fn selectable(&self) -> bool {
        matches!(self, MenuEntry::Item { enabled: true, .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuOutcome {
    Ignored,
    Consumed,
    /// Entry `i` was chosen.
    Activated(usize),
    /// Escape or a click outside.
    Close,
    /// Left / Right: switch to the neighbouring menu of a menu bar.
    Prev,
    Next,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownMenu {
    pub entries: Vec<MenuEntry>,
    pub cursor: usize,
    /// Top-left corner of the box.
    pub x: u16,
    pub y: u16,
}

impl DropdownMenu {
    pub fn new(entries: Vec<MenuEntry>, x: u16, y: u16) -> Self {
        let cursor = entries.iter().position(MenuEntry::selectable).unwrap_or(0);
        DropdownMenu { entries, cursor, x, y }
    }

    /// The box, moved left/up to stay on `screen`.
    pub fn rect(&self, screen: Rect) -> Rect {
        let (mut lw, mut sw) = (0u16, 0u16);
        for e in &self.entries {
            if let MenuEntry::Item { label, shortcut, .. } = e {
                lw = lw.max(width(&parse_hotkey(label).0));
                sw = sw.max(width(shortcut));
            }
        }
        // border + space + check column (2) + label + gap (3) + shortcut + space + border
        let w = (lw + 2 + if sw > 0 { sw + 3 } else { 0 } + 4).min(screen.w);
        let h = (self.entries.len() as u16 + 2).min(screen.h);
        let x = self.x.min(screen.right().saturating_sub(w));
        let y = self.y.min(screen.bottom().saturating_sub(h));
        Rect::new(x, y, w, h)
    }

    fn entry_at(&self, screen: Rect, col: u16, row: u16) -> Option<usize> {
        let r = self.rect(screen).inset(1, 1);
        if !r.contains(col, row) {
            return None;
        }
        let i = (row - r.y) as usize;
        (i < self.entries.len()).then_some(i)
    }

    pub fn draw(&self, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = self.rect(screen);
        super::cover(f, r, ctx);
        fill(f, r, c.surface);
        f.draw_box(r.cell_rect(), oc_grid::BoxStyle::Single, Style::new(c.border_active, c.surface));
        let inner = r.inset(1, 1);
        for (i, e) in self.entries.iter().enumerate() {
            let row = inner.row(i as u16);
            if row.is_empty() {
                break;
            }
            match e {
                MenuEntry::Separator => {
                    f.fill(row.cell_rect(), '─', Style::new(c.border, c.surface));
                    f.put_char(r.x, row.y, '├', Style::new(c.border_active, c.surface));
                    f.put_char(r.right() - 1, row.y, '┤', Style::new(c.border_active, c.surface));
                }
                MenuEntry::Item { label, shortcut, checked, enabled } => {
                    let on = i == self.cursor && *enabled;
                    let style = if on {
                        Style::new(c.cursor_fg, c.cursor_bg)
                    } else if *enabled {
                        Style::new(c.foreground, c.surface)
                    } else {
                        Style::new(c.muted, c.surface)
                    };
                    fill(f, row, style.bg);
                    if *checked == Some(true) {
                        put(f, row.x + 1, row.y, "✓", style, 1);
                    }
                    let (text, hot) = parse_hotkey(label);
                    let hot_color = if on { c.cursor_fg } else { c.accent };
                    put_hotkey_label(f, row.x + 3, row.y, &text, hot, style, hot_color, row.w.saturating_sub(4));
                    if !shortcut.is_empty() {
                        let s = if on { style } else { Style::new(c.muted, c.surface) };
                        put_right(f, row.right() - 1, row.y, shortcut, s, row.x + 3 + width(&text) + 1);
                    }
                }
            }
        }
    }

    fn step(&mut self, delta: isize) {
        let n = self.entries.len() as isize;
        if n == 0 {
            return;
        }
        let mut i = self.cursor as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if self.entries[i as usize].selectable() {
                self.cursor = i as usize;
                return;
            }
        }
    }

    pub fn handle_key(&mut self, ev: &KeyEvent) -> MenuOutcome {
        let plain = ev.mods.is_empty() || ev.mods == crate::input::Mods::SHIFT;
        match ev.key {
            Key::Up if plain => self.step(-1),
            Key::Down if plain => self.step(1),
            Key::Home if plain => {
                self.cursor = self.entries.len().saturating_sub(1);
                self.step(1);
            }
            Key::End if plain => {
                self.cursor = 0;
                self.step(-1);
            }
            Key::Enter | Key::Space if plain => {
                return if self.entries.get(self.cursor).is_some_and(MenuEntry::selectable) {
                    MenuOutcome::Activated(self.cursor)
                } else {
                    MenuOutcome::Consumed
                };
            }
            Key::Escape => return MenuOutcome::Close,
            Key::Left if plain => return MenuOutcome::Prev,
            Key::Right if plain => return MenuOutcome::Next,
            Key::Char(ch) if ev.mods.is_empty() || ev.mods.alt => {
                let hit = self.entries.iter().position(|e| match e {
                    MenuEntry::Item { label, enabled: true, .. } => parse_hotkey(label).1.is_some_and(|(_, k)| k == ch),
                    _ => false,
                });
                return match hit {
                    Some(i) => {
                        self.cursor = i;
                        MenuOutcome::Activated(i)
                    }
                    None => MenuOutcome::Consumed, // a menu swallows typing
                };
            }
            _ => return MenuOutcome::Consumed,
        }
        MenuOutcome::Consumed
    }

    pub fn handle_mouse(&mut self, ev: &MouseEvent, screen: Rect) -> MenuOutcome {
        let at = self.entry_at(screen, ev.col, ev.row);
        let inside = self.rect(screen).contains(ev.col, ev.row);
        match ev.kind {
            MouseKind::Move => {
                if let Some(i) = at.filter(|&i| self.entries[i].selectable()) {
                    self.cursor = i;
                    return MenuOutcome::Consumed;
                }
                MenuOutcome::Ignored
            }
            MouseKind::Up(_) | MouseKind::Down(_) | MouseKind::DoubleClick(_) => match at {
                Some(i) if self.entries[i].selectable() => {
                    if matches!(ev.kind, MouseKind::Up(_)) {
                        self.cursor = i;
                        MenuOutcome::Activated(i)
                    } else {
                        self.cursor = i;
                        MenuOutcome::Consumed
                    }
                }
                _ if inside => MenuOutcome::Consumed,
                _ if matches!(ev.kind, MouseKind::Down(_)) => MenuOutcome::Close,
                _ => MenuOutcome::Ignored,
            },
            MouseKind::Scroll(_) => MenuOutcome::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Mods, MouseButton};

    fn menu() -> DropdownMenu {
        DropdownMenu::new(
            vec![
                MenuEntry::item("&Copy", "F5"),
                MenuEntry::Separator,
                MenuEntry::Item { label: "Disabled".into(), shortcut: String::new(), checked: None, enabled: false },
                MenuEntry::item("&Quit", "⌘Q"),
            ],
            2,
            1,
        )
    }

    #[test]
    fn keys_skip_separators_and_disabled() {
        let mut m = menu();
        assert_eq!(m.cursor, 0);
        m.handle_key(&KeyEvent::plain(Key::Down));
        assert_eq!(m.cursor, 3);
        m.handle_key(&KeyEvent::plain(Key::Down));
        assert_eq!(m.cursor, 0, "wraps");
        assert_eq!(m.handle_key(&KeyEvent::plain(Key::Char('q'))), MenuOutcome::Activated(3));
        assert_eq!(m.handle_key(&KeyEvent::plain(Key::Escape)), MenuOutcome::Close);
        assert_eq!(m.handle_key(&KeyEvent::plain(Key::Right)), MenuOutcome::Next);
        assert_eq!(m.handle_key(&KeyEvent::new(Key::Char('x'), Mods::NONE)), MenuOutcome::Consumed);
    }

    #[test]
    fn mouse_hits() {
        let mut m = menu();
        let screen = Rect::new(0, 0, 80, 24);
        let r = m.rect(screen);
        assert_eq!((r.x, r.y, r.h), (2, 1, 6));
        let click = |kind, row| MouseEvent { kind, col: 5, row, mods: Mods::NONE };
        assert_eq!(m.handle_mouse(&click(MouseKind::Up(MouseButton::Left), 5), screen), MenuOutcome::Activated(3));
        assert_eq!(m.handle_mouse(&click(MouseKind::Up(MouseButton::Left), 3), screen), MenuOutcome::Consumed);
        let outside = MouseEvent { kind: MouseKind::Down(MouseButton::Left), col: 70, row: 20, mods: Mods::NONE };
        assert_eq!(m.handle_mouse(&outside, screen), MenuOutcome::Close);
    }
}
