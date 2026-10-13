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
    /// Unique in this buffer (never 0, which stands for "no history").
    id: u64,
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

/// The display width of a rope line (its "\n" not counted), walking runs of
/// printable ASCII a chunk at a time.
pub fn line_width(line: RopeSlice<'_>) -> usize {
    let mut col = 0;
    for chunk in line.chunks() {
        let chunk = chunk.strip_suffix('\n').unwrap_or(chunk);
        if chunk.is_ascii() && !chunk.contains('\t') {
            col += chunk.len();
        } else {
            col = chunk.chars().fold(col, advance);
        }
    }
    col
}

/// Every line's [`line_width`] (on a worker for large texts).
pub fn line_widths(rope: &Rope) -> Vec<u32> {
    rope.lines().map(|l| line_width(l).min(u32::MAX as usize - 1) as u32).collect()
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
    /// The id of the newest undo step when the text matched the file (0:
    /// none). Ids are unique, so once that step is dropped from the history
    /// (undone and replaced by new edits) the buffer never counts as clean
    /// again by accident.
    clean: Option<u64>,
    last_id: u64,
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
        self.clean != Some(self.top_id())
    }

    /// The id of the newest undo step (0: none).
    fn top_id(&self) -> u64 {
        self.undo.last().map_or(0, |g| g.id)
    }

    fn next_id(&mut self) -> u64 {
        self.last_id += 1;
        self.last_id
    }

    /// The text now matches the file (saved).
    pub fn mark_clean(&mut self) {
        self.clean = Some(self.top_id());
        self.open = false;
    }

    /// Where the history is now (save started): see [`mark_clean_at`](Self::mark_clean_at).
    pub fn save_point(&self) -> u64 {
        self.top_id()
    }

    /// The text as it was at `point` is on disk (a save finished; the user
    /// may have typed on meanwhile).
    pub fn mark_clean_at(&mut self, point: u64) {
        self.clean = Some(point);
        if self.top_id() == point {
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
        self.scan(line, offset, usize::MAX).1
    }

    /// Walk `line` from its start, char by char, while the offset stays at
    /// most `max_offset` and each char ends at most at column `max_col`;
    /// returns where it stopped as (char offset, display column). Runs of printable ASCII
    /// without tabs are skipped a chunk at a time, so a cursor at the end of
    /// a multi-megabyte minified line costs microseconds, not a char walk.
    pub fn scan(&self, line: usize, max_offset: usize, max_col: usize) -> (usize, usize) {
        let (mut off, mut col) = (0usize, 0usize);
        for chunk in self.line(line).chunks() {
            if off >= max_offset || col >= max_col {
                break;
            }
            if chunk.is_ascii() && !chunk.contains('\t') {
                let n = chunk.len().min(max_offset - off).min(max_col - col);
                off += n;
                col += n;
                if n < chunk.len() {
                    break;
                }
                continue;
            }
            for c in chunk.chars() {
                let next = advance(col, c);
                if off >= max_offset || next > max_col {
                    return (off, col);
                }
                off += 1;
                col = next;
            }
        }
        (off, col)
    }

    /// The position in `line` at display column `col` (the char containing
    /// it, or the line's end). With `round`, a click on the right half of a
    /// char goes after it.
    pub fn pos_at_col(&self, line: usize, col: usize, round: bool) -> usize {
        let start = self.line_start(line);
        let (off, x) = self.scan(line, usize::MAX, col);
        let line_text = self.line(line);
        if off >= line_text.len_chars() {
            return start + off;
        }
        let next = advance(x, line_text.char(off));
        let after = round && col >= x && col - x >= (next - x).div_ceil(2);
        start + off + usize::from(after)
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
            && self.clean != Some(self.top_id())
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
            let id = self.next_id();
            self.push(Group { id, edits: vec![edit], kind, before, after });
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
            let id = self.next_id();
            self.push(Group { id, edits: vec![edit], kind: GroupKind::Other, before, after: (self.cursor, None) });
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
            && self.clean != Some(self.top_id())
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
            let id = self.next_id();
            self.push(Group { id, edits: vec![edit], kind, before, after: (self.cursor, None) });
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
    /// the cursor ends after the last replacement. The new text is built in
    /// one pass and swapped in as a single edit (from the first match to the
    /// last), so replacing 100 000 matches costs about as much as one paste
    /// — and so do undo and redo.
    pub fn replace_ranges(&mut self, ranges: &[Range<usize>], with: &str) {
        let (Some(first), Some(last)) = (ranges.first(), ranges.last()) else {
            return;
        };
        let with = normalize(with);
        let before = (self.cursor, self.anchor);
        let span = first.start..last.end;
        let mut text = String::with_capacity(self.rope.slice(span.clone()).len_bytes());
        let mut pos = span.start;
        for r in ranges {
            for chunk in self.rope.slice(pos..r.start).chunks() {
                text.push_str(chunk);
            }
            text.push_str(&with);
            pos = r.end;
        }
        let edit = self.splice(span.clone(), &text);
        self.cursor = span.start + edit.inserted.chars().count();
        self.anchor = None;
        self.goal = None;
        let id = self.next_id();
        self.push(Group { id, edits: vec![edit], kind: GroupKind::Other, before, after: (self.cursor, None) });
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

    #[test]
    fn undoing_while_a_save_runs_doesnt_fake_a_clean_state() {
        let mut b = Buffer::new("");
        for c in ["a", " ", "b"] {
            b.insert(c, false);
        }
        b.seal();
        let point = b.save_point(); // "a b" goes to disk…
        b.undo(); // …meanwhile: "a "
        b.insert("c", false); // "a c": as many steps as when the save started
        b.mark_clean_at(point); // the save finishes
        assert!(b.modified(), "\"a c\" isn't what was saved");
        b.undo();
        assert!(b.modified());
    }

    #[test]
    fn columns_on_long_and_mixed_lines() {
        let long = "x".repeat(300_000);
        let b = Buffer::new(&format!("{long}\tz日本\n"));
        let end = b.line_end(0);
        assert_eq!(b.display_col(end), 300_000 + 4 + 1 + 4);
        assert_eq!(b.pos_at_col(0, 299_999, false), 299_999);
        assert_eq!(b.pos_at_col(0, 300_002, false), 300_000, "inside the tab");
        assert_eq!(b.pos_at_col(0, 300_005, false), 300_002, "the left half of 日");
        assert_eq!(b.pos_at_col(0, 300_006, true), 300_003, "its right half rounds up");
        assert_eq!(b.pos_at_col(0, 999_999, false), end);
        assert_eq!(b.scan(0, usize::MAX, 300_002), (300_000, 300_000), "stops before the tab");
    }

    #[test]
    fn only_newline_breaks_lines() {
        // 1.x (QPlainTextEdit): form feed, VT, NEL and U+2028 stay inside the line.
        let b = Buffer::new("a\x0cb\u{2028}c\u{85}d\x0be\nf\n");
        assert_eq!(b.len_lines(), 3);
        assert_eq!(b.line(0).to_string(), "a\x0cb\u{2028}c\u{85}d\x0be");
        assert_eq!(b.line_end(0), 9);
        assert_eq!(b.line_of(b.line_start(1)), 1);
    }

    #[test]
    fn line_widths_are_display_columns() {
        let long = "x".repeat(300_000);
        let b = Buffer::new(&format!("{long}\tz日本\n\tab\n\n"));
        let widths = line_widths(b.rope());
        assert_eq!(widths, [300_000 + 4 + 1 + 4, 6, 0, 0]);
        for (line, w) in widths.iter().enumerate() {
            assert_eq!(*w as usize, b.display_col(b.line_end(line)));
        }
    }
}
