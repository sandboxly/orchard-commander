//! Mouse selection and copying for the text-like previews (1.x showed them
//! in a `QTextBrowser`: drag to select, double-click a word, Shift+click to
//! extend, ⌘C to copy, right-click for Copy / Copy Link Location / Select All).
//!
//! Positions are (row, cell column) in the view's rows, tabs expanded; a
//! grapheme is selected when the cell it starts in is.

use std::borrow::Cow;

use oc_grid::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::text::TAB;
use crate::app::App;
use crate::dialogs::PopupMenu;
use crate::geom::Rect;
use crate::input::{KeyEvent, MouseButton, MouseEvent, MouseKind};
use crate::shortcut::Shortcut;
use crate::widgets::{DrawCtx, MenuEntry};

/// (row, cell column).
pub type Pos = (usize, usize);

#[derive(Debug, Default, Clone)]
pub struct Selection {
    anchor: Option<Pos>,
    head: Pos,
    /// The left button went down here and hasn't come up yet.
    dragging: bool,
}

/// A row as copying sees it.
pub struct RowText<'a> {
    pub text: Cow<'a, str>,
    /// Cells at the start that aren't text (indentation, list markers, quote bars).
    pub lead: usize,
    /// What joins this row to the one before: "\n", or " " / "" where a
    /// paragraph was wrapped.
    pub joiner: &'static str,
}

impl Selection {
    /// The selected range, start before end; None when nothing is selected.
    pub fn range(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        let (s, e) = if a <= self.head { (a, self.head) } else { (self.head, a) };
        (s != e).then_some((s, e))
    }

    pub fn is_empty(&self) -> bool {
        self.range().is_none()
    }

    pub fn clear(&mut self) {
        *self = Selection::default();
    }

    pub fn select_all(&mut self, rows: usize) {
        *self = Selection { anchor: Some((0, 0)), head: (rows, 0), dragging: false };
    }

    /// Selected cells of `row` (a row of `width` cells): from..to, `to` one
    /// past the text when the line break is selected too.
    pub fn cells_in_row(&self, row: usize, width: usize) -> Option<(usize, usize)> {
        let (s, e) = self.range()?;
        if row < s.0 || row > e.0 {
            return None;
        }
        let from = if row == s.0 { s.1 } else { 0 };
        let to = if row == e.0 { e.1 } else { width.max(from) + 1 };
        (from < to).then_some((from, to))
    }

    /// Left-button handling: press, drag, release, double-click (a word),
    /// Shift+press (extend). `pos` is where the event is in the rows;
    /// `word` gives the cells of the word at a position. True if the
    /// selection changed.
    pub fn mouse(&mut self, m: &MouseEvent, pos: Pos, word: impl Fn(Pos) -> Option<(usize, usize)>) -> bool {
        let before = self.range();
        match m.kind {
            MouseKind::Down(MouseButton::Left) if m.mods.shift && self.anchor.is_some() => {
                self.head = pos;
                self.dragging = true;
            }
            MouseKind::Down(MouseButton::Left) => {
                *self = Selection { anchor: Some(pos), head: pos, dragging: true };
            }
            MouseKind::DoubleClick(MouseButton::Left) => {
                if let Some((from, to)) = word(pos) {
                    *self = Selection { anchor: Some((pos.0, from)), head: (pos.0, to), dragging: false };
                }
            }
            MouseKind::Move if self.dragging => self.head = pos,
            MouseKind::Up(MouseButton::Left) => self.dragging = false,
            _ => {}
        }
        self.range() != before
    }

    /// The selected text; rows come from `row` (None past the end).
    pub fn text<'a>(&self, row: impl Fn(usize) -> Option<RowText<'a>>) -> String {
        let Some((s, e)) = self.range() else { return String::new() };
        let mut out = String::new();
        for r in s.0..=e.0 {
            let Some(rt) = row(r) else { break };
            if r > s.0 {
                out.push_str(rt.joiner);
            }
            let from = if r == s.0 { s.1 } else { 0 }.max(rt.lead);
            let to = if r == e.0 { e.1 } else { usize::MAX };
            out.push_str(&slice(&rt.text, from, to));
        }
        out
    }
}

/// Display width of a grapheme at column `col`.
fn cell_width(g: &str, col: usize) -> usize {
    if g == "\t" { TAB - col % TAB } else { g.width() }
}

/// The graphemes of `text` starting in cells from..to.
pub fn slice(text: &str, from: usize, to: usize) -> String {
    let mut col = 0;
    let mut out = String::new();
    for g in text.graphemes(true) {
        if col >= to {
            break;
        }
        if col >= from {
            out.push_str(g);
        }
        col += cell_width(g, col);
    }
    out
}

/// The cells of the word (or the run of spaces / the symbol) at `col`.
pub fn word_at(text: &str, col: usize) -> Option<(usize, usize)> {
    let mut x = 0;
    for w in text.split_word_bounds() {
        let start = x;
        for g in w.graphemes(true) {
            x += cell_width(g, x);
        }
        if col < x {
            return Some((start, x));
        }
    }
    None
}

/// Paint the selected cells of a row drawn at (`x`, `y`) with its first
/// `skip` columns scrolled away, up to `max_x`.
pub fn paint(f: &mut Frame, ctx: &DrawCtx, cells: (usize, usize), x: u16, y: u16, skip: usize, max_x: u16) {
    let c = &ctx.theme.colors;
    let (from, to) = (cells.0.max(skip), cells.1);
    for col in from..to {
        let sx = x as usize + (col - skip);
        if sx >= max_x as usize {
            break;
        }
        if let Some(cell) = f.cell_mut(sx as u16, y) {
            cell.fg = c.cursor_fg;
            cell.bg = c.cursor_bg;
        }
    }
}

/// The row and column under a mouse event in `area`, the rows scrolled to
/// `top` and the columns to `left`; outside the area it is clamped (dragging
/// past an edge).
pub fn pos_at(m: &MouseEvent, area: Rect, top: usize, left: usize) -> Pos {
    let row = (m.row as i32 - area.y as i32).clamp(0, area.h.saturating_sub(1) as i32) as usize;
    let col = (m.col as i32 - area.x as i32).max(0) as usize;
    (top + row, left + col)
}

/// ⌘C (Ctrl+C elsewhere).
pub fn is_copy(k: &KeyEvent) -> bool {
    Shortcut::parse("Primary+C").is_some_and(|s| s.matches(k))
}

/// Put `text` on the clipboard (nothing selected: nothing happens, like Qt).
pub fn copy(app: &mut App, text: &str) {
    if !text.is_empty() {
        app.clipboard.set_text(text);
    }
}

/// What was chosen in the context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuChoice {
    Copy,
    CopyLink,
    SelectAll,
}

/// Qt's standard context menu of a read-only text browser: Copy (enabled
/// with a selection), Copy Link Location (enabled on a link), Select All.
pub fn context_menu(
    app: &mut App,
    m: &MouseEvent,
    has_selection: bool,
    on_link: bool,
    chosen: impl FnOnce(&mut App, MenuChoice) + 'static,
) {
    let key = |spec: &str| Shortcut::parse(spec).map(|s| s.display()).unwrap_or_default();
    let item = |label: &str, shortcut: String, enabled: bool| MenuEntry::Item {
        label: label.into(),
        shortcut,
        checked: None,
        enabled,
    };
    let entries = vec![
        item("&Copy", key("Primary+C"), has_selection),
        item("Copy &link location", String::new(), on_link),
        MenuEntry::Separator,
        item("Select all", key("Primary+A"), true),
    ];
    app.show(PopupMenu::new(entries, m.col, m.row, move |app, i| {
        let choice = match i {
            0 => MenuChoice::Copy,
            1 => MenuChoice::CopyLink,
            _ => MenuChoice::SelectAll,
        };
        chosen(app, choice);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    fn ev(kind: MouseKind) -> MouseEvent {
        MouseEvent { kind, col: 0, row: 0, mods: Mods::NONE }
    }

    #[test]
    fn drag_selects_across_rows() {
        let rows = ["alpha beta", "\tgamma", "delta"];
        let get = |r: usize| rows.get(r).map(|t| RowText { text: (*t).into(), lead: 0, joiner: "\n" });
        let mut s = Selection::default();
        let no_word = |_| None;
        s.mouse(&ev(MouseKind::Down(MouseButton::Left)), (0, 6), no_word);
        assert!(s.is_empty());
        assert!(s.mouse(&ev(MouseKind::Move), (1, 6), no_word));
        s.mouse(&ev(MouseKind::Up(MouseButton::Left)), (1, 6), no_word);
        assert_eq!(s.text(get), "beta\n\tga");
        assert!(!s.mouse(&ev(MouseKind::Move), (2, 2), no_word), "moving without the button changes nothing");
        assert_eq!(s.cells_in_row(0, 10), Some((6, 11)));
        assert_eq!(s.cells_in_row(1, 9), Some((0, 6)));
        s.select_all(rows.len());
        assert_eq!(s.text(get), "alpha beta\n\tgamma\ndelta");
    }

    #[test]
    fn words_and_slices() {
        assert_eq!(word_at("alpha beta", 7), Some((6, 10)));
        assert_eq!(word_at("alpha beta", 5), Some((5, 6)));
        assert_eq!(word_at("ab", 5), None);
        assert_eq!(slice("a\tb", 1, 5), "\tb");
        assert_eq!(slice("日本", 2, 4), "本");
    }
}
