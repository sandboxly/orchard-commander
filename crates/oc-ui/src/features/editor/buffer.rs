//! The editor's text: a rope (ropey) with a cursor, a selection and an undo
//! history. Edits are O(log n) in the document size, so multi-megabyte files
//! type as fast as small ones. Positions are char indices into the rope;
//! line endings inside the rope are always "\n" (the file's own ending is
//! restored on save, see `oc_preview::textfile`).
//!
//! Every edit is logged as a [`Change`] (which lines it touched) for the
//! incremental highlighter.

use std::ops::Range;

use ropey::{Rope, RopeSlice};
use unicode_width::UnicodeWidthChar;

/// Tab stops every this many columns (1.x: four spaces).
pub const TAB_WIDTH: usize = 4;

/// What an edit did to the lines: `[line, old_end]` became `[line, new_end]`
/// (inclusive line numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Lines {
        line: usize,
        old_end: usize,
        new_end: usize,
    },
    /// The whole text was replaced (loaded, reloaded).
    Reset,
}

#[derive(Clone, Debug)]
struct Edit {
    at: usize,
    removed: String,
    inserted: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupKind {
    Typing,
    Backspace,
    Delete,
    Other,
}

/// One undo step.
#[derive(Clone, Debug)]
struct Group {
    edits: Vec<Edit>,
    kind: GroupKind,
    before: (usize, Option<usize>),
    after: (usize, Option<usize>),
}

/// Kinds of text for word movement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Display width of a char in the editor (tabs are handled by the caller).
pub fn char_width(c: char) -> usize {
    if c.is_control() { 1 } else { c.width().unwrap_or(1) }
}

/// The column after `c` drawn at column `col`.
pub fn advance(col: usize, c: char) -> usize {
    if c == '\t' { (col / TAB_WIDTH + 1) * TAB_WIDTH } else { col + char_width(c) }
}

#[derive(Default)]
pub struct Buffer {
    rope: Rope,
    /// Char index of the cursor.
    pub cursor: usize,
    /// The other end of the selection.
    pub anchor: Option<usize>,
    /// The display column up/down keep (where the cursor was before a short line).
    goal: Option<usize>,
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// `undo.len()` when the text matched the file; None if that state is gone.
    clean: Option<usize>,
    /// The last undo group may still take more typing.
    open: bool,
    /// Bumped on every change of the text.
    pub version: u64,
    changes: Vec<Change>,
}

impl Buffer {
    pub fn new(text: &str) -> Self {
        Buffer { rope: Rope::from_str(text), clean: Some(0), ..Default::default() }
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Replace everything (loading): no undo history, unmodified. The cursor
    /// stays where it was if it still fits.
    pub fn set_text(&mut self, text: &str) {
        self.set_rope(Rope::from_str(text));
    }

    /// [`set_text`](Self::set_text) with a rope built elsewhere (on a worker).
    pub fn set_rope(&mut self, rope: Rope) {
        self.rope = rope;
        self.cursor = self.cursor.min(self.rope.len_chars());
        self.anchor = None;
        self.goal = None;
        self.undo.clear();
        self.redo.clear();
        self.clean = Some(0);
        self.open = false;
        self.version += 1;
        self.changes.push(Change::Reset);
    }

    /// The changes since the last call (for the highlighter).
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    // -- modification state ---------------------------------------------------

    pub fn modified(&self) -> bool {
        self.clean != Some(self.undo.len())
    }

    /// The text now matches the file (saved).
    pub fn mark_clean(&mut self) {
        self.clean = Some(self.undo.len());
        self.open = false;
    }

    /// Where the history is now (save started): see [`mark_clean_at`](Self::mark_clean_at).
    pub fn save_point(&self) -> usize {
        self.undo.len()
    }

    /// The text as it was at `point` is on disk (a save finished; the user
    /// may have typed on meanwhile).
    pub fn mark_clean_at(&mut self, point: usize) {
        self.clean = Some(point);
        if self.undo.len() == point {
            self.open = false;
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Stop merging typing into the last undo step (the cursor jumped).
    pub fn seal(&mut self) {
        self.open = false;
    }

    // -- lines and columns --------------------------------------------------------

    pub fn line_of(&self, pos: usize) -> usize {
        self.rope.char_to_line(pos.min(self.rope.len_chars()))
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line.min(self.rope.len_lines()))
    }

    /// A line without its "\n".
    pub fn line(&self, line: usize) -> RopeSlice<'_> {
        let l = self.rope.line(line);
        let n = l.len_chars();
        if n > 0 && l.char(n - 1) == '\n' { l.slice(..n - 1) } else { l }
    }

    /// The char index just before the line's "\n" (or the end of the text).
    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line(line).len_chars()
    }

    /// The cursor's line and its char offset in it (0-based).
    pub fn line_col(&self, pos: usize) -> (usize, usize) {
        let line = self.line_of(pos);
        (line, pos - self.line_start(line))
    }

    /// The display column of `pos` (tabs expanded, wide chars count two).
    pub fn display_col(&self, pos: usize) -> usize {
        let (line, offset) = self.line_col(pos);
        self.line(line).chars().take(offset).fold(0, advance)
    }

    /// The position in `line` at display column `col` (the char containing
    /// it, or the line's end). With `round`, a click on the right half of a
    /// char goes after it.
    pub fn pos_at_col(&self, line: usize, col: usize, round: bool) -> usize {
        let start = self.line_start(line);
        let mut x = 0;
        for (i, c) in self.line(line).chars().enumerate() {
            let next = advance(x, c);
            if next > col {
                let after = round && col - x >= (next - x).div_ceil(2);
                return start + i + usize::from(after);
            }
            x = next;
        }
        self.line_end(line)
    }

    // -- selection --------------------------------------------------------------

    pub fn selection(&self) -> Option<Range<usize>> {
        let a = self.anchor?;
        (a != self.cursor).then(|| a.min(self.cursor)..a.max(self.cursor))
    }

    pub fn selected_text(&self) -> String {
        self.selection().map(|r| self.rope.slice(r).to_string()).unwrap_or_default()
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.rope.len_chars();
        self.goal = None;
        self.open = false;
    }

    pub fn select(&mut self, range: Range<usize>) {
        let n = self.rope.len_chars();
        self.anchor = Some(range.start.min(n));
        self.cursor = range.end.min(n);
        self.goal = None;
        self.open = false;
    }

    /// Move the cursor; `extend` keeps (or starts) the selection.
    pub fn move_to(&mut self, pos: usize, extend: bool) {
        let pos = pos.min(self.rope.len_chars());
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
        self.goal = None;
        self.open = false;
    }

    // -- movement ---------------------------------------------------------------

    fn char_at(&self, pos: usize) -> Option<char> {
        (pos < self.rope.len_chars()).then(|| self.rope.char(pos))
    }

    /// One char left, skipping combining marks (they belong to the char before).
    pub fn prev_pos(&self, pos: usize) -> usize {
        let mut p = pos.saturating_sub(1);
        while p > 0 && self.char_at(p).is_some_and(|c| c.width() == Some(0) && c != '\n' && !c.is_control()) {
            p -= 1;
        }
        p
    }

    pub fn next_pos(&self, pos: usize) -> usize {
        let n = self.rope.len_chars();
        let mut p = (pos + 1).min(n);
        while p < n && self.char_at(p).is_some_and(|c| c.width() == Some(0) && c != '\n' && !c.is_control()) {
            p += 1;
        }
        p
    }

    /// The start of the word before `pos` (Alt+Left / Ctrl+Left).
    pub fn prev_word(&self, pos: usize) -> usize {
        let mut p = pos;
        while p > 0 && self.char_at(p - 1).is_some_and(|c| class(c) == Class::Space) {
            p -= 1;
        }
        if let Some(k) = p.checked_sub(1).and_then(|i| self.char_at(i)).map(class) {
            while p > 0 && self.char_at(p - 1).is_some_and(|c| class(c) == k) {
                p -= 1;
            }
        }
        p
    }

    /// The end of the word after `pos`.
    pub fn next_word(&self, pos: usize) -> usize {
        let n = self.rope.len_chars();
        let mut p = pos;
        while p < n && self.char_at(p).is_some_and(|c| class(c) == Class::Space) {
            p += 1;
        }
        if let Some(k) = self.char_at(p).map(class) {
            while p < n && self.char_at(p).is_some_and(|c| class(c) == k) {
                p += 1;
            }
        }
        p
    }

    /// The word (or run of other chars) around `pos` (double click).
    pub fn word_at(&self, pos: usize) -> Range<usize> {
        let n = self.rope.len_chars();
        let Some(c) = self.char_at(pos).filter(|&c| c != '\n').or_else(|| pos.checked_sub(1).and_then(|p| self.char_at(p))) else {
            return pos..pos;
        };
        if c == '\n' {
            return pos..pos;
        }
        let k = class(c);
        let mut a = pos.min(n);
        while a > 0 && self.char_at(a - 1).is_some_and(|c| c != '\n' && class(c) == k) {
            a -= 1;
        }
        let mut b = a;
        while b < n && self.char_at(b).is_some_and(|c| c != '\n' && class(c) == k) {
            b += 1;
        }
        a..b
    }

    /// Up/down by `lines` (negative: up), keeping the display column.
    pub fn move_lines(&mut self, lines: isize, extend: bool) {
        let goal = self.goal.unwrap_or_else(|| self.display_col(self.cursor));
        let line = self.line_of(self.cursor) as isize;
        let last = self.len_lines() as isize - 1;
        let target = line + lines;
        let pos = if target < 0 {
            0
        } else if target > last {
            self.len_chars()
        } else {
            self.pos_at_col(target as usize, goal, false)
        };
        self.move_to(pos, extend);
        self.goal = Some(goal);
    }

    // -- editing ---------------------------------------------------------------

    /// Replace `range` with `text` without touching the history.
    fn splice(&mut self, range: Range<usize>, text: &str) -> Edit {
        let line = self.rope.char_to_line(range.start);
        let old_end = self.rope.char_to_line(range.end);
        let removed = self.rope.slice(range.clone()).to_string();
        if !range.is_empty() {
            self.rope.remove(range.clone());
        }
        if !text.is_empty() {
            self.rope.insert(range.start, text);
        }
        let new_end = self.rope.char_to_line(range.start + text.chars().count());
        self.version += 1;
        self.changes.push(Change::Lines { line, old_end, new_end });
        Edit { at: range.start, removed, inserted: text.to_owned() }
    }

    fn push(&mut self, group: Group) {
        if self.clean.is_some_and(|c| c > self.undo.len()) {
            self.clean = None; // the saved state was in the redo history
        }
        self.undo.push(group);
        self.redo.clear();
    }

    /// Replace the selection (or insert at the cursor) with `text`.
    /// `typing` merges consecutive typed characters into one undo step.
    pub fn insert(&mut self, text: &str, typing: bool) {
        let text = normalize(text);
        let before = (self.cursor, self.anchor);
        let sel = self.selection();
        let range = sel.clone().unwrap_or(self.cursor..self.cursor);
        let at = range.start;
        let n = text.chars().count();
        let merge = typing
            && sel.is_none()
            && self.open
            && !text.contains('\n')
            && self.clean != Some(self.undo.len())
            && self.undo.last().is_some_and(|g| {
                g.kind == GroupKind::Typing
                    && g.edits.len() == 1
                    && g.edits[0].removed.is_empty()
                    && g.edits[0].at + g.edits[0].inserted.chars().count() == at
            });
        let edit = self.splice(range, &text);
        self.cursor = at + n;
        self.anchor = None;
        self.goal = None;
        let after = (self.cursor, None);
        if merge {
            let g = self.undo.last_mut().expect("checked");
            g.edits[0].inserted.push_str(&edit.inserted);
            g.after = after;
            self.redo.clear();
        } else {
            let kind = if typing && sel.is_none() { GroupKind::Typing } else { GroupKind::Other };
            self.push(Group { edits: vec![edit], kind, before, after });
        }
        self.open = typing && !text.contains('\n');
    }

    /// Delete the selection, or `range` if there is none. Single-char
    /// deletions in a row (`mergeable`) are one undo step.
    fn delete(&mut self, range: Range<usize>, backward: bool, mergeable: bool) {
        if let Some(sel) = self.selection() {
            let before = (self.cursor, self.anchor);
            let edit = self.splice(sel.clone(), "");
            self.cursor = sel.start;
            self.anchor = None;
            self.goal = None;
            self.push(Group { edits: vec![edit], kind: GroupKind::Other, before, after: (self.cursor, None) });
            self.open = false;
            return;
        }
        self.anchor = None;
        if range.is_empty() {
            return;
        }
        let kind = if backward { GroupKind::Backspace } else { GroupKind::Delete };
        let before = (self.cursor, None);
        let merge = mergeable
            && self.open
            && self.clean != Some(self.undo.len())
            && self.undo.last().is_some_and(|g| {
                g.kind == kind
                    && g.edits.len() == 1
                    && g.edits[0].inserted.is_empty()
                    && if backward { g.edits[0].at == range.end } else { g.edits[0].at == range.start }
            });
        let edit = self.splice(range.clone(), "");
        self.cursor = range.start;
        self.goal = None;
        if merge {
            let g = self.undo.last_mut().expect("checked");
            if backward {
                g.edits[0].removed.insert_str(0, &edit.removed);
                g.edits[0].at = edit.at;
            } else {
                g.edits[0].removed.push_str(&edit.removed);
            }
            g.after = (self.cursor, None);
            self.redo.clear();
        } else {
            self.push(Group { edits: vec![edit], kind, before, after: (self.cursor, None) });
        }
        self.open = mergeable;
    }

    /// Backspace: the selection or the char (word with `word`) before the cursor.
    pub fn backspace(&mut self, word: bool) {
        let start = if word { self.prev_word(self.cursor) } else { self.prev_pos(self.cursor) };
        self.delete(start..self.cursor, true, !word);
    }

    /// ⌘⌫: everything from the start of the line to the cursor.
    pub fn backspace_line(&mut self) {
        let line = self.line_of(self.cursor);
        let start = self.line_start(line);
        let start = if start == self.cursor { self.prev_pos(self.cursor) } else { start };
        self.delete(start..self.cursor, true, false);
    }

    /// Delete: the selection or the char (word) after the cursor.
    pub fn delete_forward(&mut self, word: bool) {
        let end = if word { self.next_word(self.cursor) } else { self.next_pos(self.cursor) };
        self.delete(self.cursor..end, false, !word);
    }

    /// Replace several ranges (sorted, not overlapping) as one undo step;
    /// the cursor ends after the last replacement.
    pub fn replace_ranges(&mut self, ranges: &[Range<usize>], with: &str) {
        if ranges.is_empty() {
            return;
        }
        let with = normalize(with);
        let before = (self.cursor, self.anchor);
        let mut edits = Vec::with_capacity(ranges.len());
        // From the end, so earlier positions stay put; recorded front to back.
        for r in ranges.iter().rev() {
            edits.push(self.splice(r.clone(), &with));
        }
        edits.reverse();
        // Positions as they are after the earlier replacements (for undo/redo).
        let delta = with.chars().count() as isize;
        let mut shift = 0isize;
        for e in &mut edits {
            e.at = (e.at as isize + shift) as usize;
            shift += delta - e.removed.chars().count() as isize;
        }
        let last = edits.last().expect("non-empty");
        self.cursor = last.at + last.inserted.chars().count();
        self.anchor = None;
        self.goal = None;
        self.push(Group { edits, kind: GroupKind::Other, before, after: (self.cursor, None) });
        self.open = false;
    }

    pub fn undo(&mut self) -> bool {
        let Some(g) = self.undo.pop() else {
            return false;
        };
        for e in g.edits.iter().rev() {
            let n = e.inserted.chars().count();
            self.splice(e.at..e.at + n, &e.removed);
        }
        (self.cursor, self.anchor) = g.before;
        self.goal = None;
        self.redo.push(g);
        self.open = false;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(g) = self.redo.pop() else {
            return false;
        };
        for e in &g.edits {
            let n = e.removed.chars().count();
            self.splice(e.at..e.at + n, &e.inserted);
        }
        (self.cursor, self.anchor) = g.after;
        self.goal = None;
        self.undo.push(g);
        self.open = false;
        true
    }

    /// The indentation of the cursor's line (Enter keeps it).
    pub fn indent_of_line(&self, line: usize) -> String {
        self.line(line).chars().take_while(|c| *c == ' ' || *c == '\t').collect()
    }
}

/// Line endings as the rope keeps them: "\n" only.
fn normalize(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n").into() } else { text.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_merges_into_one_undo_step() {
        let mut b = Buffer::new("x\n");
        for c in ["a", "b", "c"] {
            b.insert(c, true);
        }
        assert_eq!(b.text(), "abcx\n");
        assert!(b.modified());
        b.insert("\n", true);
        b.insert("d", true);
        assert!(b.undo());
        assert_eq!(b.text(), "abc\nx\n");
        assert!(b.undo());
        assert_eq!(b.text(), "abcx\n");
        assert!(b.undo());
        assert_eq!((b.text().as_str(), b.modified()), ("x\n", false), "back to the file's text: unmodified");
        assert!(b.redo() && b.redo());
        assert_eq!(b.text(), "abc\nx\n");
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn saving_seals_and_clean_state_tracks_undo() {
        let mut b = Buffer::new("");
        b.insert("a", true);
        b.mark_clean();
        b.insert("b", true);
        assert!(b.modified());
        b.undo();
        assert!(!b.modified() && b.text() == "a");
        b.undo();
        assert!(b.modified());
        b.insert("z", true); // the saved state is gone now
        b.undo();
        b.redo();
        assert!(b.modified());
    }

    #[test]
    fn deleting_and_words() {
        let mut b = Buffer::new("hello wide world");
        b.move_to(10, false);
        b.backspace(true);
        assert_eq!(b.text(), "hello  world");
        b.backspace(false);
        b.backspace(false);
        assert_eq!(b.text(), "hell world");
        b.undo();
        assert_eq!(b.text(), "hello  world", "backspaces merge");
        b.move_to(0, false);
        b.delete_forward(true);
        assert_eq!(b.text(), "  world");
        assert_eq!(b.word_at(4), 2..7);
        assert_eq!(b.next_word(0), 7);
        assert_eq!(b.prev_word(7), 2);
    }

    #[test]
    fn selection_replace_and_columns() {
        let mut b = Buffer::new("\tab\n日本x\n");
        assert_eq!(b.display_col(1), 4);
        assert_eq!(b.display_col(b.line_start(1) + 2), 4);
        assert_eq!(b.pos_at_col(1, 3, false), b.line_start(1) + 1);
        assert_eq!(b.pos_at_col(1, 99, false), b.line_end(1));
        b.move_to(1, false);
        b.move_lines(1, true);
        assert_eq!(b.cursor, b.line_start(1) + 2, "column 4 on the next line");
        assert_eq!(b.selected_text(), "ab\n日本");
        b.insert("-", false);
        assert_eq!(b.text(), "\t-x\n");
        b.undo();
        assert_eq!(b.selection(), Some(1..6));
    }

    #[test]
    fn replace_all_is_one_step() {
        let mut b = Buffer::new("cat dog cat\nCat\n");
        b.replace_ranges(&[0..3, 8..11, 12..15], "bird");
        assert_eq!(b.text(), "bird dog bird\nbird\n");
        b.undo();
        assert_eq!(b.text(), "cat dog cat\nCat\n");
        b.redo();
        assert_eq!(b.text(), "bird dog bird\nbird\n");
    }

    #[test]
    fn changes_report_lines() {
        let mut b = Buffer::new("a\nb\nc\n");
        b.take_changes();
        b.move_to(2, false);
        b.insert("x\ny", false);
        assert_eq!(b.take_changes(), [Change::Lines { line: 1, old_end: 1, new_end: 2 }]);
        b.select(0..b.line_start(3));
        b.insert("", false);
        assert_eq!(b.take_changes(), [Change::Lines { line: 0, old_end: 3, new_end: 0 }]);
        b.insert("\r\n", false);
        assert_eq!(b.text(), "\nc\n", "CRLF pasted as LF");
    }
}
