//! DSF (DSD audio): the `fmt ` chunk for the audio properties and the ID3v2
//! tag the DSD chunk points to, read like mutagen did.

use std::io;

use super::bytes::{Cur, read_at};
use super::ReadSeek;

/// Bigger tags (huge cover art) are left unread.
const MAX_TAG: u64 = 16 << 20;

#[derive(Debug, Default, PartialEq)]
pub(crate) struct DsfInfo {
    pub channels: u32,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub sample_count: u64,
    /// The whole ID3v2 tag (header included), if there is one.
    pub id3: Option<Vec<u8>>,
}

impl DsfInfo {
    /// Seconds, as mutagen computes them (0 if unknown).
    pub fn length(&self) -> f64 {
        if self.sample_rate == 0 { 0.0 } else { self.sample_count as f64 / self.sample_rate as f64 }
    }

    /// Bits per second (mutagen: rate x bits x channels); crafted values saturate.
    pub fn bitrate(&self) -> u64 {
        (self.sample_rate as u64).saturating_mul(self.bits_per_sample as u64).saturating_mul(self.channels as u64)
    }
}

pub(crate) fn is_dsf(head: &[u8]) -> bool {
    head.starts_with(b"DSD ")
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub(crate) fn read(r: &mut dyn ReadSeek, len: u64) -> io::Result<DsfInfo> {
    let head = read_at(r, 0, 28 + 52)?;
    if !is_dsf(&head) {
        return Err(bad("not a DSF file"));
    }
    let mut c = Cur::new(&head);
    c.skip(4 + 8 + 8);
    let metadata_at = c.u64_le().ok_or_else(|| bad("DSF chunk truncated"))?;
    if c.take(4) != Some(b"fmt ") {
        return Err(bad("DSF fmt header not found"));
    }
    c.skip(8 + 4 + 4 + 4); // size, version, format id, channel type
    let (Some(channels), Some(sample_rate), Some(bits_per_sample), Some(sample_count)) =
        (c.u32_le(), c.u32_le(), c.u32_le(), c.u64_le())
    else {
        return Err(bad("DSF chunk truncated"));
    };
    let mut info = DsfInfo { channels, sample_rate, bits_per_sample, sample_count, id3: None };
    if metadata_at != 0 && metadata_at < len {
        let header = read_at(r, metadata_at, 10)?;
        if header.len() == 10 && header.starts_with(b"ID3") && header[6..10].iter().all(|b| b & 0x80 == 0) {
            let size = header[6..10].iter().fold(0u64, |acc, &b| (acc << 7) | b as u64);
            let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
            let total = 10 + size + footer;
            if total <= MAX_TAG {
                info.id3 = Some(read_at(r, metadata_at, total as usize)?);
            }
        }
    }
    Ok(info)
}

/// The tag as a WAV file's `ID3 ` chunk: lofty reads ID3v2 tags only as
/// part of a file, and this way they go through the same code as MP3s.
pub(crate) fn id3_in_wav(tag: &[u8]) -> Vec<u8> {
    let mut fmt = 1u16.to_le_bytes().to_vec(); // PCM
    fmt.extend(1u16.to_le_bytes()); // mono
    fmt.extend(8000u32.to_le_bytes());
    fmt.extend(16000u32.to_le_bytes());
    fmt.extend(2u16.to_le_bytes());
    fmt.extend(16u16.to_le_bytes());
    let mut body = b"WAVE".to_vec();
    for (id, data) in [(b"fmt ", &fmt[..]), (b"data", &[][..]), (b"ID3 ", tag)] {
        body.extend(id);
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A DSF file: stereo DSD64, `seconds` long (the samples are only claimed), optional tag.
    fn dsf(seconds: u64, tag: Option<&[u8]>) -> Vec<u8> {
        let data_len = 12 + 4096 * 2;
        let tag_at = if tag.is_some() { (28 + 52 + data_len) as u64 } else { 0 };
        let total = 28 + 52 + data_len as u64 + tag.map_or(0, |t| t.len() as u64);
        let mut out = b"DSD ".to_vec();
        out.extend(28u64.to_le_bytes());
        out.extend(total.to_le_bytes());
        out.extend(tag_at.to_le_bytes());
        out.extend(b"fmt ");
        out.extend(52u64.to_le_bytes());
        for v in [1u32, 0, 2, 2, 2_822_400, 1] {
            out.extend(v.to_le_bytes());
        }
        out.extend((2_822_400 * seconds).to_le_bytes());
        out.extend(4096u32.to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(b"data");
        out.extend((data_len as u64).to_le_bytes());
        out.extend(std::iter::repeat_n(0x69, 4096 * 2));
        if let Some(tag) = tag {
            out.extend(tag);
        }
        out
    }

    #[test]
    fn format_and_tag_location() {
        let tag = b"ID3\x04\x00\x00\x00\x00\x00\x05abcde";
        let file = dsf(3, Some(tag));
        let info = read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap();
        assert_eq!((info.channels, info.sample_rate, info.bits_per_sample), (2, 2_822_400, 1));
        assert_eq!(info.length(), 3.0);
        assert_eq!(info.bitrate(), 5_644_800);
        assert_eq!(info.id3.as_deref(), Some(&tag[..]));
    }

    #[test]
    fn hostile_files() {
        let mut huge = b"ID3\x04\x00\x00".to_vec();
        huge.extend([0x7F; 4]); // 256 MB claimed
        let file = dsf(1, Some(&huge));
        let info = read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap();
        assert_eq!(info.id3, None);
        // Points past the end, or at something else.
        let mut file = dsf(1, None);
        file[20..28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap().id3, None);
        file[20..28].copy_from_slice(&40u64.to_le_bytes());
        assert_eq!(read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap().id3, None);
        let file = dsf(1, Some(b"ID3\x04\x00\x00\x00\x00\x01\x00short"));
        for cut in 0..file.len() {
            let _ = read(&mut Cursor::new(file[..cut].to_vec()), cut as u64);
        }
        assert_eq!(DsfInfo::default().length(), 0.0);
        // Crafted format values: no overflow.
        let info = DsfInfo { channels: u32::MAX, sample_rate: u32::MAX, bits_per_sample: u32::MAX, ..DsfInfo::default() };
        assert_eq!(info.bitrate(), u64::MAX);
    }
}
