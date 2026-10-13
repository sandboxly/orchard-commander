//! Searching the buffer, line by line (a search text never spans lines: the
//! find field is one line). Case-insensitive unless `case` (1.x's default).

use std::ops::Range;

use super::buffer::Buffer;

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Matches of `needle` in `hay` as char ranges, in order, not overlapping.
fn matches_in(hay: &str, needle: &[char], case: bool) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let chars: Vec<char> = if case { hay.chars().collect() } else { hay.chars().map(fold).collect() };
    let n = needle.len();
    let mut i = 0;
    while i + n <= chars.len() {
        if chars[i] == needle[0] && chars[i..i + n] == *needle {
            out.push(i..i + n);
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

fn prepare(needle: &str, case: bool) -> Vec<char> {
    if case { needle.chars().collect() } else { needle.chars().map(fold).collect() }
}

/// The next match after `from` (forward) or the last one ending before it
/// (backward), without wrapping.
pub fn find(buf: &Buffer, needle: &str, from: usize, backward: bool, case: bool) -> Option<Range<usize>> {
    let pat = prepare(needle, case);
    if pat.is_empty() {
        return None;
    }
    let (first, offset) = buf.line_col(from);
    if backward {
        for line in (0..=first).rev() {
            let start = buf.line_start(line);
            let text = buf.line(line).to_string();
            let limit = if line == first { offset } else { usize::MAX };
            if let Some(m) = matches_in(&text, &pat, case).into_iter().rev().find(|m| m.end <= limit) {
                return Some(start + m.start..start + m.end);
            }
        }
    } else {
        for line in first..buf.len_lines() {
            let start = buf.line_start(line);
            let text = buf.line(line).to_string();
            let min = if line == first { offset } else { 0 };
            if let Some(m) = matches_in(&text, &pat, case).into_iter().find(|m| m.start >= min) {
                return Some(start + m.start..start + m.end);
            }
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
    for line in 0..buf.len_lines() {
        let start = buf.line_start(line);
        let text = buf.line(line).to_string();
        out.extend(matches_in(&text, &pat, case).into_iter().map(|m| start + m.start..start + m.end));
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
