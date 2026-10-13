//! Content matching: finds text in a stream chunk by chunk.
//!
//! Works on raw bytes (`regex::bytes`, or `memchr::memmem` for plain
//! case-sensitive text) so nothing is decoded unless there is a hit, and
//! stops at the first match. Memory stays bounded at one chunk plus the
//! overlap that is carried over so matches across chunk borders are found.

use std::io::{self, Read};

use memchr::memmem;
use regex::bytes::{Regex, RegexBuilder};

/// Bytes read per step.
pub const CHUNK: usize = 1024 * 1024;
/// Bytes kept between chunks so matches across chunk borders are found.
pub const OVERLAP: usize = 4096;
/// Longest snippet (in characters) reported for a hit.
pub const SNIPPET_CHARS: usize = 160;

/// Finds the "Containing" text in file contents.
#[derive(Clone, Debug)]
pub struct TextMatcher {
    engine: Engine,
}

#[derive(Clone, Debug)]
enum Engine {
    /// Case-sensitive plain text: SIMD substring search.
    Literal(memmem::Finder<'static>),
    Regex(Regex),
}

/// Result of searching one stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The first matching line (trimmed, at most [`SNIPPET_CHARS`] characters).
    Found(String),
    NotFound,
    Cancelled,
}

impl TextMatcher {
    /// `text` is a regular expression if `regex`, else literal text. Matching is
    /// multi-line (`^`/`$` match at line breaks), as in the Python version.
    pub fn new(text: &str, regex: bool, case: bool, whole_words: bool) -> Result<Self, regex::Error> {
        if !regex && case && !whole_words {
            let finder = memmem::Finder::new(text.as_bytes()).into_owned();
            return Ok(TextMatcher { engine: Engine::Literal(finder) });
        }
        let escaped;
        let mut pattern = if regex {
            text
        } else {
            escaped = regex::escape(text);
            &escaped
        };
        let worded;
        if whole_words {
            worded = format!(r"\b(?:{pattern})\b");
            pattern = &worded;
        }
        let re = RegexBuilder::new(pattern).case_insensitive(!case).multi_line(true).build()?;
        Ok(TextMatcher { engine: Engine::Regex(re) })
    }

    fn find(&self, haystack: &[u8]) -> Option<(usize, usize)> {
        match &self.engine {
            Engine::Literal(f) => f.find(haystack).map(|s| (s, s + f.needle().len())),
            Engine::Regex(re) => re.find(haystack).map(|m| (m.start(), m.end())),
        }
    }

    /// The first matching line, or None. Stops early (returning None) once
    /// `is_cancelled` says so.
    pub fn search<R: Read + ?Sized>(&self, stream: &mut R, is_cancelled: &dyn Fn() -> bool) -> io::Result<Option<String>> {
        let mut buf = Vec::new();
        Ok(match self.search_with(stream, &mut buf, is_cancelled)? {
            Outcome::Found(s) => Some(s),
            Outcome::NotFound | Outcome::Cancelled => None,
        })
    }

    /// [`search`](Self::search) with a caller-owned buffer, so walkers can reuse
    /// one per thread instead of allocating a chunk per file.
    pub(crate) fn search_with<R: Read + ?Sized>(
        &self,
        stream: &mut R,
        buf: &mut Vec<u8>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Outcome> {
        buf.clear();
        loop {
            if is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            let read = fill(stream, buf, CHUNK)?;
            if read == 0 {
                return Ok(Outcome::NotFound); // the carried tail was searched last round
            }
            if let Some((start, end)) = self.find(buf) {
                return Ok(Outcome::Found(snippet(buf, start, end)));
            }
            // Keep the last OVERLAP bytes, starting on a character boundary.
            let mut keep_from = buf.len().saturating_sub(OVERLAP);
            while keep_from < buf.len() && buf[keep_from] & 0xC0 == 0x80 {
                keep_from += 1;
            }
            buf.drain(..keep_from);
        }
    }
}

/// Append up to `limit` bytes from `stream` (fewer only at the end of it).
fn fill<R: Read + ?Sized>(stream: &mut R, buf: &mut Vec<u8>, limit: usize) -> io::Result<usize> {
    let before = buf.len();
    buf.reserve(limit);
    let mut taken = Read::take(&mut *stream, limit as u64);
    loop {
        match taken.read_to_end(buf) {
            Ok(_) => return Ok(buf.len() - before),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

/// The line around a match, trimmed and cut to [`SNIPPET_CHARS`] characters.
fn snippet(buf: &[u8], start: usize, end: usize) -> String {
    let line_start = memchr::memrchr(b'\n', &buf[..start]).map_or(0, |i| i + 1);
    let line_end = memchr::memchr(b'\n', &buf[end..]).map_or(buf.len(), |i| end + i);
    let line = String::from_utf8_lossy(&buf[line_start..line_end]);
    let line = line.trim();
    match line.char_indices().nth(SNIPPET_CHARS) {
        Some((cut, _)) => line[..cut].to_string(),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn first_line(m: &TextMatcher, data: &[u8]) -> Option<String> {
        m.search(&mut Cursor::new(data.to_vec()), &|| false).unwrap()
    }

    #[test]
    fn literal_case_and_snippet() {
        let m = TextMatcher::new("print", false, true, false).unwrap();
        assert_eq!(first_line(&m, b"import os\n\n    print('hello world')\n").as_deref(), Some("print('hello world')"));
        let m = TextMatcher::new("QUICK", false, false, false).unwrap();
        assert_eq!(first_line(&m, b"The Quick brown fox\r\n").as_deref(), Some("The Quick brown fox"));
        let m = TextMatcher::new("QUICK", false, true, false).unwrap();
        assert_eq!(first_line(&m, b"The Quick brown fox"), None);
    }

    #[test]
    fn whole_words_and_regex() {
        let m = TextMatcher::new("no", false, false, true).unwrap();
        assert_eq!(first_line(&m, b"nothing else\n"), None);
        assert_eq!(first_line(&m, b"say no more").as_deref(), Some("say no more"));
        let m = TextMatcher::new(r"def \w+\(\)", true, false, false).unwrap();
        assert_eq!(first_line(&m, b"x\ndef main():\n").as_deref(), Some("def main():"));
        let m = TextMatcher::new(r"^b", true, true, false).unwrap(); // multi-line
        assert_eq!(first_line(&m, b"a\nbee\n").as_deref(), Some("bee"));
    }

    #[test]
    fn match_across_chunk_border_and_late_in_file() {
        let m = TextMatcher::new("needle", false, false, false).unwrap();
        let mut data = vec![b'x'; CHUNK - 3];
        data.extend_from_slice(b"NEEDLE");
        data.extend(std::iter::repeat_n(b'y', 100));
        assert!(first_line(&m, &data).is_some());
        let mut data = vec![b'x'; 3 * CHUNK + 17];
        data.extend_from_slice(b"\nfound the needle here\n");
        assert_eq!(first_line(&m, &data).as_deref(), Some("found the needle here"));
    }

    #[test]
    fn utf8_case_folding_and_multibyte_overlap() {
        let m = TextMatcher::new("äpfel", false, false, false).unwrap();
        assert!(first_line(&m, "GRÜNE ÄPFEL".as_bytes()).is_some());
        // A multi-byte character split across the chunk border is still matched.
        let mut data = vec![b'x'; CHUNK - 1];
        data.extend_from_slice("Äpfel".as_bytes());
        assert!(first_line(&m, &data).is_some());
    }

    #[test]
    fn snippet_is_cut_and_lossy() {
        let m = TextMatcher::new("x", false, true, false).unwrap();
        let long = "x".repeat(500);
        assert_eq!(first_line(&m, long.as_bytes()).unwrap().chars().count(), SNIPPET_CHARS);
        assert_eq!(first_line(&m, b"\xff x").as_deref(), Some("\u{fffd} x"));
    }

    #[test]
    fn empty_stream_never_matches_and_cancel_stops() {
        let m = TextMatcher::new(".*", true, false, false).unwrap();
        assert_eq!(first_line(&m, b""), None);
        let mut buf = Vec::new();
        let out = m.search_with(&mut Cursor::new(b"abc".to_vec()), &mut buf, &|| true).unwrap();
        assert_eq!(out, Outcome::Cancelled);
    }

    #[test]
    fn bounded_buffer() {
        let m = TextMatcher::new("zzz", false, true, false).unwrap();
        let data = vec![b'a'; 5 * CHUNK];
        let mut buf = Vec::new();
        let out = m.search_with(&mut Cursor::new(data), &mut buf, &|| false).unwrap();
        assert_eq!(out, Outcome::NotFound);
        assert!(buf.capacity() <= 2 * (CHUNK + OVERLAP));
    }
}
