//! ASF (WMV, WMA, ASF): the header object at the start of the file.
//!
//! Read like mutagen (tags, audio properties) and hachoir (video size,
//! length, title) did: File Properties for the length, Stream Properties for
//! the video size and the audio format, Extended Stream Properties (in the
//! Header Extension) for the frame rate, and the four places tags live —
//! Content Description, Extended Content Description, Metadata and Metadata
//! Library — in mutagen's order.

use std::io;

use super::bytes::{Cur, read_at, utf16le};
use super::{Metadata, ReadSeek, clean, duration, is_digits, number, year};

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
}

pub(crate) const HEADER: [u8; 16] = guid(0x75B2_2630, 0x668E, 0x11CF, [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]);
const FILE_PROPERTIES: [u8; 16] =
    guid(0x8CAB_DCA1, 0xA947, 0x11CF, [0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const STREAM_PROPERTIES: [u8; 16] =
    guid(0xB7DC_0791, 0xA9B7, 0x11CF, [0x8E, 0xE6, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const CONTENT_DESCRIPTION: [u8; 16] =
    guid(0x75B2_2633, 0x668E, 0x11CF, [0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]);
const EXTENDED_CONTENT_DESCRIPTION: [u8; 16] =
    guid(0xD2D0_A440, 0xE307, 0x11D2, [0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50]);
const HEADER_EXTENSION: [u8; 16] =
    guid(0x5FBF_03B5, 0xA92E, 0x11CF, [0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65]);
const METADATA: [u8; 16] = guid(0xC5F8_CBEA, 0x5BAF, 0x4877, [0x84, 0x67, 0xAA, 0x8C, 0x44, 0xFA, 0x4C, 0xCA]);
const METADATA_LIBRARY: [u8; 16] =
    guid(0x4423_1C94, 0x9498, 0x49D1, [0xA1, 0x41, 0x1D, 0x13, 0x4E, 0x45, 0x70, 0x54]);
const EXTENDED_STREAM_PROPERTIES: [u8; 16] =
    guid(0x14E6_A5CB, 0xC672, 0x4332, [0x83, 0x99, 0xA9, 0x69, 0x52, 0x06, 0x5B, 0x5A]);
const AUDIO_MEDIA: [u8; 16] = guid(0xF869_9E40, 0x5B4D, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]);
const VIDEO_MEDIA: [u8; 16] = guid(0xBC19_EFC0, 0x5B4D, 0x11CF, [0xA8, 0xFD, 0x00, 0x80, 0x5F, 0x5C, 0x44, 0x2B]);

/// Headers are small, but cover art can make them megabytes; beyond this
/// only the objects that fit are read.
const MAX_HEADER: u64 = 16 << 20;
/// Bounds against crafted counts (each object or attribute is at least a few bytes anyway).
const MAX_OBJECTS: usize = 10_000;

/// An audio stream's format.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AudioFormat {
    pub channels: u16,
    pub sample_rate: u32,
    /// Bits per second.
    pub bitrate: u64,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct AsfInfo {
    /// Play duration minus preroll, unclamped (hachoir filters, mutagen clamps).
    pub seconds: Option<f64>,
    /// The first video stream's size.
    pub video_size: Option<(u64, u64)>,
    pub fps: Option<f64>,
    /// The last audio stream's format (what mutagen reports).
    pub audio: Option<AudioFormat>,
    /// The Content Description title (what hachoir reports as the title).
    pub title: Option<String>,
    /// Attribute name -> value, in mutagen's order; binary values are left out.
    pub tags: Vec<(String, String)>,
}

pub(crate) fn is_asf(head: &[u8]) -> bool {
    head.starts_with(&HEADER)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Parse the header object at the start of `r`.
pub(crate) fn read(r: &mut dyn ReadSeek) -> io::Result<AsfInfo> {
    let head = read_at(r, 0, 30)?;
    if head.len() < 30 || !is_asf(&head) {
        return Err(bad("not an ASF file"));
    }
    let mut c = Cur::new(&head[16..]);
    let size = c.u64_le().unwrap_or(0);
    let count = c.u32_le().unwrap_or(0) as usize;
    if size < 30 {
        return Err(bad("bad ASF header size"));
    }
    let body = read_at(r, 30, (size - 30).min(MAX_HEADER) as usize)?;
    Ok(parse_header(&body, count))
}

#[derive(Default)]
struct Tags {
    content: Vec<(String, String)>,
    extended: Vec<(String, String)>,
    metadata: Vec<(String, String)>,
    library: Vec<(String, String)>,
}

#[derive(Default)]
struct Parsed {
    info: AsfInfo,
    tags: Tags,
    video_stream: Option<u16>,
    /// Stream number -> 100 ns units per frame.
    frame_times: Vec<(u16, u64)>,
}

/// The header's child objects: `(guid, body)`; a truncated last one is cut short.
fn objects(data: &[u8], count: usize) -> impl Iterator<Item = ([u8; 16], &[u8])> {
    let mut c = Cur::new(data);
    (0..count.min(MAX_OBJECTS)).map_while(move |_| {
        let guid: [u8; 16] = c.take(16)?.try_into().ok()?;
        let size = c.u64_le()?;
        if size < 24 {
            return None;
        }
        let body_len = usize::try_from(size - 24).unwrap_or(usize::MAX).min(c.remaining());
        Some((guid, c.take(body_len)?))
    })
}

fn parse_header(data: &[u8], count: usize) -> AsfInfo {
    let mut p = Parsed::default();
    for (guid, body) in objects(data, count) {
        parse_object(&mut p, guid, body, true);
    }
    if let Some(stream) = p.video_stream
        && let Some(&(_, ticks)) = p.frame_times.iter().find(|(n, _)| *n == stream)
        && ticks > 0
    {
        p.info.fps = Some(1e7 / ticks as f64);
    }
    let Tags { content, extended, metadata, library } = p.tags;
    p.info.tags = [content, extended, metadata, library].concat();
    p.info
}

fn parse_object(p: &mut Parsed, guid: [u8; 16], body: &[u8], top: bool) {
    match guid {
        FILE_PROPERTIES => {
            let mut c = Cur::new(body);
            if c.skip(40).is_some()
                && let (Some(play), Some(_send), Some(preroll)) = (c.u64_le(), c.u64_le(), c.u64_le())
            {
                p.info.seconds = Some(play as f64 / 1e7 - preroll as f64 / 1000.0);
            }
        }
        STREAM_PROPERTIES => stream_properties(p, body),
        CONTENT_DESCRIPTION => content_description(&mut p.info, &mut p.tags.content, body),
        EXTENDED_CONTENT_DESCRIPTION => extended_content_description(&mut p.tags.extended, body),
        HEADER_EXTENSION if top => {
            let mut c = Cur::new(body);
            if c.skip(18).is_none() {
                return;
            }
            let Some(size) = c.u32_le() else { return };
            let inner = c.take((size as usize).min(c.remaining())).unwrap_or_default();
            for (guid, body) in objects(inner, MAX_OBJECTS) {
                parse_object(p, guid, body, false);
            }
        }
        METADATA if !top => metadata_records(&mut p.tags.metadata, body),
        METADATA_LIBRARY if !top => metadata_records(&mut p.tags.library, body),
        EXTENDED_STREAM_PROPERTIES if !top => {
            let mut c = Cur::new(body);
            if c.skip(48).is_some()
                && let (Some(stream), Some(_lang), Some(ticks)) = (c.u16_le(), c.u16_le(), c.u64_le())
            {
                p.frame_times.push((stream & 0x7F, ticks));
            }
        }
        _ => {}
    }
}

fn stream_properties(p: &mut Parsed, body: &[u8]) {
    let mut c = Cur::new(body);
    let Some(kind) = c.take(16) else { return };
    // error correction type, time offset, type-specific and error-correction lengths
    if c.skip(16 + 8 + 4 + 4).is_none() {
        return;
    }
    let Some(flags) = c.u16_le() else { return };
    if c.skip(4).is_none() {
        return;
    }
    if kind == AUDIO_MEDIA {
        // WAVEFORMATEX: codec, channels, sample rate, average bytes per second
        if let (Some(_codec), Some(channels), Some(sample_rate), Some(bytes_per_second)) =
            (c.u16_le(), c.u16_le(), c.u32_le(), c.u32_le())
        {
            p.info.audio = Some(AudioFormat { channels, sample_rate, bitrate: bytes_per_second as u64 * 8 });
        }
    } else if kind == VIDEO_MEDIA
        && p.info.video_size.is_none()
        && let (Some(w), Some(h)) = (c.u32_le(), c.u32_le())
        && w > 0
        && h > 0
    {
        p.info.video_size = Some((w as u64, h as u64));
        p.video_stream = Some(flags & 0x7F);
    }
}

fn content_description(info: &mut AsfInfo, tags: &mut Vec<(String, String)>, body: &[u8]) {
    const NAMES: [&str; 5] = ["Title", "Author", "Copyright", "Description", "Rating"];
    let mut c = Cur::new(body);
    let mut lengths = [0usize; 5];
    for len in &mut lengths {
        let Some(n) = c.u16_le() else { return };
        *len = n as usize;
    }
    for (name, len) in NAMES.into_iter().zip(lengths) {
        if len == 0 {
            continue;
        }
        let text = utf16le(c.take(len.min(c.remaining())).unwrap_or_default());
        if name == "Title" && !text.is_empty() {
            info.title = Some(text.clone());
        }
        tags.push((name.to_string(), text));
    }
}

/// A typed attribute value as text, like `str()` of mutagen's attribute;
/// `None` for binary data (byte arrays, GUIDs) and malformed numbers.
fn attribute(kind: u16, data: &[u8], dword_bool: bool) -> Option<String> {
    let mut c = Cur::new(data);
    Some(match kind {
        0 => utf16le(data),
        2 => {
            let set = if dword_bool { c.u32_le()? != 0 } else { c.u16_le()? != 0 };
            if set { "True" } else { "False" }.to_string()
        }
        3 => c.u32_le()?.to_string(),
        4 => c.u64_le()?.to_string(),
        5 => c.u16_le()?.to_string(),
        _ => return None,
    })
}

fn extended_content_description(tags: &mut Vec<(String, String)>, body: &[u8]) {
    let mut c = Cur::new(body);
    let Some(count) = c.u16_le() else { return };
    for _ in 0..count {
        let Some(name_len) = c.u16_le() else { return };
        let Some(name) = c.take(name_len as usize) else { return };
        let (Some(kind), Some(value_len)) = (c.u16_le(), c.u16_le()) else { return };
        let Some(value) = c.take(value_len as usize) else { return };
        if let Some(value) = attribute(kind, value, true) {
            tags.push((utf16le(name), value));
        }
    }
}

/// Metadata and Metadata Library objects (the same layout).
fn metadata_records(tags: &mut Vec<(String, String)>, body: &[u8]) {
    let mut c = Cur::new(body);
    let Some(count) = c.u16_le() else { return };
    for _ in 0..count {
        let (Some(_lang), Some(_stream), Some(name_len), Some(kind), Some(value_len)) =
            (c.u16_le(), c.u16_le(), c.u16_le(), c.u16_le(), c.u32_le())
        else {
            return;
        };
        let Some(name) = c.take(name_len as usize) else { return };
        let Some(value) = c.take(value_len as usize) else { return };
        if let Some(value) = attribute(kind, value, false) {
            tags.push((utf16le(name), value));
        }
    }
}

impl AsfInfo {
    /// mutagen's `tags.get(name)[0]` for the first of `names` present (even if empty).
    fn tag(&self, names: &[&str]) -> Option<&str> {
        names.iter().find_map(|n| self.tags.iter().find(|(k, _)| k == n)).map(|(_, v)| v.as_str())
    }

    /// The audio provider's fields, as the Python version computed them from
    /// mutagen — whose lookups by "easy" names ("title", "date") only matched
    /// what some encoders write — plus the standard WMA attributes
    /// ("Author", "WM/AlbumTitle", …) that it missed.
    pub(crate) fn audio_fields(&self) -> Metadata {
        let mut out = Metadata::new();
        let get = |names: &[&str]| clean(self.tag(names).unwrap_or(""));
        let raw = |names: &[&str]| self.tag(names).map(str::trim);
        for (field, names) in [
            ("artist", &["artist", "Author"][..]),
            ("albumartist", &["albumartist", "WM/AlbumArtist"]),
            ("album", &["album", "WM/AlbumTitle"]),
            ("title", &["title", "Title"]),
            ("genre", &["genre", "WM/Genre"]),
            ("composer", &["composer", "WM/Composer"]),
        ] {
            out.insert(field.into(), get(names));
        }
        let track_raw = match raw(&["tracknumber", "WM/TrackNumber"]) {
            Some(t) => t.to_string(),
            // The older, zero-based track attribute.
            None => match raw(&["WM/Track"]) {
                Some(t) if is_digits(t) => t.parse::<u64>().map_or_else(|_| t.to_string(), |n| (n + 1).to_string()),
                Some(t) => t.to_string(),
                None => String::new(),
            },
        };
        let (track, total) = number(&track_raw);
        let tracktotal = Some(get(&["tracktotal", "totaltracks", "TotalTracks", "WM/TrackTotal"]))
            .filter(|t| !t.is_empty())
            .unwrap_or(total);
        if is_digits(&track) {
            let width = if is_digits(&tracktotal) && tracktotal.parse::<u64>().map_or(true, |n| n >= 100) { 3 } else { 2 };
            out.insert("track".into(), format!("{track:0>width$}"));
        }
        out.insert("tracktotal".into(), tracktotal);
        out.insert("disc".into(), number(raw(&["discnumber", "WM/PartOfSet"]).unwrap_or("")).0);
        out.insert(
            "year".into(),
            year(&get(&["date", "originaldate", "year", "WM/Year", "WM/OriginalReleaseYear"])),
        );
        // mutagen: never negative; zero means unknown.
        let length = self.seconds.unwrap_or(0.0).max(0.0);
        if length > 0.0 {
            out.insert("duration".into(), duration(length));
        }
        if let Some(audio) = self.audio.filter(|a| a.bitrate > 0) {
            let kbps = crate::mask::round_half_even(audio.bitrate as f64 / 1000.0);
            out.insert("bitrate".into(), format!("{kbps}"));
        }
        out.retain(|_, v| !v.is_empty());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn object(guid: [u8; 16], body: &[u8]) -> Vec<u8> {
        let mut out = guid.to_vec();
        out.extend(((body.len() + 24) as u64).to_le_bytes());
        out.extend(body);
        out
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    }

    fn header(children: &[Vec<u8>]) -> Vec<u8> {
        let body = children.concat();
        let mut out = HEADER.to_vec();
        out.extend(((body.len() + 30) as u64).to_le_bytes());
        out.extend((children.len() as u32).to_le_bytes());
        out.extend([1, 2]);
        out.extend(body);
        out
    }

    fn file_properties(play_100ns: u64, preroll_ms: u64) -> Vec<u8> {
        let mut body = vec![0u8; 80];
        body[40..48].copy_from_slice(&play_100ns.to_le_bytes());
        body[56..64].copy_from_slice(&preroll_ms.to_le_bytes());
        object(FILE_PROPERTIES, &body)
    }

    fn stream(kind: [u8; 16], number: u16, specific: &[u8]) -> Vec<u8> {
        let mut body = kind.to_vec();
        body.extend([0; 16 + 8]);
        body.extend((specific.len() as u32).to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(number.to_le_bytes());
        body.extend([0; 4]);
        body.extend(specific);
        object(STREAM_PROPERTIES, &body)
    }

    fn audio_stream(number: u16, channels: u16, rate: u32, bytes_per_second: u32) -> Vec<u8> {
        let mut fmt = 0x161u16.to_le_bytes().to_vec();
        fmt.extend(channels.to_le_bytes());
        fmt.extend(rate.to_le_bytes());
        fmt.extend(bytes_per_second.to_le_bytes());
        fmt.extend([0; 8]);
        stream(AUDIO_MEDIA, number, &fmt)
    }

    fn video_stream(number: u16, w: u32, h: u32) -> Vec<u8> {
        let mut fmt = w.to_le_bytes().to_vec();
        fmt.extend(h.to_le_bytes());
        fmt.extend([2, 40, 0]);
        fmt.extend([0; 40]);
        stream(VIDEO_MEDIA, number, &fmt)
    }

    fn content_description(fields: [&str; 5]) -> Vec<u8> {
        let texts: Vec<Vec<u8>> = fields.iter().map(|s| if s.is_empty() { Vec::new() } else { utf16(s) }).collect();
        let mut body: Vec<u8> = texts.iter().flat_map(|t| (t.len() as u16).to_le_bytes()).collect();
        body.extend(texts.concat());
        object(CONTENT_DESCRIPTION, &body)
    }

    fn extended(attrs: &[(&str, u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = (attrs.len() as u16).to_le_bytes().to_vec();
        for (name, kind, value) in attrs {
            let name = utf16(name);
            body.extend((name.len() as u16).to_le_bytes());
            body.extend(name);
            body.extend(kind.to_le_bytes());
            body.extend((value.len() as u16).to_le_bytes());
            body.extend(value);
        }
        object(EXTENDED_CONTENT_DESCRIPTION, &body)
    }

    fn header_extension(children: &[Vec<u8>]) -> Vec<u8> {
        let inner = children.concat();
        let mut body = vec![0u8; 16];
        body.extend(6u16.to_le_bytes());
        body.extend((inner.len() as u32).to_le_bytes());
        body.extend(inner);
        object(HEADER_EXTENSION, &body)
    }

    fn library(attrs: &[(&str, u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = (attrs.len() as u16).to_le_bytes().to_vec();
        for (name, kind, value) in attrs {
            let name = utf16(name);
            body.extend([0, 0, 0, 0]);
            body.extend((name.len() as u16).to_le_bytes());
            body.extend(kind.to_le_bytes());
            body.extend((value.len() as u32).to_le_bytes());
            body.extend(name);
            body.extend(value);
        }
        object(METADATA_LIBRARY, &body)
    }

    fn ext_stream(number: u16, time_per_frame: u64) -> Vec<u8> {
        let mut body = vec![0u8; 48];
        body.extend(number.to_le_bytes());
        body.extend(0u16.to_le_bytes());
        body.extend(time_per_frame.to_le_bytes());
        body.extend([0; 8]);
        object(EXTENDED_STREAM_PROPERTIES, &body)
    }

    #[test]
    fn wma_tags_like_mutagen_plus_standard_names() {
        let file = header(&[
            file_properties(30_460_000, 46),
            content_description(["Tone: One", "AC/DC", "", "", ""]),
            extended(&[
                ("WM/AlbumTitle", 0, utf16("Live")),
                ("WM/TrackNumber", 3, 3u32.to_le_bytes().to_vec()),
                ("WM/Year", 0, utf16("1992-10-27")),
                ("WM/Picture", 1, vec![1, 2, 3]),
                ("WM/Genre", 0, utf16("Rock")),
                ("TotalTracks", 0, utf16("12")),
            ]),
            header_extension(&[library(&[("WM/Composer", 0, utf16("Bach")), ("WM/PartOfSet", 0, utf16("2/3"))])]),
            audio_stream(1, 2, 44100, 16_000),
        ]);
        let info = read(&mut Cursor::new(file)).unwrap();
        let meta = info.audio_fields();
        let expect: Metadata = [
            ("title", "Tone - One"),
            ("artist", "AC-DC"),
            ("album", "Live"),
            ("track", "03"),
            ("tracktotal", "12"),
            ("disc", "2"),
            ("year", "1992"),
            ("genre", "Rock"),
            ("composer", "Bach"),
            ("duration", "3s"),
            ("bitrate", "128"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(meta, expect);
    }

    #[test]
    fn easy_names_win_even_when_empty() {
        // As mutagen's lookup did: a present but empty "title" stops the search.
        let file = header(&[
            content_description(["Real Title", "", "", "", ""]),
            extended(&[("title", 0, utf16("")), ("WM/Track", 3, 4u32.to_le_bytes().to_vec())]),
        ]);
        let meta = read(&mut Cursor::new(file)).unwrap().audio_fields();
        assert_eq!(meta.get("title"), None);
        assert_eq!(meta["track"], "05"); // WM/Track counts from 0
    }

    #[test]
    fn video_stream_and_frame_rate() {
        let file = header(&[
            file_properties(30_920_000, 46),
            audio_stream(1, 1, 44100, 16_000),
            video_stream(2, 320, 240),
            content_description(["My: Film", "", "", "", ""]),
            header_extension(&[ext_stream(1, 0), ext_stream(2, 400_000)]),
        ]);
        let info = read(&mut Cursor::new(file)).unwrap();
        assert_eq!(info.video_size, Some((320, 240)));
        assert_eq!(info.fps, Some(25.0));
        assert_eq!(info.title.as_deref(), Some("My: Film"));
        assert!((info.seconds.unwrap() - 3.046).abs() < 1e-9);
    }

    #[test]
    fn hostile_headers() {
        // Truncated anywhere: never a panic.
        let file = header(&[
            file_properties(30_000_000, 0),
            content_description(["a", "b", "c", "d", "e"]),
            extended(&[("x", 0, utf16("y")), ("n", 4, 7u64.to_le_bytes().to_vec())]),
            header_extension(&[library(&[("z", 0, utf16("w"))]), ext_stream(1, 1)]),
            video_stream(1, 1, 1),
        ]);
        for cut in 0..file.len() {
            let _ = read(&mut Cursor::new(file[..cut].to_vec()));
        }
        // Absurd sizes and counts: bounded.
        let mut huge = HEADER.to_vec();
        huge.extend(u64::MAX.to_le_bytes());
        huge.extend(u32::MAX.to_le_bytes());
        huge.extend([1, 2]);
        huge.extend(object(FILE_PROPERTIES, &[0; 10]));
        huge.extend(FILE_PROPERTIES);
        huge.extend(u64::MAX.to_le_bytes());
        assert_eq!(read(&mut Cursor::new(huge)).unwrap().seconds, None);
        let mut tiny = HEADER.to_vec();
        tiny.extend(5u64.to_le_bytes());
        tiny.extend([0; 6]);
        assert!(read(&mut Cursor::new(tiny)).is_err());
        // Wrongly sized numbers are skipped, not misread.
        let file = header(&[extended(&[("WM/TrackNumber", 3, vec![1, 2])])]);
        assert!(read(&mut Cursor::new(file)).unwrap().tags.is_empty());
    }
}
