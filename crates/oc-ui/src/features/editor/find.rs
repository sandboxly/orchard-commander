//! Searching the buffer, line by line (a search text never spans lines: the
//! find field is one line). Case-insensitive unless `case` (1.x's default).

use std::ops::Range;

use super::buffer::Buffer;

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Matches of `needle` (already folded unless `case`; never a line break)
/// in `line` as char
/// ranges, in order, not overlapping. Uses the standard library's substring
/// search on the line's text (ASCII lines, the usual case, need no char
/// bookkeeping at all), so a miss in a multi-megabyte file stays quick.
fn matches_in(line: ropey::RopeSlice<'_>, needle: &str, case: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() || (case && line.len_bytes() < needle.len()) {
        return out;
    }
    let text: std::borrow::Cow<'_, str> = match line.as_str() {
        Some(s) => s.into(),
        None => line.to_string().into(),
    };
    let ascii = text.is_ascii();
    let hay: std::borrow::Cow<'_, str> = if case {
        text
    } else if ascii {
        if !needle.is_ascii() {
            return out; // a folded non-ASCII needle can't match ASCII text
        }
        text.to_ascii_lowercase().into()
    } else {
        text.chars().map(fold).collect::<String>().into()
    };
    let n = needle.chars().count();
    // Byte offsets to char offsets (folding keeps one char per char).
    let (mut last_byte, mut last_char) = (0usize, 0usize);
    for (b, _) in hay.match_indices(needle) {
        let c = if ascii { b } else { last_char + hay[last_byte..b].chars().count() };
        (last_byte, last_char) = (b, c);
        out.push(c..c + n);
    }
    out
}

fn prepare(needle: &str, case: bool) -> String {
    if case { needle.to_owned() } else { needle.chars().map(fold).collect() }
}

/// The next match after `from` (forward) or the last one ending before it
/// (backward), without wrapping. Walks the lines with the rope's line
/// iterator (no per-line index lookups).
pub fn find(buf: &Buffer, needle: &str, from: usize, backward: bool, case: bool) -> Option<Range<usize>> {
    let pat = prepare(needle, case);
    if pat.is_empty() {
        return None;
    }
    let rope = buf.rope();
    let (first, offset) = buf.line_col(from);
    if backward {
        let mut lines = rope.lines_at(first + 1);
        let mut end = buf.line_start(first + 1);
        let mut line = first + 1;
        while let Some(text) = lines.prev() {
            line -= 1;
            let start = end - text.len_chars();
            let limit = if line == first { offset } else { usize::MAX };
            if let Some(m) = matches_in(text, &pat, case).into_iter().rev().find(|m| m.end <= limit) {
                return Some(start + m.start..start + m.end);
            }
            end = start;
        }
    } else {
        let mut start = buf.line_start(first);
        for (i, text) in rope.lines_at(first).enumerate() {
            let min = if i == 0 { offset } else { 0 };
            if let Some(m) = matches_in(text, &pat, case).into_iter().find(|m| m.start >= min) {
                return Some(start + m.start..start + m.end);
            }
            start += text.len_chars();
        }
    }
    None
}

/// Every match in the document.
pub fn find_all(buf: &Buffer, needle: &str, case: bool) -> Vec<Range<usize>> {
    let pat = prepare(needle, case);
    let mut out = Vec::new();
    if pat.is_empty() {
        return out;
    }
    let mut start = 0;
    for text in buf.rope().lines() {
        out.extend(matches_in(text, &pat, case).into_iter().map(|m| start + m.start..start + m.end));
        start += text.len_chars();
    }
    out
}

/// Whether `text` is `needle` (the selection is the current match).
pub fn same(text: &str, needle: &str, case: bool) -> bool {
    if case { text == needle } else { text.chars().map(fold).eq(needle.chars().map(fold)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_forward_backward_and_all() {
        let b = Buffer::new("cat dog cat\nCat\n");
        assert_eq!(find(&b, "cat", 0, false, false), Some(0..3));
        assert_eq!(find(&b, "cat", 1, false, false), Some(8..11));
        assert_eq!(find(&b, "cat", 9, false, false), Some(12..15));
        assert_eq!(find(&b, "cat", 9, false, true), None);
        assert_eq!(find(&b, "cat", 12, true, false), Some(8..11));
        assert_eq!(find(&b, "cat", 8, true, false), Some(0..3));
        assert_eq!(find_all(&b, "CAT", false).len(), 3);
        assert_eq!(find_all(&b, "cat", true).len(), 2);
        assert!(same("Cat", "cAT", false) && !same("Cat", "cAT", true));
        let u = Buffer::new("Straße STRASSE");
        assert_eq!(find(&u, "straße", 0, false, false), Some(0..6));
    }
}
