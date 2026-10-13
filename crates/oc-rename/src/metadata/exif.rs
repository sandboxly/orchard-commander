//! Photo EXIF via kamadak-exif (JPEG, TIFF-based raw formats, HEIF, PNG, WebP).

use std::io::{BufReader, Read, SeekFrom};
use std::sync::LazyLock;

use exif::{In, Tag, Value};

use super::{
    Field, IMAGES, Metadata, MetadataProvider, ProviderError, ReadSeek, Source, clean, dimensions, format_g, has_suffix,
};

/// When and with what a photo was taken.
pub struct PhotoExif;

const FIELDS: &[Field] = &[
    Field::new("taken", "Date taken (2026-03-07)"),
    Field::new("taken_time", "Time taken (09.05.04)"),
    Field::new("taken_year", "Year taken"),
    Field::new("taken_month", "Month taken (03)"),
    Field::new("camera", "Camera make and model"),
    Field::new("make", "Camera make"),
    Field::new("model", "Camera model"),
    Field::new("lens", "Lens"),
    Field::new("iso", "ISO"),
    Field::new("focal", "Focal length in mm"),
    Field::new("width", "Width in pixels"),
    Field::new("height", "Height in pixels"),
    Field::new("resolution", "Width x height"),
];

/// A field's value as text, the way exifread printed it ("" if absent).
fn text(exif: &exif::Exif, tags: &[Tag]) -> String {
    for &tag in tags {
        let Some(field) = exif.get_field(tag, In::PRIMARY) else { continue };
        let value = match &field.value {
            Value::Ascii(parts) => parts.first().map(|p| String::from_utf8_lossy(p).trim_end_matches('\0').to_string()),
            Value::Short(v) => v.first().map(u16::to_string),
            Value::Long(v) => v.first().map(u32::to_string),
            _ => Some(field.display_value().to_string()),
        };
        return value.unwrap_or_default().trim().to_string();
    }
    String::new()
}

fn integer(exif: &exif::Exif, tag: Tag) -> Option<u64> {
    exif.get_field(tag, In::PRIMARY)?.value.get_uint(0).map(u64::from)
}

/// TIFF-based files (DNG, CR2, NEF, ARW, TIFF) keep their EXIF near the start,
/// but kamadak-exif reads them whole: try this much first.
const TIFF_PREFIX: u64 = 1 << 20;

fn read_exif(file: &mut dyn ReadSeek) -> Result<exif::Exif, ProviderError> {
    let reader = exif::Reader::new();
    let mut head = Vec::new();
    (&mut *file).take(TIFF_PREFIX).read_to_end(&mut head)?;
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        let whole = (head.len() as u64) < TIFF_PREFIX;
        match reader.read_raw(head) {
            Ok(exif) => return Ok(exif),
            // Strict parsing fails on offsets past the prefix: read it all.
            Err(e) if whole => return Err(e.into()),
            Err(_) => {}
        }
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(reader.read_from_container(&mut BufReader::new(file))?)
}

impl MetadataProvider for PhotoExif {
    fn id(&self) -> &str {
        "exif"
    }

    fn label(&self) -> &str {
        "Photo (EXIF)"
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn handles(&self, name: &str) -> bool {
        has_suffix(name, IMAGES)
    }

    fn read(&self, src: &mut Source<'_>) -> Result<Metadata, ProviderError> {
        static TAKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r"^(\d{4}):(\d{2}):(\d{2})[ T](\d{2}):(\d{2}):(\d{2})").expect("valid")
        });
        let exif = read_exif(src.file)?;
        let mut out = Metadata::new();
        let mut put = |name: &str, value: String| {
            out.insert(name.to_string(), value);
        };
        let taken = text(&exif, &[Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime]);
        if let Some(c) = TAKEN.captures(&taken) {
            put("taken", format!("{}-{}-{}", &c[1], &c[2], &c[3]));
            put("taken_time", format!("{}.{}.{}", &c[4], &c[5], &c[6]));
            put("taken_year", c[1].to_string());
            put("taken_month", c[2].to_string());
        }
        let make = clean(&text(&exif, &[Tag::Make]));
        let model = clean(&text(&exif, &[Tag::Model]));
        // Many cameras repeat the make in the model ("Canon" + "Canon EOS R6").
        let first_word = make.to_lowercase().split_whitespace().next().unwrap_or("").to_string();
        let camera = if !make.is_empty() && model.to_lowercase().starts_with(&first_word) {
            model.clone()
        } else {
            format!("{make} {model}").trim().to_string()
        };
        put("camera", camera);
        put("make", make);
        put("model", model);
        put("lens", clean(&text(&exif, &[Tag::LensModel])));
        put("iso", text(&exif, &[Tag::PhotographicSensitivity]));
        if let Some(field) = exif.get_field(Tag::FocalLength, In::PRIMARY) {
            match &field.value {
                Value::Rational(v) if !v.is_empty() && v[0].denom != 0 => {
                    put("focal", format_g(v[0].num as f64 / v[0].denom as f64));
                }
                Value::SRational(v) if !v.is_empty() && v[0].denom != 0 => {
                    put("focal", format_g(v[0].num as f64 / v[0].denom as f64));
                }
                _ => {}
            }
        }
        if let (Some(w), Some(h)) = (integer(&exif, Tag::PixelXDimension), integer(&exif, Tag::PixelYDimension)) {
            dimensions(&mut out, w, h);
        }
        out.retain(|_, v| !v.is_empty());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor, Seek};

    /// A little-endian TIFF whose IFD0 holds Make = "Canon" at `make_at`.
    fn tiff(make_at: u32, len: usize) -> Vec<u8> {
        let mut out = b"II*\0".to_vec();
        out.extend(8u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend([0x0F, 0x01, 2, 0]); // Make, ASCII
        out.extend(6u32.to_le_bytes());
        out.extend(make_at.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.resize(len, 0);
        out[make_at as usize..make_at as usize + 6].copy_from_slice(b"Canon\0");
        out
    }

    /// Refuses to be read past `max` bytes.
    struct Limited(Cursor<Vec<u8>>, u64);

    impl Read for Limited {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.position() >= self.1 {
                return Err(io::Error::other("read too far"));
            }
            self.0.read(buf)
        }
    }

    impl Seek for Limited {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.0.seek(pos)
        }
    }

    fn make(file: &mut dyn ReadSeek) -> String {
        let exif = read_exif(file).unwrap();
        text(&exif, &[Tag::Make])
    }

    #[test]
    fn raw_files_are_read_only_at_the_start() {
        let big = tiff(100, 8 << 20);
        assert_eq!(make(&mut Limited(Cursor::new(big), TIFF_PREFIX)), "Canon");
        // A value past the prefix: the whole file after all.
        let far = tiff(3 << 20, 4 << 20);
        assert_eq!(make(&mut Cursor::new(far.clone())), "Canon");
        assert!(read_exif(&mut Limited(Cursor::new(far), TIFF_PREFIX)).is_err());
        // Small files and broken ones.
        assert_eq!(make(&mut Cursor::new(tiff(100, 200))), "Canon");
        assert!(read_exif(&mut Cursor::new(b"II*\0garbage".to_vec())).is_err());
    }
}
