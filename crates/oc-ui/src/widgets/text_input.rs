//! A single-line text field: cursor, selection, clipboard, IME preedit.

use orchard_grid::{CursorShape, Frame, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{DrawCtx, fill, put};
use crate::clipboard::Clipboard;
use crate::geom::Rect;
use crate::input::{Event, Key, KeyEvent};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInput {
    pub text: String,
    /// Byte offset of the cursor (on a grapheme boundary).
    pub cursor: usize,
    /// The other end of the selection, if any.
    pub anchor: Option<usize>,
    /// Text being composed by an input method.
    pub preedit: String,
    pub placeholder: String,
}

impl TextInput {
    pub fn new(text: &str) -> Self {
        TextInput { text: text.to_owned(), cursor: text.len(), ..Default::default() }
    }

    /// With everything selected (typing replaces it), like Python's `selectAll()`.
    pub fn selected_all(text: &str) -> Self {
        let mut t = TextInput::new(text);
        t.select_all();
        t
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.cursor).then(|| (a.min(self.cursor), a.max(self.cursor)))
    }

    pub fn selected_text(&self) -> &str {
        self.selection().map_or("", |(a, b)| &self.text[a..b])
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
        self.anchor = None;
    }

    /// Replace the selection (if any) with `s`.
    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some((a, b)) => {
                self.text.replace_range(a..b, "");
                self.cursor = a;
                self.anchor = None;
                true
            }
            None => {
                self.anchor = None;
                false
            }
        }
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.text[..from].grapheme_indices(true).next_back().map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..].graphemes(true).next().map_or(from, |g| from + g.len())
    }

    fn prev_word(&self, from: usize) -> usize {
        let mut last = 0;
        for (i, w) in self.text[..from].split_word_bound_indices() {
            if w.chars().any(char::is_alphanumeric) {
                last = i;
            }
        }
        last
    }

    fn next_word(&self, from: usize) -> usize {
        for (i, w) in self.text[from..].split_word_bound_indices() {
            if w.chars().any(char::is_alphanumeric) {
                return from + i + w.len();
            }
        }
        self.text.len()
    }

    fn move_to(&mut self, pos: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
    }

    /// Handle an event; returns whether it was used. Enter/Escape/Tab and
    /// arrow up/down are left to the dialog.
    pub fn handle_event(&mut self, ev: &Event, clip: &mut dyn Clipboard) -> bool {
        match ev {
            Event::Key(k) => self.handle_key(k, clip),
            Event::Commit(s) => {
                self.preedit.clear();
                self.insert(s);
                true
            }
            Event::Preedit(s) => {
                self.preedit = s.clone();
                true
            }
            _ => false,
        }
    }

    pub fn handle_key(&mut self, ev: &KeyEvent, clip: &mut dyn Clipboard) -> bool {
        let m = ev.mods;
        let word = if cfg!(target_os = "macos") { m.alt } else { m.ctrl };
        let line = cfg!(target_os = "macos") && m.logo;
        match ev.key {
            Key::Left => {
                let pos = if line {
                    0
                } else if word {
                    self.prev_word(self.cursor)
                } else if !m.shift
                    && let Some((a, _)) = self.selection()
                {
                    a
                } else {
                    self.prev_boundary(self.cursor)
                };
                self.move_to(pos, m.shift);
            }
            Key::Right => {
                let pos = if line {
                    self.text.len()
                } else if word {
                    self.next_word(self.cursor)
                } else if !m.shift
                    && let Some((_, b)) = self.selection()
                {
                    b
                } else {
                    self.next_boundary(self.cursor)
                };
                self.move_to(pos, m.shift);
            }
            Key::Home => self.move_to(0, m.shift),
            Key::End => self.move_to(self.text.len(), m.shift),
            Key::Backspace => {
                if !self.delete_selection() {
                    let start = if line {
                        0
                    } else if word {
                        self.prev_word(self.cursor)
                    } else {
                        self.prev_boundary(self.cursor)
                    };
                    self.text.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
            }
            Key::Delete => {
                if !self.delete_selection() {
                    let end = if word { self.next_word(self.cursor) } else { self.next_boundary(self.cursor) };
                    self.text.replace_range(self.cursor..end, "");
                }
            }
            Key::Char('a') if m.has_primary() && !m.shift && !m.alt => self.select_all(),
            Key::Char('c') if m.has_primary() && !m.alt => {
                if self.selection().is_some() {
                    clip.set_text(self.selected_text());
                }
            }
            Key::Char('x') if m.has_primary() && !m.alt => {
                if self.selection().is_some() {
                    clip.set_text(self.selected_text());
                    self.delete_selection();
                }
            }
            Key::Char('v') if m.has_primary() && !m.alt => {
                if let Some(t) = clip.get_text() {
                    self.insert(t.lines().next().unwrap_or(""));
                }
            }
            _ => match ev.typed_text() {
                Some(t) => self.insert(t),
                None => return false,
            },
        }
        true
    }

    /// Clicking puts the cursor at the clicked column.
    pub fn click(&mut self, r: Rect, col: u16) {
        let offset = self.scroll(r.w);
        let target = (col.saturating_sub(r.x) as usize) + offset;
        let mut w = 0;
        let mut pos = self.text.len();
        for (i, g) in self.text.grapheme_indices(true) {
            if w + g.width() > target {
                pos = i;
                break;
            }
            w += g.width();
        }
        self.move_to(pos, false);
    }

    /// Columns scrolled off the left so the cursor stays visible.
    fn scroll(&self, w: u16) -> usize {
        let before = self.text[..self.cursor].width() + self.preedit.width();
        before.saturating_sub((w as usize).saturating_sub(1))
    }

    /// Draw on the window background; with `focused`, set the frame's cursor.
    pub fn draw(&self, f: &mut Frame, r: Rect, ctx: &DrawCtx, focused: bool) {
        let c = &ctx.theme.colors;
        fill(f, r, c.background);
        let normal = Style::new(c.foreground, c.background);
        if self.text.is_empty() && self.preedit.is_empty() {
            put(f, r.x, r.y, &self.placeholder, Style::new(c.muted, c.background), r.w);
        }
        let skip = self.scroll(r.w);
        let sel = self.selection();
        let mut x = 0usize; // display column in the text
        let mut col = r.x;
        let mut cursor_col = None;
        let emit = |f: &mut Frame, g: &str, style: Style, x: &mut usize, col: &mut u16| {
            let gw = g.width();
            if *x >= skip && *col + gw as u16 <= r.right() {
                *col = f.put_str_clipped(*col, r.y, g, style, r.right());
            }
            *x += gw;
        };
        for (i, g) in self.text.grapheme_indices(true) {
            if i == self.cursor {
                cursor_col = Some(col);
                for pg in self.preedit.graphemes(true) {
                    emit(f, pg, normal.with(orchard_grid::Flags::UNDERLINE), &mut x, &mut col);
                }
            }
            let selected = sel.is_some_and(|(a, b)| i >= a && i < b);
            let style = if selected && focused { Style::new(c.cursor_fg, c.cursor_bg) } else { normal };
            emit(f, g, style, &mut x, &mut col);
        }
        if self.cursor >= self.text.len() {
            cursor_col = Some(col);
            for pg in self.preedit.graphemes(true) {
                emit(f, pg, normal.with(orchard_grid::Flags::UNDERLINE), &mut x, &mut col);
            }
        }
        if focused && let Some(cc) = cursor_col.filter(|&cc| cc < r.right()) {
            f.cursor = Some(orchard_grid::Cursor { col: cc, row: r.y, shape: CursorShape::Beam, color: c.foreground, text: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::MemoryClipboard;
    use crate::input::Mods;

    fn key(t: &mut TextInput, k: Key, m: Mods, clip: &mut MemoryClipboard) {
        assert!(t.handle_key(&KeyEvent::new(k, m), clip), "{k:?} unused");
    }

    #[test]
    fn typing_editing_selection() {
        let mut clip = MemoryClipboard::default();
        let mut t = TextInput::selected_all("old");
        t.handle_key(&KeyEvent::typed("n"), &mut clip);
        assert_eq!(t.text, "n");
        t.insert("ew name");
        assert_eq!((t.text.as_str(), t.cursor), ("new name", 8));
        key(&mut t, Key::Left, Mods::SHIFT, &mut clip);
        key(&mut t, Key::Left, Mods::SHIFT, &mut clip);
        assert_eq!(t.selected_text(), "me");
        key(&mut t, Key::Char('c'), Mods::primary(), &mut clip);
        assert_eq!(clip.0.as_deref(), Some("me"));
        key(&mut t, Key::Backspace, Mods::NONE, &mut clip);
        assert_eq!(t.text, "new na");
        key(&mut t, Key::Home, Mods::NONE, &mut clip);
        key(&mut t, Key::Delete, Mods::NONE, &mut clip);
        assert_eq!(t.text, "ew na");
        key(&mut t, Key::Char('v'), Mods::primary(), &mut clip);
        assert_eq!(t.text, "meew na");
        key(&mut t, Key::Char('a'), Mods::primary(), &mut clip);
        key(&mut t, Key::Char('x'), Mods::primary(), &mut clip);
        assert_eq!((t.text.as_str(), clip.0.as_deref()), ("", Some("meew na")));
        assert!(!t.handle_key(&KeyEvent::plain(Key::Enter), &mut clip));
    }

    #[test]
    fn graphemes_and_ime() {
        let mut clip = MemoryClipboard::default();
        let mut t = TextInput::new("e\u{301}x");
        key(&mut t, Key::Left, Mods::NONE, &mut clip);
        key(&mut t, Key::Left, Mods::NONE, &mut clip);
        assert_eq!(t.cursor, 0, "combining accent moves as one");
        assert!(t.handle_event(&Event::Preedit("に".into()), &mut clip));
        assert!(t.handle_event(&Event::Commit("日本".into()), &mut clip));
        assert_eq!(t.text, "日本e\u{301}x");
        assert!(t.preedit.is_empty());
    }

    #[test]
    fn draws_with_cursor_and_scroll() {
        let theme = crate::theme::Theme::default_dark();
        let ctx = DrawCtx::new(&theme, (8.0, 16.0));
        let mut f = Frame::new(6, 1, theme.colors.background);
        let t = TextInput::new("abcdefghij");
        t.draw(&mut f, Rect::new(0, 0, 6, 1), &ctx, true);
        assert_eq!(f.row_text(0), "fghij ");
        assert_eq!(f.cursor.map(|c| c.col), Some(5));
        let mut t2 = TextInput::new("abcdefghij");
        t2.click(Rect::new(0, 0, 6, 1), 1);
        assert_eq!(t2.cursor, 6);
    }
}
