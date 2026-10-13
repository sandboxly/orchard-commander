//! Form controls for the settings window and the scheme editor, in the
//! grid's terms: a combo box (a field plus a popup list), a spin box, a
//! heading. Their values always come from the caller (the settings), so the
//! state here is only what is open and where its cursor is.

use oc_grid::{Flags, Frame, Style};

use crate::geom::Rect;
use crate::input::{Key, KeyEvent, MouseButton, MouseEvent, MouseKind};
use crate::widgets::{BoxFrame, DrawCtx, ListState, cover, fill, put, put_elided, width};

/// One entry of a combo box; `None` labels are separators (1.x puts one
/// between the built-in and the user's color schemes).
pub type Choice = Option<String>;

/// At most this many items show at once (Qt's `setMaxVisibleItems(20)`).
pub const MAX_VISIBLE: u16 = 20;

/// A heading: accent, bold (1.x's `dialogTitle` labels).
pub fn heading(f: &mut Frame, x: u16, y: u16, text: &str, max: u16, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    put(f, x, y, text, Style::new(c.accent, c.surface).with(Flags::BOLD), max);
}

/// A label in the form's text color.
pub fn label(f: &mut Frame, x: u16, y: u16, text: &str, max: u16, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    put_elided(f, x, y, text, Style::new(c.foreground, c.surface), max);
}

/// Muted text (hints).
pub fn muted(f: &mut Frame, x: u16, y: u16, text: &str, max: u16, ctx: &DrawCtx) {
    let c = &ctx.theme.colors;
    put_elided(f, x, y, text, Style::new(c.muted, c.surface), max);
}

/// The look of a field: on the window background like text inputs; the
/// focused one in the active color, like a focused button.
fn field_style(focused: bool, ctx: &DrawCtx) -> Style {
    let c = &ctx.theme.colors;
    if focused { Style::new(c.surface, c.border_active).with(Flags::BOLD) } else { Style::new(c.foreground, c.background) }
}

/// A closed combo box: " value      ▾".
pub fn draw_combo(f: &mut Frame, r: Rect, text: &str, focused: bool, ctx: &DrawCtx) {
    if r.is_empty() {
        return;
    }
    let style = field_style(focused, ctx);
    fill(f, r, style.bg);
    put_elided(f, r.x + 1, r.y, text, style, r.w.saturating_sub(4));
    put(f, r.right().saturating_sub(2), r.y, "▾", style, 1);
}

/// A spin box: "‹ 13 pt ›" (←/→ or a click on an arrow changes it).
pub fn draw_spin(f: &mut Frame, r: Rect, text: &str, focused: bool, ctx: &DrawCtx) {
    if r.w < 4 {
        return;
    }
    let style = field_style(focused, ctx);
    fill(f, r, style.bg);
    put(f, r.x + 1, r.y, "‹", style, 1);
    let w = width(text).min(r.w.saturating_sub(4));
    put(f, r.x + (r.w - w) / 2, r.y, text, style, w);
    put(f, r.right().saturating_sub(2), r.y, "›", style, 1);
}

/// Which way a click on a spin box at `col` turns it.
pub fn spin_hit(r: Rect, col: u16) -> i32 {
    if col < r.x + r.w / 2 { -1 } else { 1 }
}

/// The next selectable item from `current` in direction `delta` (wrapping
/// off: stops at the ends, like Qt's combo with the arrow keys).
pub fn step(items: &[Choice], current: usize, delta: i32) -> usize {
    let mut i = current as i64;
    loop {
        i += delta as i64;
        if i < 0 || i >= items.len() as i64 {
            return current;
        }
        if items[i as usize].is_some() {
            return i as usize;
        }
    }
}

/// An open combo box's list, over everything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popup {
    /// The field it belongs to.
    pub field: Rect,
    pub list: ListState,
    /// What it was opened for (the caller's control id).
    pub owner: usize,
}

/// What a key or click did to an open popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupOutcome {
    Consumed,
    /// Item `i` was chosen.
    Chosen(usize),
    /// Closed without a choice.
    Closed,
}

impl Popup {
    pub fn open(owner: usize, field: Rect, items: &[Choice], current: usize) -> Popup {
        let mut p = Popup { field, list: ListState::new(items.len()), owner };
        let visible = p.visible(items.len());
        p.list.select(current, visible);
        p
    }

    fn visible(&self, n: usize) -> usize {
        n.min(MAX_VISIBLE as usize).max(1)
    }

    /// The box: below the field if it fits, else above; at least as wide as the field.
    pub fn rect(&self, items: &[Choice], screen: Rect) -> Rect {
        let longest = items.iter().flatten().map(|s| width(s)).max().unwrap_or(0);
        let w = (longest + 4).max(self.field.w).min(screen.w);
        let h = (self.visible(items.len()) as u16 + 2).min(screen.h);
        let x = self.field.x.min(screen.right().saturating_sub(w));
        let below = self.field.bottom();
        let y = if below + h <= screen.bottom() { below } else { self.field.y.saturating_sub(h) };
        Rect::new(x, y, w, h)
    }

    pub fn draw(&self, f: &mut Frame, items: &[Choice], screen: Rect, ctx: &DrawCtx) {
        let c = &ctx.theme.colors;
        let r = self.rect(items, screen);
        cover(f, r, ctx);
        let inner = BoxFrame { title: "", subtitle: "", active: true }.draw(f, r, ctx);
        let mut list = self.list;
        list.top = list.first_visible(inner.h as usize);
        list.draw(f, inner, ctx, |f, row, i, cursor| match &items[i] {
            Some(text) => {
                let style = if cursor { Style::new(c.cursor_fg, c.cursor_bg) } else { Style::new(c.foreground, c.surface) };
                fill(f, row, style.bg);
                put_elided(f, row.x + 1, row.y, text, style, row.w.saturating_sub(2));
            }
            None => {
                let line = "─".repeat(row.w.saturating_sub(2) as usize);
                put(f, row.x + 1, row.y, &line, Style::new(c.border, c.surface), row.w.saturating_sub(2));
            }
        });
    }

    pub fn handle_key(&mut self, k: &KeyEvent, items: &[Choice]) -> PopupOutcome {
        let visible = self.visible(items.len());
        match k.key {
            Key::Escape => PopupOutcome::Closed,
            Key::Enter | Key::Space if k.mods.is_empty() => match items.get(self.list.cursor) {
                Some(Some(_)) => PopupOutcome::Chosen(self.list.cursor),
                _ => PopupOutcome::Consumed,
            },
            Key::Tab => PopupOutcome::Closed,
            Key::Up | Key::Down if k.mods.is_empty() => {
                let i = step(items, self.list.cursor, if k.key == Key::Up { -1 } else { 1 });
                self.list.select(i, visible);
                PopupOutcome::Consumed
            }
            _ => {
                if self.list.handle_key(k, visible) {
                    // PageUp/Home… may land on a separator: move off it.
                    if items.get(self.list.cursor).is_some_and(Option::is_none) {
                        let i = step(items, self.list.cursor, 1);
                        self.list.select(i, visible);
                    }
                } else if let Some(text) = k.typed_text() {
                    // Type-to-jump to the next item starting with the letter.
                    let needle = text.to_lowercase();
                    let n = items.len();
                    for d in 1..=n {
                        let i = (self.list.cursor + d) % n;
                        if items[i].as_ref().is_some_and(|s| s.to_lowercase().starts_with(&needle)) {
                            self.list.select(i, visible);
                            break;
                        }
                    }
                }
                PopupOutcome::Consumed
            }
        }
    }

    pub fn handle_mouse(&mut self, m: &MouseEvent, items: &[Choice], screen: Rect) -> PopupOutcome {
        let r = self.rect(items, screen);
        let inner = r.inset(1, 1);
        let visible = self.visible(items.len());
        match m.kind {
            MouseKind::Scroll(lines) => {
                self.list.scroll_by((-lines * 3.0).round() as isize, visible);
                PopupOutcome::Consumed
            }
            MouseKind::Move => {
                if let Some(i) = self.list.hit(inner, m.col, m.row).filter(|&i| items[i].is_some()) {
                    self.list.cursor = i;
                }
                PopupOutcome::Consumed
            }
            MouseKind::Down(_) | MouseKind::DoubleClick(_) if !r.contains(m.col, m.row) => PopupOutcome::Closed,
            MouseKind::Up(MouseButton::Left) => match self.list.hit(inner, m.col, m.row) {
                Some(i) if items[i].is_some() => PopupOutcome::Chosen(i),
                _ => PopupOutcome::Consumed,
            },
            _ => PopupOutcome::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Choice> {
        vec![Some("monokai".into()), Some("nord".into()), None, Some("Mine".into())]
    }

    #[test]
    fn steps_skip_separators_and_stop_at_the_ends() {
        let it = items();
        assert_eq!(step(&it, 1, 1), 3);
        assert_eq!(step(&it, 3, -1), 1);
        assert_eq!(step(&it, 3, 1), 3);
        assert_eq!(step(&it, 0, -1), 0);
    }

    #[test]
    fn popup_keys_and_placement() {
        let it = items();
        let screen = Rect::new(0, 0, 80, 30);
        let mut p = Popup::open(7, Rect::new(10, 5, 20, 1), &it, 1);
        assert_eq!(p.rect(&it, screen), Rect::new(10, 6, 20, 6), "below the field, as wide");
        assert_eq!(p.handle_key(&KeyEvent::plain(Key::Down), &it), PopupOutcome::Consumed);
        assert_eq!(p.list.cursor, 3, "over the separator");
        assert_eq!(p.handle_key(&KeyEvent::plain(Key::Enter), &it), PopupOutcome::Chosen(3));
        p.handle_key(&KeyEvent::typed("n"), &it);
        assert_eq!(p.list.cursor, 1, "type to jump");
        assert_eq!(p.handle_key(&KeyEvent::plain(Key::Escape), &it), PopupOutcome::Closed);
        // Near the bottom it opens above the field.
        let low = Popup::open(0, Rect::new(10, 27, 20, 1), &it, 0);
        assert_eq!(low.rect(&it, screen).bottom(), 27);
    }
}
