//! Text files for the editor: decode bytes into text and back without
//! changing what the user didn't edit.
//!
//! [`decode`] detects the encoding (byte-order marks, UTF-8, else
//! Windows-1252 / Latin-1) and the line ending style; [`TextFile::encode`]
//! writes the text back the same way. Files with NUL bytes and no UTF-16/32
//! BOM are binary.

use encoding_rs::WINDOWS_1252;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
    Cp1252,
    Latin1,
}

impl Encoding {
    /// Python codec name ("utf-8", "cp1252", …), as stored in settings.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16Le => "utf-16-le",
            Encoding::Utf16Be => "utf-16-be",
            Encoding::Utf32Le => "utf-32-le",
            Encoding::Utf32Be => "utf-32-be",
            Encoding::Cp1252 => "cp1252",
            Encoding::Latin1 => "latin-1",
        }
    }

    /// What the status line shows ("UTF-16 LE", "Windows-1252").
    pub fn display_name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Utf32Le => "UTF-32 LE",
            Encoding::Utf32Be => "UTF-32 BE",
            Encoding::Cp1252 => "Windows-1252",
            Encoding::Latin1 => "Latin-1",
        }
    }

    pub fn bom(self) -> &'static [u8] {
        match self {
            Encoding::Utf8 => b"\xef\xbb\xbf",
            Encoding::Utf16Le => b"\xff\xfe",
            Encoding::Utf16Be => b"\xfe\xff",
            Encoding::Utf32Le => b"\xff\xfe\x00\x00",
            Encoding::Utf32Be => b"\x00\x00\xfe\xff",
            Encoding::Cp1252 | Encoding::Latin1 => b"",
        }
    }
}

/// Longest first: the UTF-32 LE mark starts with the UTF-16 LE one.
const BOMS: [Encoding; 5] = [Encoding::Utf32Le, Encoding::Utf32Be, Encoding::Utf8, Encoding::Utf16Le, Encoding::Utf16Be];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Newline {
    #[default]
    Lf,
    CrLf,
    Cr,
}

impl Newline {
    pub fn as_str(self) -> &'static str {
        match self {
            Newline::Lf => "\n",
            Newline::CrLf => "\r\n",
            Newline::Cr => "\r",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Newline::Lf => "LF",
            Newline::CrLf => "CRLF",
            Newline::Cr => "CR",
        }
    }
}

/// The bytes aren't text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BinaryFile(pub String);

/// The edited text has characters the file's encoding can't store.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("'{encoding}' can't encode character {ch:?}")]
pub struct EncodeError {
    pub encoding: &'static str,
    pub ch: char,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFile {
    /// With "\n" line endings.
    pub text: String,
    pub encoding: Encoding,
    pub bom: bool,
    /// What the file used (the most common one).
    pub newline: Newline,
}

impl TextFile {
    /// "UTF-8 · LF" for the status line.
    pub fn description(&self) -> String {
        format!(
            "{}{} · {}",
            self.encoding.display_name(),
            if self.bom { " with BOM" } else { "" },
            self.newline.label()
        )
    }

    /// `text` (default: self.text) in this file's encoding, BOM and line endings.
    pub fn encode(&self, text: Option<&str>) -> Result<Vec<u8>, EncodeError> {
        let text = text.unwrap_or(&self.text);
        let owned;
        let text = if self.newline == Newline::Lf {
            text
        } else {
            owned = text.replace('\n', self.newline.as_str());
            &owned
        };
        let mut out = Vec::with_capacity(text.len() + 4);
        if self.bom {
            out.extend_from_slice(self.encoding.bom());
        }
        match self.encoding {
            Encoding::Utf8 => out.extend_from_slice(text.as_bytes()),
            Encoding::Utf16Le => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_le_bytes())),
            Encoding::Utf16Be => text.encode_utf16().for_each(|u| out.extend_from_slice(&u.to_be_bytes())),
            Encoding::Utf32Le => text.chars().for_each(|c| out.extend_from_slice(&(c as u32).to_le_bytes())),
            Encoding::Utf32Be => text.chars().for_each(|c| out.extend_from_slice(&(c as u32).to_be_bytes())),
            Encoding::Latin1 => {
                for c in text.chars() {
                    let b = u8::try_from(c as u32).map_err(|_| EncodeError { encoding: "latin-1", ch: c })?;
                    out.push(b);
                }
            }
            Encoding::Cp1252 => {
                let (bytes, _, had_errors) = WINDOWS_1252.encode(text);
                if had_errors {
                    let ch = text
                        .chars()
                        .find(|c| WINDOWS_1252.encode(c.encode_utf8(&mut [0; 4])).2)
                        .unwrap_or('\u{fffd}');
                    return Err(EncodeError { encoding: "cp1252", ch });
                }
                out.extend_from_slice(&bytes);
            }
        }
        Ok(out)
    }
}

/// Bytes Python's cp1252 codec leaves undefined (WHATWG maps them to C1 controls);
/// files containing them are Latin-1, as in the Python version.
const CP1252_UNDEFINED: [u8; 5] = [0x81, 0x8d, 0x8f, 0x90, 0x9d];

/// Detect encoding and line endings. Errors for data that isn't text.
pub fn decode(data: &[u8]) -> Result<TextFile, BinaryFile> {
    for enc in BOMS {
        let bom = enc.bom();
        if let Some(body) = data.strip_prefix(bom) {
            let text = decode_unicode(body, enc).ok_or_else(|| BinaryFile(format!("not valid {}", enc.display_name())))?;
            return Ok(with_newlines(text, enc, true));
        }
    }
    if data.contains(&0) {
        return Err(BinaryFile("contains NUL bytes".into()));
    }
    if let Ok(s) = std::str::from_utf8(data) {
        return Ok(with_newlines(s.to_owned(), Encoding::Utf8, false));
    }
    if data.iter().any(|b| CP1252_UNDEFINED.contains(b)) {
        return Ok(with_newlines(crate::content::latin1(data), Encoding::Latin1, false));
    }
    let (text, _) = WINDOWS_1252.decode_without_bom_handling(data);
    Ok(with_newlines(text.into_owned(), Encoding::Cp1252, false))
}

/// Strict decoding of a BOM'd body; None on any invalid sequence.
fn decode_unicode(body: &[u8], enc: Encoding) -> Option<String> {
    match enc {
        Encoding::Utf8 => std::str::from_utf8(body).ok().map(str::to_owned),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            if !body.len().is_multiple_of(2) {
                return None;
            }
            let units = body.chunks_exact(2).map(|c| {
                let pair = [c[0], c[1]];
                if enc == Encoding::Utf16Le { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) }
            });
            char::decode_utf16(units).collect::<Result<String, _>>().ok()
        }
        Encoding::Utf32Le | Encoding::Utf32Be => {
            if !body.len().is_multiple_of(4) {
                return None;
            }
            body.chunks_exact(4)
                .map(|c| {
                    let quad = [c[0], c[1], c[2], c[3]];
                    char::from_u32(if enc == Encoding::Utf32Le { u32::from_le_bytes(quad) } else { u32::from_be_bytes(quad) })
                })
                .collect()
        }
        Encoding::Cp1252 | Encoding::Latin1 => None,
    }
}

fn with_newlines(text: String, encoding: Encoding, bom: bool) -> TextFile {
    let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                crlf += 1;
                i += 1;
            }
            b'\r' => cr += 1,
            b'\n' => lf += 1,
            _ => {}
        }
        i += 1;
    }
    // Most common wins; ties go to the earlier of LF, CRLF, CR (Python's max()).
    let mut newline = Newline::Lf;
    let mut best = lf;
    if crlf > best {
        newline = Newline::CrLf;
        best = crlf;
    }
    if cr > best {
        newline = Newline::Cr;
    }
    let text = if crlf + cr > 0 { text.replace("\r\n", "\n").replace('\r', "\n") } else { text };
    TextFile { text, encoding, bom, newline }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn decode_cases_round_trip() {
        let cases: Vec<(Vec<u8>, Encoding, bool, Newline, &str)> = vec![
            (b"a\nb\n".to_vec(), Encoding::Utf8, false, Newline::Lf, "a\nb\n"),
            (b"a\r\nb\r\n".to_vec(), Encoding::Utf8, false, Newline::CrLf, "a\nb\n"),
            ("grüße\n".as_bytes().to_vec(), Encoding::Utf8, false, Newline::Lf, "grüße\n"),
            (b"\xef\xbb\xbfhi\n".to_vec(), Encoding::Utf8, true, Newline::Lf, "hi\n"),
            ([b"\xff\xfe".to_vec(), utf16le("hé\n")].concat(), Encoding::Utf16Le, true, Newline::Lf, "hé\n"),
            (b"caf\xe9\n".to_vec(), Encoding::Cp1252, false, Newline::Lf, "café\n"),
            (b"".to_vec(), Encoding::Utf8, false, Newline::Lf, ""),
            (b"a\rb\r".to_vec(), Encoding::Utf8, false, Newline::Cr, "a\nb\n"),
            (b"\x80 \x81\n".to_vec(), Encoding::Latin1, false, Newline::Lf, "\u{80} \u{81}\n"),
            (b"\x80 euro\n".to_vec(), Encoding::Cp1252, false, Newline::Lf, "€ euro\n"),
        ];
        for (data, enc, bom, nl, text) in cases {
            let f = decode(&data).unwrap();
            assert_eq!((f.encoding, f.bom, f.newline, f.text.as_str()), (enc, bom, nl, text), "{data:?}");
            assert_eq!(f.encode(None).unwrap(), data, "round trip of {data:?}");
        }
    }

    #[test]
    fn utf32_and_utf16be_boms() {
        let be: Vec<u8> = [b"\xfe\xff".to_vec(), "x\n".encode_utf16().flat_map(u16::to_be_bytes).collect()].concat();
        let f = decode(&be).unwrap();
        assert_eq!((f.encoding, f.text.as_str()), (Encoding::Utf16Be, "x\n"));
        let le32: Vec<u8> = [b"\xff\xfe\x00\x00".to_vec(), "ä\r\n".chars().flat_map(|c| (c as u32).to_le_bytes()).collect()].concat();
        let f = decode(&le32).unwrap();
        assert_eq!((f.encoding, f.newline, f.text.as_str()), (Encoding::Utf32Le, Newline::CrLf, "ä\n"));
        assert_eq!(f.encode(None).unwrap(), le32);
        assert_eq!(f.description(), "UTF-32 LE with BOM · CRLF");
    }

    #[test]
    fn binary_and_invalid() {
        assert_eq!(decode(&utf16le("hi\r\n")).unwrap_err().0, "contains NUL bytes"); // NUL without a BOM
        assert!(decode(b"\x89PNG\r\n\x1a\n\0\0\0").is_err());
        assert_eq!(decode(b"\xff\xfeA").unwrap_err().0, "not valid UTF-16 LE"); // odd length
        assert_eq!(decode(b"\xef\xbb\xbf\xff").unwrap_err().0, "not valid UTF-8");
        assert_eq!(decode(b"\xff\xfe\x00\xd8\x41\x00").unwrap_err().0, "not valid UTF-16 LE"); // lone surrogate
    }

    #[test]
    fn descriptions() {
        assert_eq!(decode(b"x\r\n").unwrap().description(), "UTF-8 · CRLF");
        assert_eq!(decode(b"\xef\xbb\xbfx").unwrap().description(), "UTF-8 with BOM · LF");
        assert_eq!(decode(b"caf\xe9").unwrap().description(), "Windows-1252 · LF");
    }

    #[test]
    fn newline_majority_and_ties() {
        assert_eq!(decode(b"a\r\nb\r\nc\n").unwrap().newline, Newline::CrLf);
        assert_eq!(decode(b"a\r\nb\n").unwrap().newline, Newline::Lf); // tie: LF first
        assert_eq!(decode(b"a\rb\r\nc").unwrap().newline, Newline::CrLf); // tie CRLF/CR: CRLF
        assert_eq!(decode(b"no newline").unwrap().newline, Newline::Lf);
    }

    #[test]
    fn encoding_edited_text() {
        let f = decode(b"a\r\nb").unwrap();
        assert_eq!(f.encode(Some("x\ny\n")).unwrap(), b"x\r\ny\r\n");
        let latin = decode(b"\x81\xe9").unwrap();
        assert_eq!(latin.encoding, Encoding::Latin1);
        assert_eq!(latin.encode(Some("€")).unwrap_err(), EncodeError { encoding: "latin-1", ch: '€' });
        let cp = decode(b"caf\xe9").unwrap();
        assert_eq!(cp.encode(Some("€")).unwrap(), b"\x80");
        assert_eq!(cp.encode(Some("日")).unwrap_err().ch, '日');
        let u16 = decode(&[b"\xff\xfe".to_vec(), utf16le("a")].concat()).unwrap();
        assert_eq!(u16.encode(Some("😀")).unwrap(), [b"\xff\xfe".to_vec(), utf16le("😀")].concat());
    }
}
