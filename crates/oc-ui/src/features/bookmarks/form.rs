//! Small pieces for the bookmark dialogs' forms: a drop-down choice field
//! (Qt's QComboBox) and the focus ring.

use oc_grid::{Frame, Style};

use crate::geom::Rect;
use crate::input::{Key, KeyEvent, MouseEvent, MouseKind};
use crate::widgets::{DrawCtx, DropdownMenu, MenuEntry, MenuOutcome, fill, put_elided};

/// A field showing one of several options: ↑/↓ change it, Space (or a
/// click) opens the list.
pub struct Choice {
    pub options: Vec<String>,
    pub selected: usize,
    /// The open list, if any.
    pub open: Option<DropdownMenu>,
}

impl Choice {
    pub fn new(options: Vec<String>) -> Self {
        Choice { options, selected: 0, open: None }
    }

    pub fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx, focused: bool) {
        let c = &ctx.theme.colors;
        fill(f, r, c.background);
        let style = if focused { Style::new(c.cursor_fg, c.cursor_bg) } else { Style::new(c.foreground, c.background) };
        let text = self.options.get(self.selected).map_or("", String::as_str);
        let w = crate::widgets::width(text).min(r.w.saturating_sub(2));
        fill(f, Rect::new(r.x, r.y, w, 1), style.bg);
        put_elided(f, r.x, r.y, text, style, r.w.saturating_sub(2));
        if r.w >= 2 {
            f.put_char(r.right() - 1, r.y, '▾', Style::new(c.muted, c.background));
        }
    }

    /// The open list, drawn last (over everything else).
    pub fn draw_popup(&self, f: &mut Frame, screen: Rect, ctx: &DrawCtx) {
        if let Some(m) = &self.open {
            m.draw(f, screen, ctx);
        }
    }

    fn open_at(&mut self, r: Rect) {
        let entries = self.options.iter().map(|o| MenuEntry::item(&o.replace('&', "&&"), "")).collect();
        let mut m = DropdownMenu::new(entries, r.x.saturating_sub(1), r.y + 1);
        m.cursor = self.selected;
        self.open = Some(m);
    }

    /// Keys while focused (`r`: the field). Returns whether it used the key.
    pub fn handle_key(&mut self, k: &KeyEvent, r: Rect) -> bool {
        if let Some(m) = &mut self.open {
            match m.handle_key(k) {
                MenuOutcome::Activated(i) => {
                    self.selected = i;
                    self.open = None;
                }
                MenuOutcome::Close => self.open = None,
                _ => {}
            }
            return true;
        }
        if !k.mods.is_empty() && !(k.mods.alt && k.key == Key::Down) {
            return false;
        }
        let n = self.options.len();
        match k.key {
            Key::Down if k.mods.alt => self.open_at(r),
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(n.saturating_sub(1)),
            Key::Home => self.selected = 0,
            Key::End => self.selected = n.saturating_sub(1),
            Key::Space => self.open_at(r),
            _ => return false,
        }
        true
    }

    /// Mouse events: true if the choice used it (a click on the field or the open list).
    pub fn handle_mouse(&mut self, m: &MouseEvent, r: Rect, screen: Rect) -> bool {
        if let Some(menu) = &mut self.open {
            match menu.handle_mouse(m, screen) {
                MenuOutcome::Activated(i) => {
                    self.selected = i;
                    self.open = None;
                }
                MenuOutcome::Close => self.open = None,
                _ => {}
            }
            return true;
        }
        if matches!(m.kind, MouseKind::Down(_)) && r.contains(m.col, m.row) {
            self.open_at(r);
            return true;
        }
        false
    }
}

/// Tab / Shift+Tab over `n` focus slots, skipping those `enabled` says no to.
pub fn cycle_focus(current: usize, n: usize, backwards: bool, enabled: impl Fn(usize) -> bool) -> usize {
    let mut i = current;
    for _ in 0..n {
        i = if backwards { (i + n - 1) % n } else { (i + 1) % n };
        if enabled(i) {
            return i;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choosing() {
        let r = Rect::new(10, 5, 20, 1);
        let mut c = Choice::new(vec!["Top level".into(), "Work".into(), "Work › Old".into()]);
        assert!(c.handle_key(&KeyEvent::plain(Key::Down), r));
        assert_eq!(c.selected, 1);
        assert!(c.handle_key(&KeyEvent::plain(Key::Space), r));
        assert!(c.open.is_some());
        c.handle_key(&KeyEvent::plain(Key::Down), r);
        c.handle_key(&KeyEvent::plain(Key::Enter), r);
        assert_eq!((c.selected, c.open.is_none()), (2, true));
        assert!(!c.handle_key(&KeyEvent::plain(Key::Enter), r), "Enter is the dialog's when closed");
        assert_eq!(cycle_focus(0, 4, false, |i| i != 1), 2);
        assert_eq!(cycle_focus(0, 4, true, |_| true), 3);
    }
}
