//! Name matching: file-manager masks or a regular expression.
//!
//! Masks are translated the way Python's `fnmatch.translate` does it (so `*`,
//! `?` and `[...]` classes behave exactly like the Python version) and all
//! masks of one side are compiled into a single anchored alternation: one
//! regex call per name instead of one per mask.

use regex::{Regex, RegexBuilder};

use crate::PatternError;

/// Matches entry names against the "Search for" field.
#[derive(Clone, Debug)]
pub struct NameMatcher {
    kind: Kind,
}

#[derive(Clone, Debug)]
enum Kind {
    /// "*", an empty field or an empty regex: no regex call at all.
    All,
    /// `re.search` semantics: the pattern may match anywhere in the name.
    Regex(Regex),
    /// The same with look-around or back-references, which `regex` lacks.
    Fancy(fancy_regex::Regex),
    Masks { include: Option<Regex>, exclude: Option<Regex> },
}

impl NameMatcher {
    /// `pattern` is a regular expression if `regex`, otherwise masks separated
    /// by spaces or ";" with one "|" separating included from excluded masks.
    pub fn new(pattern: &str, regex: bool, case: bool) -> Result<Self, PatternError> {
        if regex {
            if pattern.trim().is_empty() {
                return Ok(NameMatcher { kind: Kind::All });
            }
            let kind = match RegexBuilder::new(pattern).case_insensitive(!case).build() {
                Ok(re) => Kind::Regex(re),
                Err(_) => Kind::Fancy(fancy_regex::RegexBuilder::new(pattern).case_insensitive(!case).build()?),
            };
            return Ok(NameMatcher { kind });
        }
        let (include, exclude) = pattern.split_once('|').unwrap_or((pattern, ""));
        let include: Vec<&str> = split_masks(include).collect();
        // Only "*" masks (or none at all) include everything: skip the regex.
        let include = if include.iter().all(|m| m.bytes().all(|b| b == b'*')) {
            None
        } else {
            Some(compile_masks(&include, case)?)
        };
        let exclude: Vec<&str> = split_masks(exclude).collect();
        let exclude = if exclude.is_empty() { None } else { Some(compile_masks(&exclude, case)?) };
        if include.is_none() && exclude.is_none() {
            return Ok(NameMatcher { kind: Kind::All });
        }
        Ok(NameMatcher { kind: Kind::Masks { include, exclude } })
    }

    pub fn matches(&self, name: &str) -> bool {
        match &self.kind {
            Kind::All => true,
            Kind::Regex(re) => re.is_match(name),
            // Errors only mean the backtrack limit was hit: no match.
            Kind::Fancy(re) => re.is_match(name).unwrap_or(false),
            Kind::Masks { include, exclude } => {
                include.as_ref().is_none_or(|re| re.is_match(name))
                    && !exclude.as_ref().is_some_and(|re| re.is_match(name))
            }
        }
    }

    /// Whether every name matches (lets the walker skip work it only needs for hits).
    pub fn matches_all(&self) -> bool {
        matches!(self.kind, Kind::All)
    }
}

/// Masks are separated by ";" and/or whitespace.
fn split_masks(masks: &str) -> impl Iterator<Item = &str> {
    masks.split(|c: char| c == ';' || c.is_whitespace()).filter(|m| !m.is_empty())
}

fn compile_masks(masks: &[&str], case: bool) -> Result<Regex, regex::Error> {
    let mut pattern = String::from(r"\A(?:");
    for (i, mask) in masks.iter().enumerate() {
        if i > 0 {
            pattern.push('|');
        }
        if mask.contains(['*', '?', '[']) {
            translate(mask, &mut pattern);
        } else {
            // TC: plain text matches anywhere in the name.
            pattern.push_str(".*");
            pattern.push_str(&regex::escape(mask));
            pattern.push_str(".*");
        }
    }
    pattern.push_str(r")\z");
    RegexBuilder::new(&pattern).case_insensitive(!case).dot_matches_new_line(true).build()
}

/// Port of `fnmatch.translate` (without the anchors), appended to `out`.
fn translate(mask: &str, out: &mut String) {
    let chars: Vec<char> = mask.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut buf = [0u8; 4];
    while i < n {
        let c = chars[i];
        i += 1;
        match c {
            '*' => {
                while i < n && chars[i] == '*' {
                    i += 1;
                }
                out.push_str(".*");
            }
            '?' => out.push('.'),
            '[' => {
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    out.push_str(r"\[");
                } else {
                    push_class(&chars[i..j], out);
                    i = j + 1;
                }
            }
            _ => out.push_str(&regex::escape(c.encode_utf8(&mut buf))),
        }
    }
}

/// A `[...]` class body: "!" negates, "a-z" is a range, reversed ranges are
/// dropped (fnmatch removes them), every other character is literal.
fn push_class(stuff: &[char], out: &mut String) {
    let (negate, body) = match stuff.split_first() {
        Some(('!', rest)) => (true, rest),
        _ => (false, stuff),
    };
    let mut items = String::new();
    let mut buf = [0u8; 4];
    let mut k = 0;
    while k < body.len() {
        if k + 2 < body.len() && body[k + 1] == '-' {
            let (lo, hi) = (body[k], body[k + 2]);
            if lo <= hi {
                items.push_str(&regex::escape(lo.encode_utf8(&mut buf)));
                items.push('-');
                items.push_str(&regex::escape(hi.encode_utf8(&mut buf)));
            }
            k += 3;
        } else {
            items.push_str(&regex::escape(body[k].encode_utf8(&mut buf)));
            k += 1;
        }
    }
    match (items.is_empty(), negate) {
        (true, false) => out.push_str(r"[^\s\S]"), // only empty ranges: never matches
        (true, true) => out.push('.'),
        (false, false) => {
            out.push('[');
            out.push_str(&items);
            out.push(']');
        }
        (false, true) => {
            out.push_str("[^");
            out.push_str(&items);
            out.push(']');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, name: &str) -> bool {
        NameMatcher::new(pattern, false, false).unwrap().matches(name)
    }

    #[test]
    fn total_commander_masks() {
        let cases = [
            ("*.pdf", "Report.PDF", true), // case-insensitive by default
            ("report", "big-report.zip", true), // no wildcards: anywhere in the name
            ("*.py;*.md", "notes.md", true),
            ("*.py *.md", "x.txt", false),
            ("*.zip|*big*", "big-report.zip", false), // "|" excludes
            ("*.zip|*big*", "small.zip", true),
            ("|*.tmp", "file.txt", true), // only exclusions: everything else
            ("|*.tmp", "file.tmp", false),
            ("report?.pdf", "report1.pdf", true),
        ];
        for (pattern, name, expected) in cases {
            assert_eq!(m(pattern, name), expected, "{pattern} vs {name}");
        }
    }

    #[test]
    fn name_options() {
        assert!(!NameMatcher::new("*.pdf", false, true).unwrap().matches("A.PDF"));
        assert!(NameMatcher::new(r"^report-\d{4}", true, false).unwrap().matches("Report-2026.PDF"));
        assert!(!NameMatcher::new(r"^report-\d{4}", true, true).unwrap().matches("Report-2026.PDF"));
    }

    #[test]
    fn empty_and_star_match_everything() {
        for p in ["", "  ", "*", "**", "* ;*"] {
            let nm = NameMatcher::new(p, false, false).unwrap();
            assert!(nm.matches_all() && nm.matches(".hidden"), "{p:?}");
        }
        assert!(NameMatcher::new(" ", true, false).unwrap().matches_all());
        assert!(!NameMatcher::new("|x", false, false).unwrap().matches_all());
    }

    #[test]
    fn fnmatch_classes() {
        assert!(m("file[0-9].txt", "file7.txt"));
        assert!(!m("file[0-9].txt", "fileA.txt"));
        assert!(m("file[!0-9].txt", "fileA.txt"));
        assert!(!m("file[!0-9].txt", "file1.txt"));
        assert!(m("[]]x", "]x")); // "]" first in a class is literal
        assert!(m("a[b", "a[b")); // unclosed "[" is literal
        assert!(!m("[z-a]x", "zx")); // reversed range is empty: never matches
        assert!(m("[-a]x", "-x"));
        assert!(m("[a-]x", "-x"));
        assert!(m("[^a]x", "^x")); // "^" is literal in fnmatch classes
        assert!(!m("[^a]x", "bx"));
    }

    #[test]
    fn regex_metacharacters_in_masks_are_literal() {
        assert!(m("a+b(1).txt", "a+b(1).txt"));
        assert!(!m("a.b", "axb"));
        assert!(m("*.{x}", "f.{x}"));
        assert!(!m("*.{x}", "f.x"));
    }

    #[test]
    fn wildcards_span_whole_name_and_unicode_case() {
        assert!(m("ä*", "Äpfel.txt"));
        assert!(m("*", "line\nbreak"));
        assert!(!m("?", "ab"));
        assert!(m("??", "äb"));
    }

    #[test]
    fn invalid_regex_is_an_error() {
        assert!(NameMatcher::new("(", true, false).is_err());
        assert!(NameMatcher::new("(", false, false).is_ok());
        assert!(NameMatcher::new(r"(?<=x", true, false).is_err());
    }

    #[test]
    fn python_look_around_and_back_references() {
        let nm = NameMatcher::new(r"^(?!report).*pdf", true, false).unwrap();
        assert!(nm.matches("invoice.pdf"));
        assert!(!nm.matches("Report-2026.pdf")); // case-insensitive like the rest
        assert!(NameMatcher::new(r"(?<=v)\d", true, true).unwrap().matches("v2.txt"));
        assert!(!NameMatcher::new(r"(?<=v)\d", true, true).unwrap().matches("x2.txt"));
        let twice = NameMatcher::new(r"^(\w+)-\1\.", true, true).unwrap();
        assert!(twice.matches("abc-abc.txt") && !twice.matches("abc-abd.txt"));
    }
}
