//! Video and image properties from the container / file headers.
//!
//! Small hand-written readers for the headers only (nothing is decoded and
//! big media payloads are skipped by seeking): ISO-BMFF (MP4, MOV, M4V, 3GP),
//! Matroska/WebM, AVI, ASF (WMV), FLV and MPEG program/transport streams for
//! videos; `imagesize` for images. The container is recognised by its
//! content, not the file name.

use std::io::{self, BufReader, SeekFrom};

use super::{
    Field, IMAGES, Metadata, MetadataProvider, ProviderError, ReadSeek, Source, VIDEO, asf, clean, dimensions,
    duration, flv, format_g, has_suffix, mpeg,
};

/// hachoir's limits for plausible values (anything else it left out).
const MAX_SIDE: u64 = 20_000;
const MAX_FPS: f64 = 150.0;
const MIN_SECONDS: f64 = 0.001;
const MAX_SECONDS: f64 = 365.0 * 86_400.0;

/// Width, height, length, frame rate and title of videos; size of images.
pub struct MediaProperties;

const FIELDS: &[Field] = &[
    Field::new("width", "Width in pixels"),
    Field::new("height", "Height in pixels (vertical resolution)"),
    Field::new("resolution", "Width x height (1920x1080)"),
    Field::new("quality", "Height as 1080p / 720p (videos)"),
    Field::new("duration", "Length (1h02m03s)"),
    Field::new("fps", "Frames per second (videos)"),
    Field::new("title", "Title (videos)"),
];

/// What a container header says.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct VideoInfo {
    pub size: Option<(u64, u64)>,
    pub seconds: Option<f64>,
    pub fps: Option<f64>,
    pub title: Option<String>,
}

impl MetadataProvider for MediaProperties {
    fn id(&self) -> &str {
        "media"
    }

    fn label(&self) -> &str {
        "Video and image properties"
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn handles(&self, name: &str) -> bool {
        has_suffix(name, VIDEO) || has_suffix(name, IMAGES)
    }

    fn read(&self, src: &mut Source<'_>) -> Result<Metadata, ProviderError> {
        let is_video = has_suffix(src.name, VIDEO);
        let mut out = Metadata::new();
        if !is_video {
            let size = imagesize::reader_size(BufReader::new(&mut *src.file))?;
            dimensions(&mut out, size.width as u64, size.height as u64);
            return Ok(out);
        }
        // Headers are read in small pieces: buffer them (one read per atom or
        // element instead of a system call or server round trip per field).
        let info = video_info(&mut BufReader::new(&mut *src.file))?;
        let side = |n: &u64| (1..=MAX_SIDE).contains(n);
        if let Some((w, h)) = info.size.filter(|(w, h)| side(w) && side(h)) {
            dimensions(&mut out, w, h);
            out.insert("quality".into(), format!("{h}p"));
        }
        if let Some(s) = info.seconds.filter(|s| (MIN_SECONDS..=MAX_SECONDS).contains(s)) {
            out.insert("duration".into(), duration(s));
        }
        if let Some(fps) = info.fps.filter(|f| (1.0..=MAX_FPS).contains(f)) {
            out.insert("fps".into(), format_g(fps));
        }
        if let Some(title) = info.title {
            out.insert("title".into(), clean(&title));
        }
        out.retain(|_, v| !v.is_empty());
        Ok(out)
    }
}

/// Sniff the container and read its header.
pub(crate) fn video_info(file: &mut dyn ReadSeek) -> io::Result<VideoInfo> {
    let len = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    // Enough for a few transport stream packets.
    let mut head = [0u8; 1024];
    let n = read_up_to(file, &mut head)?;
    let head = &head[..n];
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip") {
        return mp4_info(file, len);
    }
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return mkv_info(file, len);
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return avi_info(file, len);
    }
    if asf::is_asf(head) {
        let asf = asf::read(file)?;
        return Ok(VideoInfo { size: asf.video_size, seconds: asf.seconds, fps: asf.fps, title: asf.title });
    }
    if flv::is_flv(head) {
        return flv::read(file, len);
    }
    if mpeg::is_program_stream(head) || mpeg::is_video_es(head) || mpeg::ts_layout(head).is_some() {
        return mpeg::read(file, len, head);
    }
    Err(io::Error::new(io::ErrorKind::Unsupported, "unknown video container"))
}

fn read_up_to(r: &mut dyn ReadSeek, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn u32_be(r: &mut dyn ReadSeek) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn u64_be(r: &mut dyn ReadSeek) -> io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn skip(r: &mut dyn ReadSeek, n: i64) -> io::Result<()> {
    r.seek(SeekFrom::Current(n)).map(|_| ())
}

// ---------------------------------------------------------------------------
// ISO base media (MP4, MOV)
// ---------------------------------------------------------------------------

struct Atom {
    kind: [u8; 4],
    body: u64,
    end: u64,
}

/// The atoms in `start..end`, read lazily (bodies are skipped by seeking).
fn atoms(r: &mut dyn ReadSeek, start: u64, end: u64, mut each: impl FnMut(&mut dyn ReadSeek, &Atom) -> io::Result<bool>) -> io::Result<()> {
    let mut at = start;
    // A bound on the number of atoms guards against crafted loops of tiny boxes.
    for _ in 0..100_000 {
        if at.saturating_add(8) > end {
            break;
        }
        r.seek(SeekFrom::Start(at))?;
        let size = u32_be(r)? as u64;
        let mut kind = [0; 4];
        r.read_exact(&mut kind)?;
        let (header, size) = match size {
            0 => (8, end - at),
            1 => (16, u64_be(r)?),
            n => (8, n),
        };
        if size < header {
            return Err(bad("bad atom size"));
        }
        let atom = Atom { kind, body: at + header, end: (at.saturating_add(size)).min(end) };
        if !each(r, &atom)? {
            break;
        }
        at = at.saturating_add(size);
    }
    Ok(())
}

#[derive(Default)]
struct Track {
    video: bool,
    size: (u64, u64),
    timescale: u32,
    samples: u64,
    ticks: u64,
}

fn mp4_info(r: &mut dyn ReadSeek, len: u64) -> io::Result<VideoInfo> {
    let mut moov = None;
    atoms(r, 0, len, |_, a| {
        if &a.kind == b"moov" {
            moov = Some((a.body, a.end));
            return Ok(false);
        }
        Ok(true)
    })?;
    let (start, end) = moov.ok_or_else(|| bad("no moov atom"))?;
    let mut info = VideoInfo::default();
    let mut video: Option<Track> = None;
    atoms(r, start, end, |r, a| {
        match &a.kind {
            b"mvhd" => {
                let version = u32_be(r)? >> 24;
                let (timescale, length) = if version == 1 {
                    skip(r, 16)?;
                    (u32_be(r)?, u64_be(r)?)
                } else {
                    skip(r, 8)?;
                    (u32_be(r)?, u32_be(r)? as u64)
                };
                let unknown = length == u32::MAX as u64 || length == u64::MAX;
                if timescale > 0 && !unknown {
                    info.seconds = Some(length as f64 / timescale as f64);
                }
            }
            b"trak" if video.is_none() => {
                let track = mp4_track(r, a.body, a.end, 0)?;
                if track.video && track.size.0 > 0 && track.size.1 > 0 {
                    video = Some(track);
                }
            }
            _ => {}
        }
        Ok(true)
    })?;
    if let Some(t) = video {
        info.size = Some(t.size);
        if t.ticks > 0 && t.timescale > 0 {
            info.fps = Some(t.samples as f64 * t.timescale as f64 / t.ticks as f64);
        }
    }
    Ok(info)
}

/// The track in `start..end`; `depth` counts nested mdia/minf/stbl boxes,
/// so a crafted file can't recurse until the stack overflows (an abort that
/// no panic guard catches).
fn mp4_track(r: &mut dyn ReadSeek, start: u64, end: u64, depth: u32) -> io::Result<Track> {
    let mut track = Track::default();
    if depth > 8 {
        return Ok(track);
    }
    atoms(r, start, end, |r, a| {
        match &a.kind {
            b"tkhd" => {
                let version = u32_be(r)? >> 24;
                // times, track id, reserved, duration, reserved, layer, group, volume, reserved, matrix
                skip(r, if version == 1 { 32 } else { 20 } + 8 + 8 + 36)?;
                track.size = ((u32_be(r)? >> 16) as u64, (u32_be(r)? >> 16) as u64);
            }
            b"mdia" | b"minf" | b"stbl" => {
                let inner = mp4_track(r, a.body, a.end, depth + 1)?;
                track.video |= inner.video;
                if inner.timescale > 0 {
                    track.timescale = inner.timescale;
                }
                if inner.ticks > 0 {
                    (track.samples, track.ticks) = (inner.samples, inner.ticks);
                }
            }
            b"hdlr" => {
                skip(r, 8)?;
                let mut handler = [0; 4];
                r.read_exact(&mut handler)?;
                track.video = &handler == b"vide";
            }
            b"mdhd" => {
                let version = u32_be(r)? >> 24;
                skip(r, if version == 1 { 16 } else { 8 })?;
                track.timescale = u32_be(r)?;
            }
            b"stts" => {
                skip(r, 4)?;
                let entries = u32_be(r)?.min(1_000_000);
                for _ in 0..entries {
                    let count = u32_be(r)? as u64;
                    let delta = u32_be(r)? as u64;
                    track.samples = track.samples.saturating_add(count);
                    track.ticks = track.ticks.saturating_add(count * delta);
                }
            }
            _ => {}
        }
        Ok(true)
    })?;
    Ok(track)
}

// ---------------------------------------------------------------------------
// Matroska / WebM
// ---------------------------------------------------------------------------

const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_TYPE: u32 = 0x83;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO_SETTINGS: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const TIMECODE_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TITLE: u32 = 0x7BA9;

fn byte(r: &mut dyn ReadSeek) -> io::Result<u8> {
    let mut b = [0];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

/// An element ID (marker bits kept, as IDs are written in the spec).
fn ebml_id(r: &mut dyn ReadSeek) -> io::Result<u32> {
    let first = byte(r)?;
    let len = first.leading_zeros() + 1;
    if len > 4 {
        return Err(bad("bad element id"));
    }
    let mut id = first as u32;
    for _ in 1..len {
        id = (id << 8) | byte(r)? as u32;
    }
    Ok(id)
}

/// An element size; None for "unknown" (live streams).
fn ebml_size(r: &mut dyn ReadSeek) -> io::Result<Option<u64>> {
    let first = byte(r)?;
    let len = first.leading_zeros() + 1;
    if len > 8 {
        return Err(bad("bad element size"));
    }
    let mut value = (first as u64) & (0xFF >> len);
    let mut all_ones = value == (0xFF >> len) as u64;
    for _ in 1..len {
        let b = byte(r)?;
        all_ones &= b == 0xFF;
        value = (value << 8) | b as u64;
    }
    Ok(if all_ones { None } else { Some(value) })
}

fn ebml_uint(r: &mut dyn ReadSeek, size: u64) -> io::Result<u64> {
    if size > 8 {
        return Err(bad("integer too long"));
    }
    (0..size).try_fold(0u64, |acc, _| Ok((acc << 8) | byte(r)? as u64))
}

fn ebml_float(r: &mut dyn ReadSeek, size: u64) -> io::Result<f64> {
    Ok(match size {
        4 => f32::from_bits(ebml_uint(r, 4)? as u32) as f64,
        8 => f64::from_bits(ebml_uint(r, 8)?),
        _ => return Err(bad("bad float size")),
    })
}

/// The elements in `start..end`: `each(reader, id, body start, body end)`
/// returns whether to go on. An element of unknown size runs to `end` and
/// is the last one visited.
fn elements(
    r: &mut dyn ReadSeek,
    start: u64,
    end: u64,
    mut each: impl FnMut(&mut dyn ReadSeek, u32, u64, u64) -> io::Result<bool>,
) -> io::Result<()> {
    let mut at = start;
    for _ in 0..100_000 {
        if at >= end {
            break;
        }
        r.seek(SeekFrom::Start(at))?;
        let id = ebml_id(r)?;
        let size = ebml_size(r)?;
        let body = r.stream_position()?;
        if body > end {
            break; // the element's header runs past its parent
        }
        let body_end = size.map_or(end, |s| body.saturating_add(s).min(end));
        if !each(r, id, body, body_end)? || size.is_none() {
            break;
        }
        at = body_end;
    }
    Ok(())
}

fn mkv_info(r: &mut dyn ReadSeek, len: u64) -> io::Result<VideoInfo> {
    let mut segment = None;
    elements(r, 0, len, |_, id, body, end| {
        if id == SEGMENT {
            segment = Some((body, end));
            return Ok(false);
        }
        Ok(true)
    })?;
    let (start, end) = segment.ok_or_else(|| bad("no segment"))?;
    let mut info = VideoInfo::default();
    let (mut scale, mut raw_duration) = (1_000_000u64, None);
    let (mut have_info, mut have_tracks) = (false, false);
    elements(r, start, end, |r, id, body, end| {
        match id {
            INFO => {
                have_info = true;
                elements(r, body, end, |r, id, body, end| {
                    let size = end - body;
                    match id {
                        TIMECODE_SCALE => scale = ebml_uint(r, size)?,
                        DURATION => raw_duration = Some(ebml_float(r, size)?),
                        TITLE if size <= 4096 => {
                            let mut text = vec![0; size as usize];
                            r.read_exact(&mut text)?;
                            let title = String::from_utf8_lossy(&text).trim_end_matches('\0').to_string();
                            info.title = Some(title).filter(|t| !t.is_empty());
                        }
                        _ => {}
                    }
                    Ok(true)
                })?;
            }
            TRACKS => {
                have_tracks = true;
                elements(r, body, end, |r, id, body, end| {
                    if id == TRACK_ENTRY && info.size.is_none() {
                        mkv_track(r, body, end, &mut info)?;
                    }
                    Ok(true)
                })?;
            }
            // Media data: the header is over once both parts were seen.
            CLUSTER if have_info && have_tracks => return Ok(false),
            _ => {}
        }
        Ok(true)
    })?;
    if let Some(d) = raw_duration {
        info.seconds = Some(d * scale as f64 / 1e9);
    }
    Ok(info)
}

fn mkv_track(r: &mut dyn ReadSeek, start: u64, end: u64, info: &mut VideoInfo) -> io::Result<()> {
    let (mut is_video, mut size, mut frame_ns) = (false, (0, 0), 0);
    elements(r, start, end, |r, id, body, end| {
        let len = end - body;
        match id {
            TRACK_TYPE => is_video = ebml_uint(r, len)? == 1,
            DEFAULT_DURATION => frame_ns = ebml_uint(r, len)?,
            VIDEO_SETTINGS => elements(r, body, end, |r, id, body, end| {
                match id {
                    PIXEL_WIDTH => size.0 = ebml_uint(r, end - body)?,
                    PIXEL_HEIGHT => size.1 = ebml_uint(r, end - body)?,
                    _ => {}
                }
                Ok(true)
            })?,
            _ => {}
        }
        Ok(true)
    })?;
    if is_video && size.0 > 0 && size.1 > 0 {
        info.size = Some(size);
        if frame_ns > 0 {
            info.fps = Some(1e9 / frame_ns as f64);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// AVI
// ---------------------------------------------------------------------------

fn avi_info(r: &mut dyn ReadSeek, len: u64) -> io::Result<VideoInfo> {
    // RIFF "AVI " -> LIST "hdrl" -> "avih" (the main header) comes first.
    let mut at = 12u64;
    while at + 8 <= len.min(1 << 20) {
        r.seek(SeekFrom::Start(at))?;
        let mut head = [0u8; 12];
        r.read_exact(&mut head[..8])?;
        let size = u32_le(&head[4..8]) as u64;
        if &head[..4] == b"LIST" {
            r.read_exact(&mut head[8..12])?;
            if &head[8..12] == b"hdrl" {
                let mut avih = [0u8; 48];
                r.read_exact(&mut avih[..8])?;
                if &avih[..4] != b"avih" {
                    return Err(bad("no avih header"));
                }
                r.read_exact(&mut avih[..40])?;
                let us_per_frame = u32_le(&avih[0..4]) as f64;
                let frames = u32_le(&avih[16..20]) as f64;
                let (w, h) = (u32_le(&avih[32..36]) as u64, u32_le(&avih[36..40]) as u64);
                let mut info = VideoInfo::default();
                if w > 0 && h > 0 {
                    info.size = Some((w, h));
                }
                if us_per_frame > 0.0 {
                    info.fps = Some(1e6 / us_per_frame);
                    info.seconds = Some(frames * us_per_frame / 1e6);
                }
                return Ok(info);
            }
        }
        at += 8 + size + (size & 1);
    }
    Err(bad("no hdrl list"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.push(0x80 | body.len() as u8); // one-byte size (< 127)
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn matroska_header() {
        let title = ebml(&[0x7B, 0xA9], b"My: Film");
        let scale = ebml(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]); // 1 ms
        let dur = ebml(&[0x44, 0x89], &125_500.0f64.to_bits().to_be_bytes()); // 125.5 s
        let info = ebml(&[0x15, 0x49, 0xA9, 0x66], &[scale, dur, title].concat());
        let video = ebml(&[0xE0], &[ebml(&[0xB0], &[0x07, 0x80]), ebml(&[0xBA], &[0x04, 0x38])].concat());
        let audio_track = ebml(&[0xAE], &ebml(&[0x83], &[2]));
        let video_track = ebml(
            &[0xAE],
            &[ebml(&[0x83], &[1]), ebml(&[0x23, 0xE3, 0x83], &40_000_000u32.to_be_bytes()), video].concat(),
        );
        let tracks = ebml(&[0x16, 0x54, 0xAE, 0x6B], &[audio_track, video_track].concat());
        let cluster = ebml(&[0x1F, 0x43, 0xB6, 0x75], &[0; 10]);
        let mut segment = vec![0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]; // unknown size
        segment.extend([info, tracks, cluster].concat());
        let file = [ebml(&[0x1A, 0x45, 0xDF, 0xA3], &ebml(&[0x42, 0x82], b"webm")), segment].concat();
        let got = video_info(&mut Cursor::new(file)).unwrap();
        assert_eq!(got.size, Some((1920, 1080)));
        assert_eq!(got.seconds, Some(125.5));
        assert_eq!(got.fps, Some(25.0));
        assert_eq!(got.title.as_deref(), Some("My: Film"));
    }

    #[test]
    fn avi_header() {
        let mut avih = vec![0u8; 56];
        avih[0..4].copy_from_slice(&33_367u32.to_le_bytes()); // ~29.97 fps
        avih[16..20].copy_from_slice(&300u32.to_le_bytes());
        avih[32..36].copy_from_slice(&640u32.to_le_bytes());
        avih[36..40].copy_from_slice(&480u32.to_le_bytes());
        let mut hdrl = b"hdrlavih".to_vec();
        hdrl.extend(56u32.to_le_bytes());
        hdrl.extend(avih);
        let mut list = b"LIST".to_vec();
        list.extend((hdrl.len() as u32).to_le_bytes());
        list.extend(hdrl);
        let mut file = b"RIFF".to_vec();
        file.extend(((list.len() + 4) as u32).to_le_bytes());
        file.extend(b"AVI ");
        file.extend(list);
        let got = video_info(&mut Cursor::new(file)).unwrap();
        assert_eq!(got.size, Some((640, 480)));
        assert_eq!(format_g(got.fps.unwrap()), "29.9697");
        assert_eq!(duration(got.seconds.unwrap()), "10s");
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(body);
        out
    }

    #[test]
    fn hostile_mp4_boxes() {
        // Deeply nested boxes: bounded recursion instead of a stack overflow.
        let mut inner = mp4_box(b"hdlr", &[0; 12]);
        for _ in 0..20_000 {
            inner = mp4_box(b"mdia", &inner);
        }
        let file = [mp4_box(b"ftyp", b"isom"), mp4_box(b"moov", &mp4_box(b"trak", &inner))].concat();
        assert_eq!(video_info(&mut Cursor::new(file)).unwrap(), VideoInfo::default());
        // Sample counts that overflow, and a box claiming nearly 2^64 bytes.
        let mut stts = vec![0; 4];
        stts.extend(2u32.to_be_bytes());
        stts.extend([0xFF; 16]);
        let stbl = mp4_box(b"stbl", &mp4_box(b"stts", &stts));
        let file = [mp4_box(b"ftyp", b"isom"), mp4_box(b"moov", &mp4_box(b"trak", &stbl))].concat();
        assert!(video_info(&mut Cursor::new(file)).is_ok());
        let mut huge = vec![0, 0, 0, 1];
        huge.extend(b"ftyp");
        huge.extend(u64::MAX.to_be_bytes());
        assert!(video_info(&mut Cursor::new(huge)).is_err()); // no moov
    }

    #[test]
    fn matroska_element_header_past_its_parent() {
        // Info is 2 bytes long, but its child's header needs 3.
        let mut segment = vec![0x18, 0x53, 0x80, 0x67, 0xFF]; // unknown size
        segment.extend([0x15, 0x49, 0xA9, 0x66, 0x82, 0x7B, 0xA9, 0x81, b'x']);
        let file = [ebml(&[0x1A, 0x45, 0xDF, 0xA3], &ebml(&[0x42, 0x82], b"webm")), segment].concat();
        let _ = video_info(&mut Cursor::new(file)); // no panic
    }

    #[test]
    fn unknown_and_truncated() {
        assert!(video_info(&mut Cursor::new(b"not a video at all".to_vec())).is_err());
        assert!(video_info(&mut Cursor::new(Vec::new())).is_err());
        let mut truncated = 100u32.to_be_bytes().to_vec();
        truncated.extend(b"ftypisom");
        assert!(video_info(&mut Cursor::new(truncated)).is_err()); // no moov
    }
}
