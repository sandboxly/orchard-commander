//! Search & replace with Python `re.sub` semantics: replacement templates
//! (`$1` like TC, `\1` / `\g<name>` like Python) and the way empty matches
//! are replaced, so RegEx renames give the same names as before.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use crate::rule::Rule;

/// Compiled search/replace pairs. A broken expression is kept as an error and
/// only reported when a name actually reaches this step (as in Python, where
/// items waiting for metadata never compile the expression).
pub(crate) struct SearchReplace {
    pairs: Result<Vec<Pair>, String>,
}

enum Pair {
    /// Plain text: matched literally, replacement inserted as is.
    Plain { re: regex::Regex, replacement: String },
    Regex {
        re: fancy_regex::Regex,
        /// The same expression refusing empty matches (None if it only ever
        /// matches empty), for Python's "must advance" rule after an empty match.
        non_empty: Option<fancy_regex::Regex>,
        template: Vec<Part>,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum Part {
    Literal(String),
    Group(usize),
}

impl SearchReplace {
    pub(crate) fn new(rule: &Rule) -> Self {
        if rule.search.is_empty() {
            return SearchReplace { pairs: Ok(Vec::new()) };
        }
        SearchReplace { pairs: compile(rule) }
    }

    /// Apply every pair in order; Err(message) for a bad expression or template.
    pub(crate) fn apply(&self, name: String) -> Result<String, String> {
        let pairs = self.pairs.as_ref().map_err(Clone::clone)?;
        let mut name = name;
        for pair in pairs {
            name = match pair {
                Pair::Plain { re, replacement } => {
                    let replaced = match re.replace_all(&name, regex::NoExpand(replacement)) {
                        std::borrow::Cow::Owned(s) => Some(s),
                        std::borrow::Cow::Borrowed(_) => None,
                    };
                    replaced.unwrap_or(name)
                }
                Pair::Regex { re, non_empty, template } => sub(re, non_empty.as_ref(), template, &name)?,
            };
        }
        Ok(name)
    }
}

/// Search terms paired with replacements: one replacement for all, or one per
/// term (the last one repeats). Empty terms are dropped.
fn pairs(rule: &Rule) -> impl Iterator<Item = (&str, &str)> {
    let replaces: Vec<&str> = rule.replace.split('|').collect();
    rule.search
        .split('|')
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(move |(i, s)| (s, replaces[i.min(replaces.len() - 1)]))
}

fn compile(rule: &Rule) -> Result<Vec<Pair>, String> {
    let mut out = Vec::new();
    for (search, replacement) in pairs(rule) {
        if !rule.regex {
            let re = regex::RegexBuilder::new(&plain_pattern(search, !rule.case_sensitive))
                .case_insensitive(!rule.case_sensitive)
                .build()
                .map_err(|e| e.to_string())?;
            out.push(Pair::Plain { re, replacement: replacement.to_string() });
            continue;
        }
        let pattern = python_pattern(search, !rule.case_sensitive);
        let build = |non_empty: bool| {
            fancy_regex::RegexBuilder::new(&pattern)
                .case_insensitive(!rule.case_sensitive)
                .find_not_empty(non_empty)
                .build()
        };
        let re = build(false).map_err(|e| e.to_string())?;
        let non_empty = build(true).ok();
        let groups = re.captures_len() - 1;
        let names: HashMap<&str, usize> =
            re.capture_names().enumerate().filter_map(|(i, n)| n.map(|n| (n, i))).collect();
        let template = parse_template(&dollar_groups(replacement), groups, &names)?;
        out.push(Pair::Regex { non_empty, template, re });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Python's IGNORECASE
// ---------------------------------------------------------------------------

/// Python's IGNORECASE compares simple lowercase mappings, so it treats i, I,
/// İ and ı as one letter; Unicode case folding (Rust's regex engines) splits
/// them into i/I and İ, ı. All other letters agree between the two.
const ANY_I: &str = "iIİı";

fn is_i(c: char) -> bool {
    ANY_I.contains(c)
}

/// The literal `search` as a pattern.
fn plain_pattern(search: &str, ignore_case: bool) -> String {
    if !ignore_case || !search.contains(is_i) {
        return regex::escape(search);
    }
    let mut out = String::with_capacity(search.len() + 8);
    let mut buf = [0; 4];
    for c in search.chars() {
        if is_i(c) {
            out.push_str("[iIİı]");
        } else {
            out.push_str(&regex::escape(c.encode_utf8(&mut buf)));
        }
    }
    out
}

/// Python's `\w`: letters, digits and numbers (`str.isalnum`) plus "_" — no
/// combining marks, unlike Unicode's word class that Rust's engines use.
const PY_WORD: &str = r"\p{L}\p{N}_";

/// `pattern` rewritten so Rust's engines match what Python's `re` matched:
/// `\w` / `\W` as in Python, octal escapes, character classes re-spelled
/// with escaped literals (see [`python_class`]) and, when ignoring case,
/// every literal i-letter standing for all four (see [`ANY_I`]). Group
/// syntax, other escapes and class ranges are left alone.
fn python_pattern(pattern: &str, ignore_case: bool) -> Cow<'_, str> {
    if !pattern.contains(['\\', '[']) && !(ignore_case && pattern.contains(is_i)) {
        return Cow::Borrowed(pattern);
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut i = 0;
    let mut in_class = false;
    let copy_through = |out: &mut String, i: &mut usize, end: char| {
        while *i < chars.len() {
            out.push(chars[*i]);
            *i += 1;
            if chars[*i - 1] == end {
                break;
            }
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            i += 1;
            // Octal escapes: "\0", "\012", "\101" (Rust reads them as back-references).
            if !in_class && let Some((value, len)) = octal_escape(&chars[i..], false) {
                push_literal(&mut out, value);
                i += len;
                continue;
            }
            match (chars.get(i), in_class) {
                // Case-sensitive, or folding would add marks like U+0345 (folds to "ι").
                (Some('w'), false) => out.push_str(&format!("(?-i:[{PY_WORD}])")),
                (Some('W'), false) => out.push_str(&format!("(?-i:[^{PY_WORD}])")),
                (Some('w'), true) => out.push_str(PY_WORD),
                (Some('W'), true) => out.push_str(&format!("[^{PY_WORD}]")), // a nested class
                (Some(&d), _) => {
                    out.push(c);
                    out.push(d);
                    if matches!(d, 'p' | 'P' | 'N' | 'x') && chars.get(i + 1) == Some(&'{') {
                        i += 1;
                        copy_through(&mut out, &mut i, '}');
                        continue;
                    }
                }
                (None, _) => out.push(c),
            }
            i += 1;
            continue;
        }
        if in_class {
            let in_range = chars.get(i + 1) == Some(&'-') || (i > 0 && chars[i - 1] == '-');
            match c {
                ']' => in_class = false,
                c if ignore_case && is_i(c) && !in_range => {
                    out.push_str(ANY_I);
                    i += 1;
                    continue;
                }
                _ => {}
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '['
            && let Some(end) = python_class(&chars, i, ignore_case, &mut out)
        {
            i = end;
            continue;
        }
        match c {
            '[' => {
                // Not a class Python accepts: leave it to the engine to complain.
                in_class = true;
                out.push(c);
                i += 1;
                // "[]...]" and "[^]...]": a leading "]" is a literal.
                if chars.get(i) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
                if chars.get(i) == Some(&']') {
                    out.push(']');
                    i += 1;
                }
            }
            '(' if chars.get(i + 1) == Some(&'?') => {
                // Group syntax: names, flags, look-arounds, comments, conditionals.
                out.push_str("(?");
                i += 2;
                let whole = matches!(chars.get(i), Some('#' | '(')) || chars[i..].starts_with(&['P', '=']);
                if whole {
                    copy_through(&mut out, &mut i, ')');
                } else {
                    while i < chars.len() {
                        let d = chars[i];
                        out.push(d);
                        i += 1;
                        if matches!(d, ':' | ')' | '>' | '=' | '!') {
                            break;
                        }
                    }
                }
            }
            c if ignore_case && is_i(c) => {
                out.push_str("[iIİı]");
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Cow::Owned(out)
}

/// `c` as a pattern literal that means the same inside and outside a class
/// and under `(?x)`: punctuation, spaces and controls as `\x{..}`.
fn push_literal(out: &mut String, c: char) {
    if c.is_alphanumeric() || !(c.is_ascii() || c.is_whitespace() || c.is_control()) {
        out.push(c);
    } else {
        out.push_str(&format!("\\x{{{:x}}}", c as u32));
    }
}

/// Python's octal escape after a backslash (`chars` starts behind it):
/// `(value, characters used)`. Outside a class only "\0", "\0oo" and three
/// octal digits are octal (else a group reference); inside one, 1–3 digits.
fn octal_escape(chars: &[char], in_class: bool) -> Option<(char, usize)> {
    let octal = |i: usize| chars.get(i).is_some_and(|c| ('0'..='7').contains(c));
    let len = if in_class || chars.first() == Some(&'0') {
        if !octal(0) {
            return None;
        }
        1 + (1..3).take_while(|&i| octal(i)).count()
    } else if octal(0) && octal(1) && octal(2) {
        3
    } else {
        return None;
    };
    let value = chars[..len].iter().fold(0u32, |acc, c| acc * 8 + c.to_digit(8).expect("octal"));
    // Above 0o377 Python refuses; leave it to the engine to refuse too.
    if value > 0o377 { None } else { char::from_u32(value).map(|c| (c, len)) }
}

/// One member of a Python character class.
enum ClassItem {
    Literal(char),
    /// Already in Rust syntax: `\d`, the Python word class, …
    Set(Cow<'static, str>),
}

/// The class member at `chars[*i]` the way Python's `re` reads it; None for
/// what Python refuses or this doesn't know (the class is then left alone).
fn class_item(chars: &[char], i: &mut usize) -> Option<ClassItem> {
    let c = *chars.get(*i)?;
    if c != '\\' {
        *i += 1;
        return Some(ClassItem::Literal(c));
    }
    let e = *chars.get(*i + 1)?;
    *i += 2;
    let hex = |i: &mut usize, n: usize| {
        let digits: String = chars.get(*i..*i + n)?.iter().collect();
        let value = u32::from_str_radix(&digits, 16).ok().filter(|_| digits.chars().all(|c| c.is_ascii_hexdigit()))?;
        *i += n;
        char::from_u32(value)
    };
    Some(match e {
        'a' => ClassItem::Literal('\x07'),
        'b' => ClassItem::Literal('\x08'),
        'f' => ClassItem::Literal('\x0c'),
        'n' => ClassItem::Literal('\n'),
        'r' => ClassItem::Literal('\r'),
        't' => ClassItem::Literal('\t'),
        'v' => ClassItem::Literal('\x0b'),
        'd' => ClassItem::Set(Cow::Borrowed(r"\d")),
        'D' => ClassItem::Set(Cow::Borrowed(r"\D")),
        's' => ClassItem::Set(Cow::Borrowed(r"\s")),
        'S' => ClassItem::Set(Cow::Borrowed(r"\S")),
        'w' => ClassItem::Set(Cow::Borrowed(PY_WORD)),
        'W' => ClassItem::Set(Cow::Owned(format!("[^{PY_WORD}]"))), // a nested class
        'x' => ClassItem::Literal(hex(i, 2)?),
        'u' => ClassItem::Literal(hex(i, 4)?),
        'U' => ClassItem::Literal(hex(i, 8)?),
        '0'..='7' => {
            let (value, len) = octal_escape(&chars[*i - 1..], true)?;
            *i += len - 1;
            ClassItem::Literal(value)
        }
        e if e.is_ascii_alphanumeric() => return None, // "bad escape" in Python (or \N{…})
        e => ClassItem::Literal(e),
    })
}

/// The Python class starting at `chars[start]` ("[") in Rust syntax, appended
/// to `out`; returns where it ends. Every literal is escaped, so what Rust
/// reads as nested classes or set operations ("[[]", "[a&&b]", "[~~]",
/// "[[:alpha:]]") stays the plain characters Python saw.
fn python_class(chars: &[char], start: usize, ignore_case: bool, out: &mut String) -> Option<usize> {
    let mut class = String::from("[");
    let mut i = start + 1;
    if chars.get(i) == Some(&'^') {
        class.push('^');
        i += 1;
    }
    let push = |class: &mut String, item: ClassItem| match item {
        ClassItem::Literal(c) if ignore_case && is_i(c) => class.push_str(ANY_I),
        ClassItem::Literal(c) => push_literal(class, c),
        ClassItem::Set(s) => class.push_str(&s),
    };
    let mut empty = true;
    loop {
        if *chars.get(i)? == ']' && !empty {
            i += 1;
            break;
        }
        let item = class_item(chars, &mut i)?;
        empty = false;
        if chars.get(i) != Some(&'-') {
            push(&mut class, item);
            continue;
        }
        i += 1;
        if *chars.get(i)? == ']' {
            push(&mut class, item);
            push_literal(&mut class, '-');
            i += 1;
            break;
        }
        match (item, class_item(chars, &mut i)?) {
            // Ranges are left as they are (no i-letter widening). Reversed
            // ranges stay reversed, so the engine refuses them as Python did.
            (ClassItem::Literal(lo), ClassItem::Literal(hi)) => {
                push_literal(&mut class, lo);
                class.push('-');
                push_literal(&mut class, hi);
            }
            _ => return None, // "bad character range"
        }
    }
    class.push(']');
    out.push_str(&class);
    Some(i)
}

/// `$1` like TC becomes `\1` like Python (Python's `\$(\d+)`, Unicode digits included).
fn dollar_groups(replacement: &str) -> std::borrow::Cow<'_, str> {
    static DOLLAR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\$(\d+)").expect("valid"));
    DOLLAR.replace_all(replacement, r"\${1}")
}

// ---------------------------------------------------------------------------
// Python's replacement template (re._parser.parse_template, Python 3.12)
// ---------------------------------------------------------------------------

/// Python's template tokenizer: one character, or a backslash and the next.
struct Tokenizer {
    chars: Vec<char>,
    index: usize,
    next: Option<(char, Option<char>)>,
}

fn error_at(msg: String, pos: usize) -> String {
    format!("{msg} at position {pos}")
}

impl Tokenizer {
    fn new(s: &str) -> Result<Self, String> {
        let mut t = Tokenizer { chars: s.chars().collect(), index: 0, next: None };
        t.advance()?;
        Ok(t)
    }

    fn advance(&mut self) -> Result<(), String> {
        let Some(&c) = self.chars.get(self.index) else {
            self.next = None;
            return Ok(());
        };
        if c == '\\' {
            let Some(&n) = self.chars.get(self.index + 1) else {
                return Err(error_at("bad escape (end of pattern)".into(), self.chars.len() - 1));
            };
            self.next = Some((c, Some(n)));
            self.index += 2;
        } else {
            self.next = Some((c, None));
            self.index += 1;
        }
        Ok(())
    }

    fn get(&mut self) -> Result<Option<(char, Option<char>)>, String> {
        let this = self.next;
        self.advance()?;
        Ok(this)
    }

    /// The next token if it is the plain character `c`.
    fn next_is(&self, pred: impl Fn(char) -> bool) -> Option<char> {
        match self.next {
            Some((c, None)) if pred(c) => Some(c),
            _ => None,
        }
    }

    fn tell(&self) -> usize {
        let pending = match self.next {
            None => 0,
            Some((_, None)) => 1,
            Some((_, Some(_))) => 2,
        };
        self.index - pending
    }

    fn error(&self, msg: String, offset: usize) -> String {
        error_at(msg, self.tell().saturating_sub(offset))
    }

    /// Characters up to `terminator` (consumed), for `\g<name>`.
    fn get_until(&mut self, terminator: char, what: &str) -> Result<String, String> {
        let mut result = String::new();
        loop {
            let c = self.get()?;
            match c {
                None if result.is_empty() => return Err(self.error(format!("missing {what}"), 0)),
                None => {
                    let len = result.chars().count();
                    return Err(self.error(format!("missing {terminator}, unterminated name"), len));
                }
                Some((t, None)) if t == terminator => {
                    if result.is_empty() {
                        return Err(self.error(format!("missing {what}"), 1));
                    }
                    return Ok(result);
                }
                Some((a, b)) => {
                    result.push(a);
                    result.extend(b);
                }
            }
        }
    }
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_alphabetic()) && chars.all(|c| c == '_' || c.is_alphanumeric())
}

fn parse_template(source: &str, groups: usize, names: &HashMap<&str, usize>) -> Result<Vec<Part>, String> {
    let mut s = Tokenizer::new(source)?;
    let mut parts = Vec::new();
    let mut literal = String::new();
    let add_group = |s: &Tokenizer, literal: &mut String, parts: &mut Vec<Part>, index: u64, offset: usize| {
        if index > groups as u64 {
            return Err(s.error(format!("invalid group reference {index}"), offset));
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(std::mem::take(literal)));
        }
        parts.push(Part::Group(index as usize));
        Ok(())
    };
    while let Some((c0, escaped)) = s.get()? {
        let Some(c) = escaped else {
            literal.push(c0);
            continue;
        };
        match c {
            'g' => {
                if s.next_is(|c| c == '<').is_none() {
                    return Err(s.error("missing <".into(), 0));
                }
                s.get()?;
                let name = s.get_until('>', "group name")?;
                let name_len = name.chars().count();
                let index = if name.bytes().all(|b| b.is_ascii_digit()) {
                    name.bytes().fold(0u64, |acc, d| acc.saturating_mul(10).saturating_add((d - b'0') as u64))
                } else {
                    if !is_identifier(&name) {
                        return Err(s.error(format!("bad character in group name '{name}'"), name_len + 1));
                    }
                    match names.get(name.as_str()) {
                        Some(&i) => i as u64,
                        None => return Err(format!("unknown group name '{name}'")),
                    }
                };
                add_group(&s, &mut literal, &mut parts, index, name_len + 1)?;
            }
            '0' => {
                let mut digits = String::from("0");
                for _ in 0..2 {
                    match s.next_is(|c| ('0'..='7').contains(&c)) {
                        Some(d) => {
                            digits.push(d);
                            s.get()?;
                        }
                        None => break,
                    }
                }
                let value = u32::from_str_radix(&digits, 8).expect("octal digits") & 0xff;
                literal.push(char::from_u32(value).expect("below 256"));
            }
            '1'..='9' => {
                let mut this = String::from(c);
                let mut is_octal = false;
                if let Some(d) = s.next_is(|c| c.is_ascii_digit()) {
                    s.get()?;
                    this.push(d);
                    let octal = |c: char| ('0'..='7').contains(&c);
                    let third = if octal(c) && octal(d) { s.next_is(octal) } else { None };
                    if let Some(e) = third {
                        s.get()?;
                        this.push(e);
                        is_octal = true;
                        let value = u32::from_str_radix(&this, 8).expect("octal digits");
                        if value > 0o377 {
                            return Err(s.error(
                                format!("octal escape value \\{this} outside of range 0-0o377"),
                                this.len() + 1,
                            ));
                        }
                        literal.push(char::from_u32(value).expect("below 256"));
                    }
                }
                if !is_octal {
                    let index: u64 = this.parse().expect("ascii digits");
                    add_group(&s, &mut literal, &mut parts, index, this.len())?;
                }
            }
            _ => match c {
                'a' => literal.push('\x07'),
                'b' => literal.push('\x08'),
                'f' => literal.push('\x0c'),
                'n' => literal.push('\n'),
                'r' => literal.push('\r'),
                't' => literal.push('\t'),
                'v' => literal.push('\x0b'),
                '\\' => literal.push('\\'),
                c if c.is_ascii_alphabetic() => return Err(s.error(format!("bad escape \\{c}"), 2)),
                c => {
                    literal.push('\\');
                    literal.push(c);
                }
            },
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    Ok(parts)
}

// ---------------------------------------------------------------------------
// Substitution
// ---------------------------------------------------------------------------

fn next_char_boundary(text: &str, pos: usize) -> usize {
    text[pos..].chars().next().map_or(text.len() + 1, |c| pos + c.len_utf8())
}

/// `re.sub(pattern, template, text)`: like Python 3.7+, an empty match right
/// after a non-empty one is replaced too ("x*" on "abxd" gives "-a-b--d-"),
/// but the search never stays on the same spot after an empty match.
fn sub(re: &fancy_regex::Regex, non_empty: Option<&fancy_regex::Regex>, template: &[Part], text: &str) -> Result<String, String> {
    let err = |e: fancy_regex::Error| e.to_string();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut pos = 0;
    let mut must_advance = false;
    while pos <= text.len() {
        let caps = if must_advance {
            // A non-empty match at this very spot, else any match further on.
            let here = match non_empty {
                Some(ne) => ne.captures_from_pos(text, pos).map_err(err)?.filter(|c| c.get(0).is_some_and(|m| m.start() == pos)),
                None => None,
            };
            match here {
                Some(c) => Some(c),
                None => {
                    let next = next_char_boundary(text, pos);
                    if next > text.len() { None } else { re.captures_from_pos(text, next).map_err(err)? }
                }
            }
        } else {
            re.captures_from_pos(text, pos).map_err(err)?
        };
        let Some(caps) = caps else { break };
        let m = caps.get(0).expect("group 0 always matches");
        out.push_str(&text[last..m.start()]);
        for part in template {
            match part {
                Part::Literal(s) => out.push_str(s),
                Part::Group(i) => out.push_str(caps.get(*i).map_or("", |g| g.as_str())),
            }
        }
        last = m.end();
        must_advance = m.start() == m.end();
        pos = m.end();
    }
    out.push_str(&text[last..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(search: &str, replace: &str, regex: bool) -> Rule {
        Rule { search: search.into(), replace: replace.into(), regex, ..Rule::default() }
    }

    fn apply(search: &str, replace: &str, regex: bool, text: &str) -> Result<String, String> {
        SearchReplace::new(&rule(search, replace, regex)).apply(text.to_string())
    }

    #[test]
    fn python_empty_match_rules() {
        assert_eq!(apply("x*", "-", true, "abxd").unwrap(), "-a-b--d-");
        assert_eq!(apply("", "-", true, "ab").unwrap(), "ab"); // empty search: nothing to do
        assert_eq!(apply("^", ">", true, "ab").unwrap(), ">ab");
        assert_eq!(apply("$", "<", true, "ab").unwrap(), "ab<");
        assert_eq!(apply("a??", "-", true, "a").unwrap(), "---");
    }

    #[test]
    fn templates() {
        assert_eq!(apply(r"(\w)(\w)", r"\2\1", true, "abcd").unwrap(), "badc");
        assert_eq!(apply(r"(?P<x>b)", r"[\g<x>\g<0>\g<1>]", true, "abc").unwrap(), "a[bbb]c");
        assert_eq!(apply("(a)?b", r"<\1>", true, "b").unwrap(), "<>"); // unmatched group: empty
        // "|" separates searches, so it can't be an alternation (as in Python).
        assert!(apply("(a)|(z)", r"<\2>", true, "a").is_err());
        assert_eq!(apply("a", r"\n\t\\\-", true, "a").unwrap(), "\n\t\\\\-");
        assert_eq!(apply("a", r"\101\0", true, "a").unwrap(), "A\0");
        assert_eq!(apply("(a)", "$1$1", true, "a").unwrap(), "aa");
        assert_eq!(apply("a", "$", true, "a").unwrap(), "$");
    }

    #[test]
    fn template_errors_like_python() {
        assert_eq!(apply("(a)", r"\2", true, "a").unwrap_err(), "invalid group reference 2 at position 1");
        assert_eq!(apply("a", r"\q", true, "a").unwrap_err(), "bad escape \\q at position 0");
        assert_eq!(apply("a", "x\\", true, "a").unwrap_err(), "bad escape (end of pattern) at position 1");
        assert_eq!(apply("a", r"\g<y>", true, "a").unwrap_err(), "unknown group name 'y'");
        assert_eq!(apply("a", r"\g<1", true, "a").unwrap_err(), "missing >, unterminated name at position 3");
        assert_eq!(apply("a", r"\g1", true, "a").unwrap_err(), "missing < at position 2");
        assert_eq!(apply("a", r"\g<>", true, "a").unwrap_err(), "missing group name at position 3");
        assert_eq!(apply("a", r"\g<a-b>", true, "a").unwrap_err(), "bad character in group name 'a-b' at position 3");
        assert_eq!(apply("a", r"\777", true, "a").unwrap_err(), "octal escape value \\777 outside of range 0-0o377 at position 0");
        assert!(apply("(", "", true, "a").is_err());
        // Errors surface even when nothing matches (Python compiles the template first).
        assert!(apply("zzz", r"\5", true, "a").is_err());
    }

    #[test]
    fn dotted_and_dotless_i_like_python() {
        let w = |p| python_pattern(p, true).into_owned();
        assert_eq!(w("(?P<kind>i)[a-ix]\\i(?i)"), "(?P<kind>[iIİı])[a-ix]\\i(?i)");
        assert_eq!(w("(?P=kind)(?#ii)[]i](?<=i)"), "(?P=kind)(?#ii)[\\x{5d}iIİı](?<=[iIİı])");
        assert_eq!(w("\\p{Latin}i"), "\\p{Latin}[iIİı]");
        assert_eq!(python_pattern("i", false), "i");
        assert_eq!(apply("i", "_", false, "İstanbul ılık Ii").unwrap(), "_stanbul _l_k __");
        assert_eq!(apply("[aeiou]", "", true, "İstanbul").unwrap(), "stnbl");
        let cs = Rule { search: "i".into(), replace: "_".into(), case_sensitive: true, ..Rule::default() };
        assert_eq!(SearchReplace::new(&cs).apply("İiI".into()).unwrap(), "İ_I");
    }

    #[test]
    fn python_word_class() {
        assert_eq!(python_pattern(r"\w+\W[\w.][^\W]", false), r"(?-i:[\p{L}\p{N}_])+(?-i:[^\p{L}\p{N}_])[\p{L}\p{N}_\x{2e}][^[^\p{L}\p{N}_]]");
        assert_eq!(python_pattern(r"\d\\w", false), r"\d\\w");
        // Combining marks are not word characters in Python.
        assert_eq!(apply(r"\w+", "<>", true, "cafe\u{301}!").unwrap(), "<>\u{301}!");
        assert_eq!(apply(r"[^\W]", "x", true, "a_1-").unwrap(), "xxx-");
    }

    #[test]
    fn classes_and_octal_escapes_like_python() {
        // Python reads these as plain characters; Rust as nested classes / set operations.
        assert_eq!(apply("[[]", "_", true, "a[b").unwrap(), "a_b");
        assert_eq!(apply("[a&&b]", "_", true, "a&b").unwrap(), "___");
        assert_eq!(apply("[~~]", "_", true, "~x").unwrap(), "_x");
        assert_eq!(apply("[[:alpha:]]", "_", true, "a]b:]").unwrap(), "_b_");
        assert_eq!(apply("[]-]", "_", true, "a]-b").unwrap(), "a__b");
        assert_eq!(apply("[a-]", "_", true, "a-b").unwrap(), "__b");
        assert_eq!(apply(r"[\]\-\\]", "_", true, r"a]-\b").unwrap(), "a___b");
        assert_eq!(apply(r"[\x41-\x43\t]", "_", true, "abd\t").unwrap(), "__d_");
        assert_eq!(apply(r"(?x)[ ]a", "_", true, "b a").unwrap(), "b_");
        assert_eq!(apply(r"[\101]", "_", true, "aB").unwrap(), "_B");
        assert_eq!(apply(r"\101\0", "_", true, "xa\0").unwrap(), "x_");
        assert_eq!(apply(r"(a)\1", "_", true, "aa").unwrap(), "_"); // still a back-reference
        // What Python refuses stays refused.
        assert!(apply("[z-a]", "_", true, "a").is_err());
        assert!(apply("[a", "_", true, "a").is_err());
    }

    #[test]
    fn plain_and_pairs() {
        assert_eq!(apply(".", "_", false, "a.b.c").unwrap(), "a_b_c");
        assert_eq!(apply("-|_", " ", false, "a-b_c").unwrap(), "a b c");
        assert_eq!(apply("-|_|x", "+|=", false, "a-b_cx").unwrap(), "a+b=c=");
        assert_eq!(apply("||b", "1|2|3", false, "abc").unwrap(), "a3c");
        assert_eq!(apply("b", "$1\\1", false, "abc").unwrap(), "a$1\\1c"); // plain: no groups
        assert_eq!(apply("B", "x", false, "abc").unwrap(), "axc");
    }
}
