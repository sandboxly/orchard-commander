//! Cursor and scrolling for lists and tables of any length. Only the visible
//! rows are ever touched, so 100 000 entries cost the same as 100.

use orchard_grid::Frame;

use super::scrollbar::Scrollbar;
use super::{DrawCtx, fill};
use crate::geom::Rect;
use crate::input::{Key, KeyEvent};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListState {
    pub len: usize,
    pub cursor: usize,
    /// First visible row.
    pub top: usize,
}

impl ListState {
    pub fn new(len: usize) -> Self {
        ListState { len, cursor: 0, top: 0 }
    }

    /// New length; cursor and scroll position are clamped.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.top = self.top.min(len.saturating_sub(1));
    }

    /// Put the cursor on `i` (clamped) and scroll it into view.
    pub fn select(&mut self, i: usize, visible: usize) {
        self.cursor = i.min(self.len.saturating_sub(1));
        self.ensure_visible(visible);
    }

    pub fn ensure_visible(&mut self, visible: usize) {
        let visible = visible.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + visible {
            self.top = self.cursor + 1 - visible;
        }
        // Don't leave empty rows at the bottom when the list could fill them.
        let max_top = self.len.saturating_sub(visible);
        self.top = self.top.min(max_top);
    }

    pub fn move_by(&mut self, delta: isize, visible: usize) {
        if self.len == 0 {
            return;
        }
        let target = (self.cursor as isize + delta).clamp(0, self.len as isize - 1);
        self.select(target as usize, visible);
    }

    /// Scroll the view (mouse wheel) without moving the cursor, like Qt's
    /// item views: the cursor may scroll out of view; the next cursor key
    /// brings it back. (Dragging it along would change the file a preview
    /// shows and F5/F8 act on just by scrolling.)
    pub fn scroll_by(&mut self, lines: isize, visible: usize) {
        let max_top = self.len.saturating_sub(visible.max(1));
        self.top = (self.top as isize + lines).clamp(0, max_top as isize) as usize;
    }

    /// The first row to show with `visible` rows: the scroll position,
    /// without leaving empty rows below the end.
    pub fn first_visible(&self, visible: usize) -> usize {
        self.top.min(self.len.saturating_sub(visible.max(1)))
    }

    /// Up/Down/PageUp/PageDown/Home/End (plain). Returns whether the key was used.
    pub fn handle_key(&mut self, ev: &KeyEvent, visible: usize) -> bool {
        if !ev.mods.is_empty() {
            return false;
        }
        let page = visible.saturating_sub(1).max(1) as isize;
        match ev.key {
            Key::Up => self.move_by(-1, visible),
            Key::Down => self.move_by(1, visible),
            Key::PageUp => self.move_by(-page, visible),
            Key::PageDown => self.move_by(page, visible),
            Key::Home => self.select(0, visible),
            Key::End => self.select(self.len.saturating_sub(1), visible),
            _ => return false,
        }
        true
    }

    /// The item at screen `row` inside `r`, if any.
    pub fn hit(&self, r: Rect, col: u16, row: u16) -> Option<usize> {
        if !r.contains(col, row) {
            return None;
        }
        let i = self.top + (row - r.y) as usize;
        (i < self.len).then_some(i)
    }

    /// Visible range for a view `visible` rows tall.
    pub fn visible_range(&self, visible: usize) -> std::ops::Range<usize> {
        self.top..(self.top + visible).min(self.len)
    }

    /// Fill `r` with the surface, call `row(frame, row_rect, index, is_cursor)`
    /// for each visible item, and draw a scrollbar if the list overflows.
    pub fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx, mut row: impl FnMut(&mut Frame, Rect, usize, bool)) {
        fill(f, r, ctx.theme.colors.surface);
        for (n, i) in self.visible_range(r.h as usize).enumerate() {
            row(f, r.row(n as u16), i, i == self.cursor);
        }
        Scrollbar { total: self.len, visible: r.h as usize, top: self.top }.draw(f, r, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_and_scrolling() {
        let mut l = ListState::new(100);
        l.handle_key(&KeyEvent::plain(Key::PageDown), 10);
        assert_eq!((l.cursor, l.top), (9, 0));
        l.handle_key(&KeyEvent::plain(Key::Down), 10);
        assert_eq!((l.cursor, l.top), (10, 1));
        l.handle_key(&KeyEvent::plain(Key::End), 10);
        assert_eq!((l.cursor, l.top), (99, 90));
        l.scroll_by(-50, 10);
        assert_eq!((l.cursor, l.top), (99, 40), "the wheel scrolls the view, not the cursor (Qt)");
        l.handle_key(&KeyEvent::plain(Key::Up), 10);
        assert_eq!((l.cursor, l.top), (98, 89), "a cursor key brings the cursor back into view");
        l.scroll_by(1000, 10);
        assert_eq!(l.top, 90, "never past the end");
        l.handle_key(&KeyEvent::plain(Key::Home), 10);
        assert_eq!((l.cursor, l.top), (0, 0));
        l.set_len(0);
        l.move_by(1, 10);
        assert_eq!(l.cursor, 0);
        assert_eq!(ListState::new(5).hit(Rect::new(0, 2, 10, 10), 3, 4), Some(2));
        assert_eq!(ListState::new(5).hit(Rect::new(0, 2, 10, 10), 3, 9), None);
    }
}
