//! Name and extension masks: placeholders expanded for one item.
//!
//! Positions and ranges count characters (Unicode scalar values), as in the
//! Python version, so `[N1-3]` and error positions agree with what users see.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Datelike, Local, Timelike};

use crate::metadata::{self, Registry};
use crate::rule::{Case, Item, Rule, split_name};
use crate::search::SearchReplace;

/// A mask can't be read; `position` is where in the mask (0-based, in characters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskError {
    pub message: String,
    pub position: usize,
}

impl fmt::Display for MaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MaskError {}

/// Why no new name could be computed for an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameError {
    /// The rule's mask is broken (affects every item).
    Mask(MaskError),
    /// The search expression or its replacement is broken.
    Search(String),
    /// A `[=field]` placeholder needs metadata that hasn't been read yet.
    MetadataPending,
    /// The file has no value for a `[=field]` placeholder (and it has no fallback).
    MissingMetadata(String),
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Mask(e) => e.fmt(f),
            NameError::Search(e) => write!(f, "Search: {e}"),
            NameError::MetadataPending => f.write_str("reading metadata…"),
            NameError::MissingMetadata(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for NameError {}

impl From<MaskError> for NameError {
    fn from(e: MaskError) -> Self {
        NameError::Mask(e)
    }
}

/// Character index of byte offset `byte` in `s`.
fn char_pos(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

fn mask_error(mask: &str, byte: usize, message: String) -> NameError {
    NameError::Mask(MaskError { message, position: char_pos(mask, byte) })
}

// ---------------------------------------------------------------------------
// Ranges
// ---------------------------------------------------------------------------

/// `text[start:stop]` with Python's slice rules (negative counts from the
/// end, out-of-range clamps), on characters.
fn py_slice(text: &str, start: i128, stop: Option<i128>) -> &str {
    let len = text.chars().count() as i128;
    let norm = |i: i128| if i < 0 { (i + len).max(0) } else { i.min(len) };
    let a = norm(start);
    let b = stop.map_or(len, norm);
    if b <= a {
        return "";
    }
    let byte_at = |n: i128| text.char_indices().nth(n as usize).map_or(text.len(), |(i, _)| i);
    let from = byte_at(a);
    let to = if b == len { text.len() } else { byte_at(b) };
    &text[from..to]
}

/// Parse `-?\d+` (ASCII digits) at the start of `s`; saturates instead of
/// overflowing, which gives the same slices as Python's big ints.
fn parse_int(s: &str) -> Option<(i64, &str)> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let value = rest[..digits].bytes().fold(0i64, |acc, d| acc.saturating_mul(10).saturating_add((d - b'0') as i64));
    Some((if neg { -value } else { value }, &rest[digits..]))
}

#[derive(Clone, Copy, Debug)]
enum Sep {
    Dash,
    Comma,
}

/// A range spec: `(start, separator, end)`, or None if it doesn't parse.
fn parse_range(spec: &str) -> Option<(i64, Option<Sep>, Option<i64>)> {
    let (start, rest) = parse_int(spec)?;
    let mut chars = rest.chars();
    let sep = match chars.next() {
        None => return Some((start, None, None)),
        Some('-') => Sep::Dash,
        Some(',') => Sep::Comma,
        Some(_) => return None,
    };
    let rest = chars.as_str();
    if rest.is_empty() {
        return Some((start, Some(sep), None));
    }
    match parse_int(rest)? {
        (end, "") => Some((start, Some(sep), Some(end))),
        _ => None,
    }
}

/// Ranges: '2-5', '2,5', '2-', '-8,5', '-8-5', '2--5', '-5-', '3' (one char).
/// `at` is the byte offset of the placeholder in `mask`, for errors.
fn slice<'t>(text: &'t str, spec: &str, mask: &str, at: usize) -> Result<&'t str, NameError> {
    if spec.is_empty() {
        return Ok(text);
    }
    let Some((start, sep, end)) = parse_range(spec) else {
        return Err(mask_error(mask, at, format!("Unknown range “{spec}”")));
    };
    if start == 0 {
        return Err(mask_error(mask, at, "Ranges start at 1".into()));
    }
    let len = text.chars().count() as i128;
    let start = start as i128;
    let i = if start > 0 { start - 1 } else { len + start }.max(0);
    Ok(match (sep, end) {
        (None, _) => py_slice(text, i, Some(i + 1)),
        (Some(Sep::Comma), None) => return Err(mask_error(mask, at, format!("Missing length in “{spec}”"))),
        (Some(Sep::Comma), Some(n)) => py_slice(text, i, Some(i + (n as i128).max(0))),
        (Some(Sep::Dash), None) => py_slice(text, i, None),
        (Some(Sep::Dash), Some(end)) => {
            let mut stop = end as i128;
            if start < 0 && stop > 0 {
                stop = -stop; // [N-8-5]: both counted from the end
            }
            if stop > 0 { py_slice(text, i, Some(stop)) } else { py_slice(text, i, Some(len + stop + 1)) }
        }
    })
}

// ---------------------------------------------------------------------------
// Case
// ---------------------------------------------------------------------------

/// Python's `[^\W_]`: letters, digits and numbers (`str.isalnum`), without
/// the combining marks Rust's `char::is_alphanumeric` counts.
static WORD: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[\p{L}\p{N}]+").expect("valid"));

fn push_upper_first(out: &mut String, word: &str) {
    let mut chars = word.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(&chars.as_str().to_lowercase());
    }
}

/// Append `text` converted to `case`.
pub(crate) fn push_case(out: &mut String, text: &str, case: Case) {
    match case {
        Case::Unchanged => out.push_str(text),
        Case::Lower => out.push_str(&text.to_lowercase()),
        Case::Upper => out.push_str(&text.to_uppercase()),
        Case::First => push_upper_first(out, text),
        Case::Words => {
            let mut last = 0;
            for m in WORD.find_iter(text) {
                out.push_str(&text[last..m.start()]);
                push_upper_first(out, m.as_str());
                last = m.end();
            }
            out.push_str(&text[last..]);
        }
    }
}

pub(crate) fn apply_case(text: String, case: Case) -> String {
    if case == Case::Unchanged {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    push_case(&mut out, &text, case);
    out
}

fn switch(cmd: &str) -> Option<Case> {
    Some(match cmd {
        "U" => Case::Upper,
        "L" => Case::Lower,
        "F" => Case::Words,
        "n" => Case::Unchanged,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

/// Python's round(): halves to even.
pub(crate) fn round_half_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 { 2.0 * (x / 2.0).round() } else { r }
}

/// Local time of `mtime` like `datetime.fromtimestamp` (which rounds to the
/// microsecond, so 9.9999999 shows as 10 seconds); "now" for 0.
fn local_time(mtime: f64) -> DateTime<Local> {
    if mtime == 0.0 || !mtime.is_finite() {
        return Local::now();
    }
    let mut secs = mtime.trunc();
    let mut us = round_half_even((mtime - secs) * 1e6);
    if us >= 1e6 {
        secs += 1.0;
        us -= 1e6;
    } else if us < 0.0 {
        secs -= 1.0;
        us += 1e6;
    }
    DateTime::from_timestamp(secs as i64, (us as u32) * 1000).map_or_else(Local::now, |t| t.with_timezone(&Local))
}

// ---------------------------------------------------------------------------
// Expansion
// ---------------------------------------------------------------------------

/// A [`Rule`] prepared for many items: the search expressions are compiled
/// once and the metadata fields come from one registry snapshot.
pub struct Renamer {
    rule: Rule,
    registry: Arc<Registry>,
    search: SearchReplace,
}

impl Renamer {
    /// With the global metadata registry ([`metadata::registry`]).
    pub fn new(rule: &Rule) -> Self {
        Self::with_registry(rule, metadata::registry())
    }

    pub fn with_registry(rule: &Rule, registry: Arc<Registry>) -> Self {
        Renamer { search: SearchReplace::new(rule), rule: rule.clone(), registry }
    }

    pub fn rule(&self) -> &Rule {
        &self.rule
    }

    /// The new name for `item`; `index` counts from 0 in the batch (for `[C]`).
    pub fn new_name(&self, item: &Item, index: usize) -> Result<String, NameError> {
        let mut name = String::with_capacity(item.name.len() + 8);
        self.expand_into(&mut name, &self.rule.name_mask, item, index)?;
        if !item.is_dir {
            let stem_len = name.len();
            name.push('.');
            self.expand_into(&mut name, &self.rule.ext_mask, item, index)?;
            if name.len() == stem_len + 1 {
                name.truncate(stem_len); // empty extension: no dot
            }
        }
        let name = self.search.apply(name).map_err(NameError::Search)?;
        Ok(apply_case(name, self.rule.case))
    }

    /// Apply one mask (name or extension) to `item`.
    pub fn expand(&self, mask: &str, item: &Item, index: usize) -> Result<String, NameError> {
        let mut out = String::new();
        self.expand_into(&mut out, mask, item, index)?;
        Ok(out)
    }

    fn expand_into(&self, out: &mut String, mask: &str, item: &Item, index: usize) -> Result<(), NameError> {
        let (stem, ext) = split_name(&item.name, item.is_dir);
        let mut when: Option<DateTime<Local>> = None; // only computed if a date is asked for
        let mut case = Case::Unchanged;
        let mut pos = 0;
        while pos < mask.len() {
            let rest = &mask[pos..];
            if !rest.starts_with('[') {
                if rest.starts_with(']') {
                    return Err(mask_error(mask, pos, "“]” without “[” (write []] for a bracket)".into()));
                }
                let next = rest.find(['[', ']']).map_or(mask.len(), |i| pos + i);
                push_case(out, &mask[pos..next], case);
                pos = next;
                continue;
            }
            if rest.starts_with("[[]") || rest.starts_with("[]]") {
                out.push(rest.as_bytes()[1] as char);
                pos += 3;
                continue;
            }
            let Some(close) = rest[1..].find(']').map(|i| pos + 1 + i) else {
                return Err(mask_error(mask, pos, "“[” is never closed".into()));
            };
            let cmd = &mask[pos + 1..close];
            if let Some(c) = switch(cmd) {
                case = c;
            } else {
                let value = self.placeholder(cmd, item, stem, ext, &mut when, index, mask, pos)?;
                push_case(out, &value, case);
            }
            pos = close + 1;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn placeholder<'a>(
        &self,
        cmd: &'a str,
        item: &'a Item,
        stem: &'a str,
        ext: &'a str,
        when: &mut Option<DateTime<Local>>,
        index: usize,
        mask: &str,
        at: usize,
    ) -> Result<Cow<'a, str>, NameError> {
        if let Some(field) = cmd.strip_prefix('=') {
            return self.metadata_field(cmd, field, item, mask, at);
        }
        let mut chars = cmd.chars();
        let head = chars.next();
        let spec = chars.as_str();
        match head {
            Some('N') => return Ok(Cow::Borrowed(slice(stem, spec, mask, at)?)),
            Some('E') => return Ok(Cow::Borrowed(slice(ext, spec, mask, at)?)),
            Some('P') => return Ok(Cow::Borrowed(slice(&item.parent, spec, mask, at)?)),
            Some('G') => return Ok(Cow::Borrowed(slice(&item.grandparent, spec, mask, at)?)),
            // [2-5]: the whole name with extension
            Some(c) if c.is_ascii_digit() || c == '-' => return Ok(Cow::Borrowed(slice(&item.name, cmd, mask, at)?)),
            Some('C') => return self.counter(cmd, index, mask, at).map(Cow::Owned),
            _ => {}
        }
        let date_part = matches!(cmd, "d" | "t" | "Y" | "M" | "D" | "h" | "m" | "s");
        if !date_part {
            return Err(mask_error(mask, at, format!("Unknown placeholder “[{cmd}]”")));
        }
        let t = *when.get_or_insert_with(|| local_time(item.mtime));
        Ok(Cow::Owned(match cmd {
            "d" => format!("{:04}-{:02}-{:02}", t.year(), t.month(), t.day()),
            "t" => format!("{:02}.{:02}.{:02}", t.hour(), t.minute(), t.second()),
            "Y" => format!("{:04}", t.year()),
            "M" => format!("{:02}", t.month()),
            "D" => format!("{:02}", t.day()),
            "h" => format!("{:02}", t.hour()),
            "m" => format!("{:02}", t.minute()),
            _ => format!("{:02}", t.second()),
        }))
    }

    /// `[C]`, `[C10]`, `[C+5]`, `[C:3]`, `[C10+5:3]`.
    fn counter(&self, cmd: &str, index: usize, mask: &str, at: usize) -> Result<String, NameError> {
        let bad = || mask_error(mask, at, format!("Unknown counter “[{cmd}]” (example: [C10+5:3])"));
        let digits_at = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
        let mut rest = &cmd[1..];
        let mut start = None;
        let n = digits_at(rest);
        if n > 0 {
            start = Some(parse_int(&rest[..n]).ok_or_else(bad)?.0);
            rest = &rest[n..];
        }
        let mut step = None;
        if let Some(r) = rest.strip_prefix('+') {
            let (value, r) = parse_int(r).ok_or_else(bad)?;
            step = Some(value);
            rest = r;
        }
        let mut digits = None;
        if let Some(r) = rest.strip_prefix(':') {
            let n = digits_at(r);
            if n == 0 {
                return Err(bad());
            }
            digits = Some(parse_int(&r[..n]).ok_or_else(bad)?.0);
            rest = &r[n..];
        }
        if !rest.is_empty() {
            return Err(bad());
        }
        let start = start.unwrap_or(self.rule.counter_start) as i128;
        let step = step.unwrap_or(self.rule.counter_step) as i128;
        // Wider than any name: absurd widths would only exhaust memory.
        let width = digits.unwrap_or(self.rule.counter_digits).clamp(1, 4096) as usize;
        let value = start.saturating_add((index as i128).saturating_mul(step));
        Ok(zfill(&value.to_string(), width))
    }

    /// `[=artist]`, `[=artist:1-3]` (range as for [N]), `[=artist|Unknown]` (fallback when missing).
    fn metadata_field<'a>(
        &self,
        cmd: &str,
        field: &'a str,
        item: &'a Item,
        mask: &str,
        at: usize,
    ) -> Result<Cow<'a, str>, NameError> {
        let Some((name, spec, fallback)) = parse_field(field) else {
            return Err(mask_error(mask, at, format!("Unknown placeholder “[{cmd}]” (example: [=artist])")));
        };
        if !self.registry.has_field(name) {
            return Err(mask_error(mask, at, format!("Unknown field “{name}” (see Insert… for the list)")));
        }
        let spec = spec.unwrap_or("");
        if !spec.is_empty() {
            slice("", spec, mask, at)?; // check the range syntax even before metadata arrives
        }
        let Some(meta) = &item.meta else {
            return Err(NameError::MetadataPending);
        };
        match meta.get(name).filter(|v| !v.is_empty()) {
            Some(value) => Ok(Cow::Borrowed(slice(value, spec, mask, at)?)),
            None => match fallback {
                Some(f) => Ok(Cow::Borrowed(f)),
                None if !item.meta_error.is_empty() => Err(NameError::MissingMetadata(item.meta_error.clone())),
                None => Err(NameError::MissingMetadata(format!("no {name}"))),
            },
        }
    }
}

/// `name(:spec)?(|fallback)?` after the "=" — the Python `=(\w+)(?::([-,\d]*))?(?:\|(.*))?`.
fn parse_field(s: &str) -> Option<(&str, Option<&str>, Option<&str>)> {
    static NAME: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[\p{L}\p{N}_]+").expect("valid"));
    let name_len = NAME.find(s).map_or(0, |m| m.end());
    if name_len == 0 {
        return None;
    }
    let (name, mut rest) = s.split_at(name_len);
    let mut spec = None;
    if let Some(r) = rest.strip_prefix(':') {
        let n = r.find(|c: char| !(c == '-' || c == ',' || c.is_ascii_digit())).unwrap_or(r.len());
        spec = Some(&r[..n]);
        rest = &r[n..];
    }
    if rest.is_empty() {
        return Some((name, spec, None));
    }
    rest.strip_prefix('|').map(|fallback| (name, spec, Some(fallback)))
}

/// Python's `str.zfill`: zeros after the sign.
fn zfill(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        return s.to_string();
    }
    let (sign, digits) = match s.strip_prefix(['-', '+']) {
        Some(d) => (&s[..1], d),
        None => ("", s),
    };
    let mut out = String::with_capacity(width);
    out.push_str(sign);
    out.extend(std::iter::repeat_n('0', width - len));
    out.push_str(digits);
    out
}

/// Whether the rule uses `[=field]` placeholders (then metadata must be read first).
pub fn needs_metadata(rule: &Rule) -> bool {
    rule.name_mask.contains("[=") || rule.ext_mask.contains("[=")
}

/// Apply one mask to `item` (global metadata registry). Prefer a [`Renamer`] for batches.
pub fn expand(mask: &str, item: &Item, index: usize, rule: &Rule) -> Result<String, NameError> {
    Renamer::new(rule).expand(mask, item, index)
}

/// The new name for `item` (global metadata registry). Prefer a [`Renamer`] for batches.
pub fn new_name(item: &Item, index: usize, rule: &Rule) -> Result<String, NameError> {
    Renamer::new(rule).new_name(item, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_slices() {
        assert_eq!(py_slice("abcdef", 1, Some(3)), "bc");
        assert_eq!(py_slice("abcdef", 4, None), "ef");
        assert_eq!(py_slice("abc", 0, Some(-6)), "");
        assert_eq!(py_slice("abc", 0, Some(-1)), "ab");
        assert_eq!(py_slice("äöü", 1, Some(2)), "ö");
        assert_eq!(py_slice("abc", 5, Some(9)), "");
    }

    #[test]
    fn ranges() {
        assert!(matches!(parse_range("2--5"), Some((2, Some(Sep::Dash), Some(-5)))));
        assert!(matches!(parse_range("-5-"), Some((-5, Some(Sep::Dash), None))));
        assert!(matches!(parse_range("2,"), Some((2, Some(Sep::Comma), None))));
        assert!(parse_range("2x").is_none());
        assert!(parse_range("").is_none());
        assert!(parse_range("2-5-").is_none());
        assert!(matches!(parse_range("99999999999999999999999"), Some((i64::MAX, None, None))));
    }

    #[test]
    fn fields() {
        assert_eq!(parse_field("artist"), Some(("artist", None, None)));
        assert_eq!(parse_field("artist:1-3|x|y"), Some(("artist", Some("1-3"), Some("x|y"))));
        assert_eq!(parse_field("artist:"), Some(("artist", Some(""), None)));
        assert_eq!(parse_field("artist:1x"), None);
        assert_eq!(parse_field(""), None);
        assert_eq!(parse_field("a b"), None);
    }

    #[test]
    fn zfill_like_python() {
        assert_eq!(zfill("5", 3), "005");
        assert_eq!(zfill("-5", 3), "-05");
        assert_eq!(zfill("1234", 3), "1234");
    }

    #[test]
    fn words_and_first() {
        let conv = |s: &str, c| apply_case(s.to_string(), c);
        assert_eq!(conv("hello WORLD-again_x.txt", Case::Words), "Hello World-Again_X.Txt");
        assert_eq!(conv("élan vital", Case::Words), "Élan Vital");
        assert_eq!(conv("hELLO", Case::First), "Hello");
        assert_eq!(conv("straße", Case::Upper), "STRASSE");
        assert_eq!(conv("ΟΔΟΣ", Case::Lower), "οδος");
        assert_eq!(conv("", Case::First), "");
    }

    #[test]
    fn banker_rounding() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(3.5), 4.0);
        assert_eq!(round_half_even(-2.5), -2.0);
        assert_eq!(round_half_even(2.4), 2.0);
        assert_eq!(round_half_even(2.6), 3.0);
    }
}
