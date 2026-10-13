//! Hand-written PDFs for tests (feature `testing`), so no fixture files or
//! PDF writer are needed: blank or painted pages, a title, encryption.

use md5::{Digest, Md5};

/// A page: size in points and an optional filled rectangle
/// (`[x, y, width, height]` in points from the bottom left, RGB 0–1).
#[derive(Clone, Debug, PartialEq)]
pub struct TestPage {
    pub width: f32,
    pub height: f32,
    pub fill: Option<([f32; 4], [f32; 3])>,
}

impl TestPage {
    pub fn blank(width: f32, height: f32) -> Self {
        TestPage { width, height, fill: None }
    }

    /// The left half filled with `rgb`, the right half white.
    pub fn left_half(width: f32, height: f32, rgb: [f32; 3]) -> Self {
        TestPage { width, height, fill: Some(([0.0, 0.0, width / 2.0, height], rgb)) }
    }
}

/// Builds a small, valid PDF 1.4 file.
#[derive(Clone, Debug, Default)]
pub struct TestPdf {
    pub pages: Vec<TestPage>,
    pub title: Option<String>,
    /// Standard security handler, RC4 40-bit (revision 2): opening needs the
    /// user password; an empty one opens without asking.
    pub encrypt: Option<(String, String)>,
    /// Bytes of an unreferenced stream object after the pages: a reader that
    /// only fetches what it needs skips them.
    pub junk: usize,
}

/// `pages` blank pages: A4 portrait, then a landscape-ish one, alternating
/// (like the Python tests' `pdf_bytes`).
pub fn pdf_bytes(pages: usize) -> Vec<u8> {
    TestPdf::with_pages(pages).build()
}

impl TestPdf {
    pub fn with_pages(n: usize) -> Self {
        let pages = (0..n).map(|i| TestPage::blank(595.0, if i % 2 == 0 { 842.0 } else { 420.0 })).collect();
        TestPdf { pages, ..TestPdf::default() }
    }

    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    pub fn page(mut self, page: TestPage) -> Self {
        self.pages.push(page);
        self
    }

    pub fn encrypted(mut self, user_password: &str, owner_password: &str) -> Self {
        self.encrypt = Some((user_password.to_string(), owner_password.to_string()));
        self
    }

    pub fn junk(mut self, bytes: usize) -> Self {
        self.junk = bytes;
        self
    }

    pub fn build(&self) -> Vec<u8> {
        // Objects: 1 catalog, 2 page tree, then per page a page (+ contents),
        // then info and encryption dictionaries.
        let mut objects: Vec<Vec<u8>> = vec![Vec::new(), Vec::new()];
        let mut kids = Vec::new();
        let file_id = Md5::digest(format!("{:?}{:?}", self.pages, self.title).as_bytes()).to_vec();
        let key = self.encrypt.as_ref().map(|(user, owner)| encryption(user, owner, &file_id));
        for page in &self.pages {
            let num = objects.len() + 1;
            kids.push(format!("{num} 0 R"));
            let mut dict =
                format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << >>", page.width, page.height);
            if let Some(([x, y, w, h], [r, g, b])) = page.fill {
                dict.push_str(&format!(" /Contents {} 0 R >>", num + 1));
                objects.push(dict.into_bytes());
                let mut data = format!("{r} {g} {b} rg {x} {y} {w} {h} re f\n").into_bytes();
                if let Some(k) = &key {
                    data = rc4(&object_key(&k.key, num + 1), &data);
                }
                let mut stream = format!("<< /Length {} >>\nstream\n", data.len()).into_bytes();
                stream.extend_from_slice(&data);
                stream.extend_from_slice(b"\nendstream");
                objects.push(stream);
            } else {
                dict.push_str(" >>");
                objects.push(dict.into_bytes());
            }
        }
        if self.junk > 0 {
            let mut stream = format!("<< /Length {} >>\nstream\n", self.junk).into_bytes();
            stream.resize(stream.len() + self.junk, b'#');
            stream.extend_from_slice(b"\nendstream");
            objects.push(stream);
        }
        objects[0] = b"<< /Type /Catalog /Pages 2 0 R >>".to_vec();
        objects[1] = format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), self.pages.len()).into_bytes();
        let info = self.title.as_ref().map(|title| {
            let num = objects.len() + 1;
            let mut text = text_string(title);
            if let Some(k) = &key {
                text = rc4(&object_key(&k.key, num), &text);
            }
            objects.push(format!("<< /Title <{}> >>", hex(&text)).into_bytes());
            num
        });
        let encrypt = key.as_ref().map(|k| {
            objects.push(
                format!("<< /Filter /Standard /V 1 /R 2 /O <{}> /U <{}> /P {} >>", hex(&k.o), hex(&k.u), PERMISSIONS)
                    .into_bytes(),
            );
            objects.len()
        });

        let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for off in offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        let mut trailer = format!("trailer\n<< /Size {} /Root 1 0 R", objects.len() + 1);
        if let Some(n) = info {
            trailer.push_str(&format!(" /Info {n} 0 R"));
        }
        if let Some(n) = encrypt {
            trailer.push_str(&format!(" /Encrypt {n} 0 R"));
        }
        trailer.push_str(&format!(" /ID [<{0}> <{0}>] >>\nstartxref\n{xref}\n%%EOF\n", hex(&file_id)));
        out.extend_from_slice(trailer.as_bytes());
        out
    }
}

/// A PDF text string: PDFDocEncoding for ASCII, UTF-16BE with a BOM otherwise.
fn text_string(s: &str) -> Vec<u8> {
    if s.is_ascii() {
        return s.as_bytes().to_vec();
    }
    let mut out = vec![0xfe, 0xff];
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// -- Standard security handler, revision 2 (PDF 1.7 spec, 7.6.3) -------------------------------

const PAD: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08, 0x2e, 0x2e, 0x00,
    0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];
const PERMISSIONS: i32 = -44;

struct Encryption {
    key: Vec<u8>,
    o: Vec<u8>,
    u: Vec<u8>,
}

fn padded(password: &str) -> Vec<u8> {
    let mut p: Vec<u8> = password.bytes().take(32).collect();
    p.extend_from_slice(&PAD[..32 - p.len()]);
    p
}

fn encryption(user: &str, owner: &str, file_id: &[u8]) -> Encryption {
    // Algorithm 3: the O entry.
    let owner = if owner.is_empty() { user } else { owner };
    let owner_key = &Md5::digest(padded(owner))[..5];
    let o = rc4(owner_key, &padded(user));
    // Algorithm 2: the file key.
    let mut h = Md5::new();
    h.update(padded(user));
    h.update(&o);
    h.update(PERMISSIONS.to_le_bytes());
    h.update(file_id);
    let key = h.finalize()[..5].to_vec();
    // Algorithm 4: the U entry.
    let u = rc4(&key, &PAD);
    Encryption { key, o, u }
}

/// Algorithm 1: the key for one object (generation 0).
fn object_key(key: &[u8], num: usize) -> Vec<u8> {
    let mut h = Md5::new();
    h.update(key);
    h.update(&(num as u32).to_le_bytes()[..3]);
    h.update([0, 0]);
    h.finalize()[..(key.len() + 5).min(16)].to_vec()
}

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: Vec<u8> = (0..=255).collect();
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|&b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[(s[i as usize].wrapping_add(s[j as usize])) as usize]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_matches_the_classic_test_vector() {
        assert_eq!(hex(&rc4(b"Key", b"Plaintext")), "bbf316e8d940af0ad3");
    }

    #[test]
    fn xref_offsets_point_at_objects() {
        let pdf = TestPdf::with_pages(2).title("x").build();
        const HEADER: usize = 15; // "%PDF-1.4\n" and a binary comment; ASCII after it
        let text = std::str::from_utf8(&pdf[HEADER..]).unwrap();
        let xref: usize = text.rsplit("startxref\n").next().unwrap().lines().next().unwrap().parse().unwrap();
        let at = |off: usize| &text[off - HEADER..];
        assert!(at(xref).starts_with("xref\n0 6\n"));
        for (i, line) in at(xref).lines().skip(3).take(5).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(at(off).starts_with(&format!("{} 0 obj", i + 1)), "object {}", i + 1);
        }
    }
}
