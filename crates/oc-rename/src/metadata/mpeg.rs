//! MPEG program streams (.mpg, .mpeg, .vob), transport streams (.ts, .m2ts,
//! .mts) and bare MPEG-1/2 video.
//!
//! Size and frame rate come from the video's own headers (MPEG-1/2 sequence
//! header and extension, H.264 or HEVC sequence parameter set), the length
//! from the first and last presentation time stamps — read from a bounded
//! piece at the start and one at the end of the file, never the whole file.
//! (hachoir, used by the Python version, found nothing in these.)

use std::collections::HashMap;
use std::io;

use super::bytes::{Bits, read_at};
use super::media::VideoInfo;
use super::ReadSeek;

/// How much of the start is read; more only if the video headers weren't in the first piece.
const HEAD: [usize; 2] = [512 << 10, 4 << 20];
/// How much of the end is read for the last time stamp; more if none was in the first piece.
const TAIL: [usize; 2] = [512 << 10, 4 << 20];
/// Video bytes collected from the start for the codec headers.
const MAX_ES: usize = 1 << 20;
/// Time stamps are 33-bit counts of a 90 kHz clock.
const PTS_WRAP: u64 = 1 << 33;
const CLOCK: f64 = 90_000.0;

/// What a video elementary stream's headers say.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct VideoParams {
    pub size: Option<(u64, u64)>,
    pub fps: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Codec {
    Mpeg12,
    H264,
    Hevc,
    /// Try what can be recognised (program streams don't say).
    Unknown,
    /// A video stream whose headers aren't read (MPEG-4 part 2, VC-1).
    Other,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

pub(crate) fn is_program_stream(head: &[u8]) -> bool {
    head.starts_with(&[0, 0, 1, 0xBA])
}

/// Bare MPEG-1/2 video (an .m2v saved as .mpg).
pub(crate) fn is_video_es(head: &[u8]) -> bool {
    head.starts_with(&[0, 0, 1, 0xB3])
}

/// Where the first transport packet's sync byte is and the packet size:
/// 188 bytes, 192 (M2TS: a 4-byte time code before each) or 204 (with
/// Reed-Solomon parity). Several sync bytes in a row must line up.
pub(crate) fn ts_layout(buf: &[u8]) -> Option<(usize, usize)> {
    for size in [188usize, 192, 204] {
        let first = if size == 192 { 4 } else { 0 };
        for start in (first..size.min(buf.len())).chain(0..first) {
            let possible = buf.len().saturating_sub(start).div_ceil(size);
            let need = possible.min(6);
            if need >= 2 && (0..need).all(|k| buf[start + k * size] == 0x47) {
                return Some((start, size));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Video elementary streams
// ---------------------------------------------------------------------------

/// Positions just after each `00 00 01` start code prefix.
fn start_codes(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 <= data.len() {
            if data[i + 2] > 1 {
                i += 3;
            } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                i += 3;
                return Some(i);
            } else {
                i += 1;
            }
        }
        None
    })
}

fn analyse(es: &[u8], codec: Codec) -> Option<VideoParams> {
    match codec {
        Codec::Mpeg12 => mpeg12_sequence(es),
        Codec::H264 => annex_b_sps(es, false),
        Codec::Hevc => annex_b_sps(es, true),
        Codec::Unknown => mpeg12_sequence(es).or_else(|| annex_b_sps(es, false)),
        Codec::Other => None,
    }
}

/// MPEG-1/2 sequence header, refined by the MPEG-2 sequence extension.
fn mpeg12_sequence(es: &[u8]) -> Option<VideoParams> {
    let at = start_codes(es).find(|&p| es.get(p) == Some(&0xB3))?;
    let mut b = Bits::new(es.get(at + 1..)?);
    let (mut w, mut h) = (b.bits(12)?, b.bits(12)?);
    b.skip(4)?; // aspect ratio
    let mut fps = match b.bits(4)? {
        1 => Some(24_000.0 / 1001.0),
        2 => Some(24.0),
        3 => Some(25.0),
        4 => Some(30_000.0 / 1001.0),
        5 => Some(30.0),
        6 => Some(50.0),
        7 => Some(60_000.0 / 1001.0),
        8 => Some(60.0),
        _ => None,
    };
    // The next start code: a sequence extension in MPEG-2 (quantiser matrices may come between).
    let rest = es.get(at + 9..)?;
    if let Some(p) = start_codes(rest).next()
        && rest.get(p) == Some(&0xB5)
        && let Some(ext) = rest.get(p + 1..p + 7)
        && ext[0] >> 4 == 1
    {
        let mut e = Bits::new(ext);
        e.skip(4 + 8 + 1 + 2)?; // id, profile and level, progressive, chroma format
        w |= e.bits(2)? << 12;
        h |= e.bits(2)? << 12;
        e.skip(12 + 1 + 8 + 1)?; // bit rate, marker, VBV buffer, low delay
        let (n, d) = (e.bits(2)?, e.bits(5)?);
        fps = fps.map(|f| f * (n + 1) as f64 / (d + 1) as f64);
    }
    let size = (w > 0 && h > 0).then_some((w as u64, h as u64));
    Some(VideoParams { size, fps })
}

/// The NAL unit after `at` (a start code's end), without emulation prevention bytes.
fn nal_payload(data: &[u8], header: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().min(1024));
    let mut zeros = 0;
    for &byte in data.iter().skip(header).take(4096) {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        if zeros >= 2 && byte <= 1 {
            out.truncate(out.len() - zeros); // the next start code's zeros
            break;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// The first sequence parameter set in an Annex B byte stream.
fn annex_b_sps(es: &[u8], hevc: bool) -> Option<VideoParams> {
    for at in start_codes(es) {
        let Some(&header) = es.get(at) else { break };
        if header & 0x80 != 0 {
            continue;
        }
        let found = if hevc { (header >> 1) & 0x3F == 33 } else { header & 0x1F == 7 };
        if found && let Some(params) = if hevc { hevc_sps(&es[at..]) } else { h264_sps(&es[at..]) } {
            return Some(params);
        }
    }
    None
}

/// An H.264 sequence parameter set NAL unit (header byte first).
pub(crate) fn h264_sps(nal: &[u8]) -> Option<VideoParams> {
    let data = nal_payload(nal, 1);
    let mut b = Bits::new(&data);
    let profile = b.bits(8)?;
    b.skip(16)?; // constraint flags, level
    b.ue()?; // id
    let (mut chroma, mut separate_planes) = (1, false);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = b.ue()?;
        if chroma == 3 {
            separate_planes = b.bit()? == 1;
        }
        b.ue()?; // luma bit depth
        b.ue()?; // chroma bit depth
        b.bit()?; // transform bypass
        if b.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    b.ue()?; // log2 max frame num
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()?.min(256) {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // reference frames
    b.bit()?; // gaps allowed
    let width_mbs = b.ue()? as u64 + 1;
    let height_units = b.ue()? as u64 + 1;
    let frame_mbs_only = b.bit()? as u64;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?; // direct 8x8 inference
    let mut width = width_mbs * 16;
    let mut height = height_units * 16 * (2 - frame_mbs_only);
    if b.bit()? == 1 {
        let (left, right, top, bottom) = (b.ue()? as u64, b.ue()? as u64, b.ue()? as u64, b.ue()? as u64);
        let (unit_x, unit_y) = if separate_planes || chroma == 0 {
            (1, 2 - frame_mbs_only)
        } else {
            (if chroma == 3 { 1 } else { 2 }, if chroma == 1 { 2 } else { 1 } * (2 - frame_mbs_only))
        };
        let (cx, cy) = (unit_x * (left + right), unit_y * (top + bottom));
        if cx < width && cy < height {
            (width, height) = (width - cx, height - cy);
        }
    }
    let size = Some((width, height));
    let fps = if b.bit()? == 1 { vui_frame_rate(&mut b) } else { None };
    Some(VideoParams { size, fps })
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + b.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// The frame rate from H.264 VUI timing information (two fields per frame).
fn vui_frame_rate(b: &mut Bits<'_>) -> Option<f64> {
    if b.bit()? == 1 && b.bits(8)? == 255 {
        b.skip(32)?; // sample aspect ratio
    }
    if b.bit()? == 1 {
        b.bit()?; // overscan appropriate
    }
    if b.bit()? == 1 {
        b.skip(4)?; // video format, full range
        if b.bit()? == 1 {
            b.skip(24)?; // colour description
        }
    }
    if b.bit()? == 1 {
        b.ue()?;
        b.ue()?;
    }
    if b.bit()? == 1 {
        let (units, scale) = (b.bits(32)?, b.bits(32)?);
        if units > 0 && scale > 0 {
            return Some(scale as f64 / (2.0 * units as f64));
        }
    }
    None
}

/// An HEVC sequence parameter set NAL unit (two header bytes first): the size only.
fn hevc_sps(nal: &[u8]) -> Option<VideoParams> {
    let data = nal_payload(nal, 2);
    let mut b = Bits::new(&data);
    b.skip(4)?; // video parameter set id
    let sub_layers = b.bits(3)? as usize;
    b.skip(1)?;
    // profile_tier_level: general profile (88 bits) and level (8)
    b.skip(88 + 8)?;
    let mut present = [(false, false); 8];
    for p in present.iter_mut().take(sub_layers) {
        *p = (b.bit()? == 1, b.bit()? == 1);
    }
    if sub_layers > 0 {
        b.skip(2 * (8 - sub_layers))?;
    }
    for &(profile, level) in present.iter().take(sub_layers) {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    b.ue()?; // id
    let chroma = b.ue()?;
    if chroma == 3 {
        b.bit()?;
    }
    let (mut width, mut height) = (b.ue()? as u64, b.ue()? as u64);
    if b.bit()? == 1 {
        let (left, right, top, bottom) = (b.ue()? as u64, b.ue()? as u64, b.ue()? as u64, b.ue()? as u64);
        let unit_x = if chroma == 1 || chroma == 2 { 2 } else { 1 };
        let unit_y = if chroma == 1 { 2 } else { 1 };
        let (cx, cy) = (unit_x * (left + right), unit_y * (top + bottom));
        if cx < width && cy < height {
            (width, height) = (width - cx, height - cy);
        }
    }
    Some(VideoParams { size: (width > 0 && height > 0).then_some((width, height)), fps: None })
}

// ---------------------------------------------------------------------------
// PES packets (shared by program and transport streams)
// ---------------------------------------------------------------------------

/// A time stamp field: 4 bits prefix, 33 bits with marker bits between.
fn timestamp(b: &[u8]) -> Option<u64> {
    let b = b.get(..5)?;
    if b[0] & 1 == 0 || b[2] & 1 == 0 || b[4] & 1 == 0 {
        return None;
    }
    Some(
        ((b[0] as u64 >> 1) & 7) << 30
            | (b[1] as u64) << 22
            | (b[2] as u64 >> 1) << 15
            | (b[3] as u64) << 7
            | b[4] as u64 >> 1,
    )
}

/// After the 6-byte PES start: the PTS (if any) and the payload.
fn pes_header(stream_id: u8, data: &[u8]) -> Option<(Option<u64>, &[u8])> {
    if matches!(stream_id, 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF) {
        return Some((None, data)); // no header fields
    }
    let first = *data.first()?;
    if first >> 6 == 2 {
        // MPEG-2
        let flags = *data.get(1)?;
        let header_len = *data.get(2)? as usize;
        let pts = if flags & 0x80 != 0 { timestamp(data.get(3..)?) } else { None };
        return Some((pts, data.get(3 + header_len..)?));
    }
    // MPEG-1: stuffing, buffer size, time stamps
    let mut k = 0;
    while data.get(k) == Some(&0xFF) && k < 16 {
        k += 1;
    }
    if data.get(k)? & 0xC0 == 0x40 {
        k += 2;
    }
    let marker = *data.get(k)?;
    let pts = match marker & 0xF0 {
        0x20 => {
            let pts = timestamp(data.get(k..)?);
            k += 5;
            pts
        }
        0x30 => {
            let pts = timestamp(data.get(k..)?);
            k += 10;
            pts
        }
        _ if marker == 0x0F => {
            k += 1;
            None
        }
        _ => return None,
    };
    Some((pts, data.get(k..)?))
}

fn is_video_id(id: u8) -> bool {
    (0xE0..=0xEF).contains(&id)
}

fn is_media_id(id: u8) -> bool {
    is_video_id(id) || (0xC0..=0xDF).contains(&id) || id == 0xBD
}

/// `a - b` on the 33-bit clock, between -2^32 and 2^32.
fn clock_diff(a: u64, b: u64) -> i64 {
    let d = (a.wrapping_sub(b) % PTS_WRAP) as i64;
    if d >= (PTS_WRAP / 2) as i64 { d - PTS_WRAP as i64 } else { d }
}

/// Time stamps seen per stream: the earliest and latest, relative to the
/// first one seen, so a clock wrapping around in between doesn't matter.
#[derive(Default)]
struct Stamps(HashMap<u32, (u64, i64, i64)>);

impl Stamps {
    fn add(&mut self, stream: u32, pts: u64) {
        let pts = pts % PTS_WRAP;
        let e = self.0.entry(stream).or_insert((pts, 0, 0));
        let d = clock_diff(pts, e.0);
        e.1 = e.1.min(d);
        e.2 = e.2.max(d);
    }

    fn first(&self, stream: u32) -> Option<u64> {
        self.0.get(&stream).map(|&(at, min, _)| (at as i64 + min).rem_euclid(PTS_WRAP as i64) as u64)
    }

    fn last(&self, stream: u32) -> Option<u64> {
        self.0.get(&stream).map(|&(at, _, max)| (at as i64 + max).rem_euclid(PTS_WRAP as i64) as u64)
    }
}

/// The length between the first stamps at the start and the last at the end:
/// the video stream's if it has both, otherwise the longest of the others.
fn length(head: &Stamps, tail: &Stamps, video: Option<u32>, fps: Option<f64>) -> Option<f64> {
    let span = |stream: &u32| {
        let (first, last) = (head.first(*stream)?, tail.last(*stream)?);
        Some((last.wrapping_sub(first) % PTS_WRAP) as f64 / CLOCK)
    };
    if let Some(v) = video
        && let Some(s) = span(&v)
    {
        // The last frame lasts one frame too.
        return Some(s + fps.filter(|f| *f > 0.0).map_or(0.0, |f| 1.0 / f));
    }
    head.0.keys().filter_map(span).max_by(f64::total_cmp)
}

// ---------------------------------------------------------------------------
// Program streams
// ---------------------------------------------------------------------------

/// Each PES packet in `buf` (from a pack header or start code on): stream id,
/// PTS, payload (cut short at the end of `buf`).
fn ps_packets(buf: &[u8], mut each: impl FnMut(u8, Option<u64>, &[u8])) {
    let mut i = 0;
    while i + 4 <= buf.len() {
        let Some(at) = start_codes(&buf[i..]).next().map(|p| i + p) else { break };
        let Some(&id) = buf.get(at) else { break };
        match id {
            0xBA => {
                let Some(&mode) = buf.get(at + 1) else { break };
                i = if mode >> 6 == 1 {
                    let Some(&stuffing) = buf.get(at + 10) else { break };
                    at + 11 + (stuffing & 7) as usize // MPEG-2 pack header
                } else if mode >> 4 == 2 {
                    at + 9 // MPEG-1
                } else {
                    at + 1
                };
            }
            0xBB..=0xFF => {
                let Some(len) = buf.get(at + 1..at + 3) else { break };
                let len = u16::from_be_bytes([len[0], len[1]]) as usize;
                let body_start = at + 3;
                let body = &buf[body_start..(body_start + len).min(buf.len())];
                if is_media_id(id)
                    && let Some((pts, payload)) = pes_header(id, body)
                {
                    each(id, pts, payload);
                }
                i = body_start + len.max(1);
            }
            _ => i = at + 1,
        }
    }
}

fn program_stream(r: &mut dyn ReadSeek, len: u64) -> io::Result<VideoInfo> {
    let mut params = None;
    let mut head_stamps = Stamps::default();
    let mut video_id = None;
    for size in HEAD {
        let buf = read_at(r, 0, size)?;
        let mut es = Vec::new();
        head_stamps = Stamps::default();
        ps_packets(&buf, |id, pts, payload| {
            if let Some(pts) = pts {
                head_stamps.add(id as u32, pts);
            }
            if is_video_id(id) && *video_id.get_or_insert(id) == id && es.len() < MAX_ES {
                es.extend_from_slice(&payload[..payload.len().min(MAX_ES - es.len())]);
            }
        });
        params = analyse(&es, Codec::Unknown);
        if params.is_some() || (buf.len() as u64) >= len {
            break;
        }
    }
    let params = params.unwrap_or_default();
    let mut seconds = None;
    for size in TAIL {
        let start = len.saturating_sub(size as u64);
        let buf = read_at(r, start, size)?;
        // From the first pack header, so packets are parsed from their start.
        let from = start_codes(&buf).find(|&p| buf.get(p) == Some(&0xBA)).map_or(buf.len(), |p| p - 3);
        let mut tail_stamps = Stamps::default();
        ps_packets(&buf[from..], |id, pts, _| {
            if let Some(pts) = pts {
                tail_stamps.add(id as u32, pts);
            }
        });
        let video = video_id.map(u32::from);
        seconds = length(&head_stamps, &tail_stamps, video, params.fps);
        let video_found = video.is_none_or(|v| tail_stamps.0.contains_key(&v));
        if (seconds.is_some() && video_found) || start == 0 {
            break;
        }
    }
    Ok(VideoInfo { size: params.size, fps: params.fps, seconds, title: None })
}

// ---------------------------------------------------------------------------
// Transport streams
// ---------------------------------------------------------------------------

/// A 188-byte packet's PID, payload-start flag and payload.
fn ts_packet(p: &[u8]) -> Option<(u16, bool, &[u8])> {
    if p.len() < 188 || p[0] != 0x47 || p[1] & 0x80 != 0 {
        return None; // not in sync, or marked as damaged
    }
    let start = p[1] & 0x40 != 0;
    let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
    let control = (p[3] >> 4) & 3;
    if control & 1 == 0 {
        return None; // no payload
    }
    let offset = if control & 2 != 0 { 5 + p[4] as usize } else { 4 };
    Some((pid, start, p.get(offset..188)?))
}

/// The packets of `buf`, aligned on their sync bytes.
fn ts_packets(buf: &[u8]) -> impl Iterator<Item = (u16, bool, &[u8])> {
    let layout = ts_layout(buf);
    let (start, size) = layout.unwrap_or((buf.len(), 188));
    buf.get(start..).unwrap_or_default().chunks(size).filter_map(ts_packet)
}

/// A PSI section being collected from packets.
#[derive(Default)]
struct Section(Vec<u8>);

impl Section {
    /// Add a packet's payload; the complete section once it's all there.
    fn push(&mut self, start: bool, payload: &[u8]) -> Option<&[u8]> {
        if start {
            let pointer = *payload.first()? as usize;
            self.0 = payload.get(1 + pointer..)?.to_vec();
        } else if !self.0.is_empty() && self.0.len() < 1024 {
            self.0.extend_from_slice(payload);
        }
        let len = 3 + ((*self.0.get(1)? as usize & 0x0F) << 8 | *self.0.get(2)? as usize);
        // Without the CRC.
        (self.0.len() >= len && len >= 12).then(|| &self.0[..len - 4])
    }
}

fn stream_codec(stream_type: u8) -> Option<Codec> {
    Some(match stream_type {
        0x01 | 0x02 => Codec::Mpeg12,
        0x1B => Codec::H264,
        0x24 => Codec::Hevc,
        0x10 | 0xEA => Codec::Other,
        _ => return None,
    })
}

#[derive(Default)]
struct TsHead {
    pmt_pid: Option<u16>,
    /// Elementary stream PIDs from the program map table.
    streams: Vec<u16>,
    video: Option<(u16, Codec)>,
    es: Vec<u8>,
    in_video_pes: bool,
    stamps: Stamps,
    video_stamps: Vec<u64>,
}

impl TsHead {
    fn packet(&mut self, pat: &mut Section, pmt: &mut Section, pid: u16, start: bool, payload: &[u8]) {
        if pid == 0 && self.pmt_pid.is_none() {
            if let Some(s) = pat.push(start, payload) {
                // program number, PID: the first real program (0 is the network table)
                self.pmt_pid = s.get(8..).unwrap_or_default().chunks_exact(4).find_map(|e| {
                    (u16::from_be_bytes([e[0], e[1]]) != 0).then(|| u16::from_be_bytes([e[2] & 0x1F, e[3]]))
                });
            }
            return;
        }
        if Some(pid) == self.pmt_pid && self.streams.is_empty() {
            if let Some(s) = pmt.push(start, payload) {
                self.program_map(s);
            }
            return;
        }
        let known = self.streams.is_empty() || self.streams.contains(&pid);
        if start && known && payload.starts_with(&[0, 0, 1]) && payload.len() >= 6 {
            let id = payload[3];
            if !is_media_id(id) {
                return;
            }
            // No program map (yet): the first video stream id decides.
            if self.video.is_none() && self.streams.is_empty() && is_video_id(id) {
                self.video = Some((pid, Codec::Unknown));
            }
            let Some((pts, data)) = pes_header(id, &payload[6..]) else { return };
            if let Some(pts) = pts {
                self.stamps.add(pid as u32, pts);
            }
            let is_video = self.video.is_some_and(|(v, _)| v == pid);
            if is_video {
                self.in_video_pes = true;
                if let Some(pts) = pts {
                    self.video_stamps.push(pts);
                }
                self.collect(data);
            }
        } else if !start && self.in_video_pes && self.video.is_some_and(|(v, _)| v == pid) {
            self.collect(payload);
        }
    }

    fn collect(&mut self, data: &[u8]) {
        let room = MAX_ES.saturating_sub(self.es.len());
        self.es.extend_from_slice(&data[..data.len().min(room)]);
    }

    fn program_map(&mut self, s: &[u8]) {
        let Some(info_len) = s.get(10..12).map(|b| (b[0] as usize & 0x0F) << 8 | b[1] as usize) else { return };
        let mut at = 12 + info_len;
        while let Some(e) = s.get(at..at + 5) {
            let (kind, pid) = (e[0], u16::from_be_bytes([e[1] & 0x1F, e[2]]));
            self.streams.push(pid);
            if self.video.is_none()
                && let Some(codec) = stream_codec(kind)
            {
                self.video = Some((pid, codec));
            }
            at += 5 + ((e[3] as usize & 0x0F) << 8 | e[4] as usize);
        }
    }

    /// The frame rate from the time stamp steps: the most common one.
    fn stamp_rate(&self) -> Option<f64> {
        let mut stamps = self.video_stamps.clone();
        stamps.sort_unstable();
        stamps.dedup();
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for w in stamps.windows(2) {
            *counts.entry(w[1] - w[0]).or_default() += 1;
        }
        let (&step, &n) = counts.iter().max_by_key(|(step, n)| (**n, std::cmp::Reverse(**step)))?;
        (n >= 4 && step > 0).then(|| CLOCK / step as f64)
    }
}

fn transport_stream(r: &mut dyn ReadSeek, len: u64) -> io::Result<VideoInfo> {
    let mut head = TsHead::default();
    let mut params = None;
    for size in HEAD {
        let buf = read_at(r, 0, size)?;
        if ts_layout(&buf).is_none() {
            return Err(bad("not a transport stream"));
        }
        head = TsHead::default();
        let (mut pat, mut pmt) = (Section::default(), Section::default());
        for (pid, start, payload) in ts_packets(&buf) {
            head.packet(&mut pat, &mut pmt, pid, start, payload);
        }
        params = head.video.and_then(|(_, codec)| analyse(&head.es, codec));
        let video_without_headers = head.video.is_some_and(|(_, c)| c == Codec::Other);
        if params.is_some() || video_without_headers || (buf.len() as u64) >= len {
            break;
        }
    }
    let mut params = params.unwrap_or_default();
    if params.fps.is_none() {
        params.fps = head.stamp_rate();
    }
    let video = head.video.map(|(pid, _)| pid as u32);
    let mut seconds = None;
    for size in TAIL {
        let start = len.saturating_sub(size as u64);
        let buf = read_at(r, start, size)?;
        let mut tail = Stamps::default();
        for (pid, start, payload) in ts_packets(&buf) {
            let known = head.streams.is_empty() || head.streams.contains(&pid);
            if start && known && payload.starts_with(&[0, 0, 1]) && payload.len() >= 6 && is_media_id(payload[3])
                && let Some((Some(pts), _)) = pes_header(payload[3], &payload[6..])
            {
                tail.add(pid as u32, pts);
            }
        }
        seconds = length(&head.stamps, &tail, video, params.fps);
        let video_found = video.is_none_or(|v| tail.0.contains_key(&v));
        if (seconds.is_some() && video_found) || start == 0 {
            break;
        }
    }
    Ok(VideoInfo { size: params.size, fps: params.fps, seconds, title: None })
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub(crate) fn read(r: &mut dyn ReadSeek, len: u64, head: &[u8]) -> io::Result<VideoInfo> {
    if is_program_stream(head) {
        program_stream(r, len)
    } else if is_video_es(head) {
        let buf = read_at(r, 0, HEAD[0])?;
        let params = mpeg12_sequence(&buf).unwrap_or_default();
        Ok(VideoInfo { size: params.size, fps: params.fps, ..VideoInfo::default() })
    } else if ts_layout(head).is_some() {
        transport_stream(r, len)
    } else {
        Err(bad("not an MPEG stream"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An MPEG-2 sequence header (+ extension) for `w`x`h` at frame rate code `rate`.
    fn sequence_header(w: u32, h: u32, rate: u8, ext: Option<(u8, u8)>) -> Vec<u8> {
        let mut out = vec![0, 0, 1, 0xB3];
        out.extend([(w >> 4) as u8, ((w & 0xF) << 4) as u8 | (h >> 8) as u8, h as u8, 0x10 | rate]);
        out.extend([0xFF, 0xFF, 0xE0, 0x00]); // bit rate, marker, vbv, flags
        if let Some((n, d)) = ext {
            // id 1, profile/level, progressive, 4:2:0, no size extension,
            // bit rate, marker, VBV, low delay, frame rate n/d: 48 bits
            let bits = crate::metadata::bytes::tests::pack(&[
                (1, 4),
                (0x48, 8),
                (1, 1),
                (1, 2),
                (0, 4),
                (0, 12),
                (1, 1),
                (0, 8),
                (0, 1),
                (n as u64, 2),
                (d as u64, 5),
            ]);
            out.extend([0, 0, 1, 0xB5]);
            out.extend(&(bits << 16).to_be_bytes()[..6]);
        }
        out.extend([0, 0, 1, 0xB8, 0, 0, 0, 0]); // GOP
        out
    }

    fn pts_bytes(prefix: u8, pts: u64) -> [u8; 5] {
        [
            (prefix << 4) | (((pts >> 30) as u8 & 7) << 1) | 1,
            (pts >> 22) as u8,
            ((pts >> 15) as u8) << 1 | 1,
            (pts >> 7) as u8,
            (pts as u8) << 1 | 1,
        ]
    }

    fn pes(id: u8, pts: Option<u64>, payload: &[u8]) -> Vec<u8> {
        let mut header = vec![0x80, if pts.is_some() { 0x80 } else { 0 }];
        let fields = pts.map(|p| pts_bytes(2, p).to_vec()).unwrap_or_default();
        header.push(fields.len() as u8);
        header.extend(fields);
        let mut out = vec![0, 0, 1, id];
        out.extend(((header.len() + payload.len()) as u16).to_be_bytes());
        out.extend(header);
        out.extend(payload);
        out
    }

    fn pack() -> Vec<u8> {
        vec![0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 1, 0x89, 0xC3, 0xF8]
    }

    #[test]
    fn pts_round_trip() {
        for pts in [0, 1, 90_000, (1 << 33) - 1, 0x1_2345_6789] {
            assert_eq!(timestamp(&pts_bytes(2, pts)), Some(pts));
        }
        assert_eq!(timestamp(&[0x20, 0, 0, 0, 0]), None); // marker bits missing
    }

    #[test]
    fn mpeg2_sequence_with_extension() {
        let es = sequence_header(720, 576, 3, Some((0, 0)));
        assert_eq!(mpeg12_sequence(&es), Some(VideoParams { size: Some((720, 576)), fps: Some(25.0) }));
        let es = sequence_header(1920, 1080, 4, None);
        let got = mpeg12_sequence(&es).unwrap();
        assert_eq!(got.size, Some((1920, 1080)));
        assert_eq!(super::super::format_g(got.fps.unwrap()), "29.97");
        // Frame rate extension: 25 * 2/1 = 50
        assert_eq!(mpeg12_sequence(&sequence_header(16, 16, 3, Some((1, 0)))).unwrap().fps, Some(50.0));
    }

    /// An H.264 SPS for 1920x1080 (68 macroblock rows cropped by 8), 25 fps, main profile.
    fn h264_sps_1080p() -> Vec<u8> {
        let mut bits = String::new();
        let ue = |v: u32| {
            let n = v + 1;
            let len = 32 - n.leading_zeros();
            format!("{}{:b}", "0".repeat(len as usize - 1), n)
        };
        bits += "01001101"; // profile 77
        bits += "0000000000101000"; // constraints, level 40
        bits += &ue(0); // id
        bits += &ue(0); // log2 max frame num - 4
        bits += &ue(2); // poc type 2
        bits += &ue(1); // ref frames
        bits += "0";
        bits += &ue(119); // width 120 mbs
        bits += &ue(67); // height 68 map units
        bits += "1"; // frame mbs only
        bits += "1"; // direct 8x8
        bits += "1"; // cropping
        bits += &ue(0);
        bits += &ue(0);
        bits += &ue(0);
        bits += &ue(4); // bottom: 4 * 2 = 8 lines
        bits += "1"; // vui
        bits += "0000"; // no aspect, overscan, signal type, chroma loc
        bits += "1";
        bits += &format!("{:032b}", 1u32);
        bits += &format!("{:032b}", 50u32);
        bits += "1"; // fixed rate
        bits += "1"; // trailing bit
        bits += &"0".repeat(bits.len().div_ceil(8) * 8 - bits.len());
        let raw: Vec<u8> = (0..bits.len() / 8).map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap()).collect();
        // Emulation prevention, as an encoder writes it.
        let mut nal = vec![0x67];
        let mut zeros = 0;
        for byte in raw {
            if zeros >= 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            nal.push(byte);
        }
        nal
    }

    #[test]
    fn h264_parameters() {
        let sps = h264_sps_1080p();
        assert_eq!(h264_sps(&sps), Some(VideoParams { size: Some((1920, 1080)), fps: Some(25.0) }));
        let mut es = vec![0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1];
        es.extend(&sps);
        es.extend([0, 0, 1, 0x68, 0xCE]);
        assert_eq!(analyse(&es, Codec::H264).unwrap().size, Some((1920, 1080)));
        // Truncated anywhere: no panic.
        for cut in 0..sps.len() {
            let _ = h264_sps(&sps[..cut]);
        }
    }

    #[test]
    fn emulation_prevention_is_removed() {
        assert_eq!(nal_payload(&[0x67, 0, 0, 3, 1, 5, 0, 0, 1, 9], 1), [0, 0, 1, 5]);
    }

    #[test]
    fn program_stream_size_rate_and_length() {
        let mut file = Vec::new();
        let seq = sequence_header(352, 288, 3, Some((0, 0)));
        for frame in 0..100u64 {
            file.extend(pack());
            let payload = if frame == 0 { seq.clone() } else { vec![0, 0, 1, 0, 0xAA, 0xBB] };
            file.extend(pes(0xE0, Some(45_000 + frame * 3600), &payload));
            file.extend(pes(0xC0, Some(44_000 + frame * 3600), &[0xFF, 0xFD, 0, 0]));
        }
        file.extend([0, 0, 1, 0xB9]);
        let len = file.len() as u64;
        let info = read(&mut Cursor::new(file), len, &[0, 0, 1, 0xBA]).unwrap();
        assert_eq!(info.size, Some((352, 288)));
        assert_eq!(info.fps, Some(25.0));
        assert!((info.seconds.unwrap() - 4.0).abs() < 1e-9, "{:?}", info.seconds);
    }

    /// `payload` split into 188-byte packets on `pid`.
    fn ts(pid: u16, payload: &[u8], prefix: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, chunk) in payload.chunks(184).enumerate() {
            if prefix {
                out.extend([0, 0, 0, 0]);
            }
            let start = if i == 0 { 0x40 } else { 0 };
            out.extend([0x47, start | (pid >> 8) as u8, pid as u8]);
            if chunk.len() == 184 {
                out.push(0x10);
                out.extend(chunk);
            } else {
                // adaptation field with stuffing
                out.push(0x30);
                let stuffing = 183 - chunk.len();
                out.push(stuffing as u8);
                if stuffing > 0 {
                    out.push(0);
                    out.extend(std::iter::repeat_n(0xFF, stuffing - 1));
                }
                out.extend(chunk);
            }
        }
        out
    }

    fn psi(table: &[u8]) -> Vec<u8> {
        let mut out = vec![0]; // pointer field
        out.extend(table);
        out.extend([0, 0, 0, 0]); // CRC (not checked)
        out
    }

    fn pat_pmt(video_type: u8) -> (Vec<u8>, Vec<u8>) {
        let pat = psi(&[0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, 0xF0, 0x00]);
        let pmt = psi(&[
            0x02, 0xB0, 23, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0, video_type, 0xE1, 0x00, 0xF0, 0, 0x03, 0xE1, 0x01,
            0xF0, 0,
        ]);
        (pat, pmt)
    }

    fn transport(prefix: bool, video_type: u8, first: &[u8], frames: u64) -> Vec<u8> {
        let (pat, pmt) = pat_pmt(video_type);
        let mut file = [ts(0, &pat, prefix), ts(0x1000, &pmt, prefix)].concat();
        for frame in 0..frames {
            let payload = if frame == 0 { first.to_vec() } else { vec![0x55; 500] };
            file.extend(ts(0x100, &pes(0xE0, Some(126_000 + frame * 3003), &payload), prefix));
            file.extend(ts(0x101, &pes(0xC0, Some(125_000 + frame * 3003), &[0xFF; 50]), prefix));
        }
        file
    }

    #[test]
    fn transport_streams_188_and_192() {
        for prefix in [false, true] {
            let file = transport(prefix, 0x02, &sequence_header(1280, 720, 4, Some((0, 0))), 300);
            let len = file.len() as u64;
            let head = file[..4096].to_vec();
            assert_eq!(ts_layout(&head), Some(if prefix { (4, 192) } else { (0, 188) }));
            let info = read(&mut Cursor::new(file), len, &head).unwrap();
            assert_eq!(info.size, Some((1280, 720)));
            assert_eq!(super::super::format_g(info.fps.unwrap()), "29.97");
            assert!((info.seconds.unwrap() - 300.0 * 3003.0 / 90_000.0).abs() < 1e-6, "{:?}", info.seconds);
        }
    }

    #[test]
    fn transport_stream_h264_and_wrapped_clock() {
        let mut first = vec![0, 0, 0, 1];
        first.extend(h264_sps_1080p());
        let (pat, pmt) = pat_pmt(0x1B);
        let mut file = [ts(0, &pat, false), ts(0x1000, &pmt, false)].concat();
        let start = PTS_WRAP - 90_000; // wraps after one second
        for frame in 0..75u64 {
            let payload = if frame == 0 { first.clone() } else { vec![0x55; 300] };
            let pts = (start + frame * 3600) % PTS_WRAP;
            file.extend(ts(0x100, &pes(0xE0, Some(pts), &payload), false));
        }
        let len = file.len() as u64;
        let info = read(&mut Cursor::new(file.clone()), len, &file[..2048]).unwrap();
        assert_eq!(info.size, Some((1920, 1080)));
        assert_eq!(info.fps, Some(25.0));
        assert!((info.seconds.unwrap() - 3.0).abs() < 1e-9, "{:?}", info.seconds);
    }

    #[test]
    fn rate_from_time_stamps_when_headers_have_none() {
        // MPEG-4 part 2 video: no headers read, the rate comes from the stamps.
        let file = transport(false, 0x10, &[0x55; 100], 50);
        let len = file.len() as u64;
        let info = read(&mut Cursor::new(file.clone()), len, &file[..2048]).unwrap();
        assert_eq!(info.size, None);
        assert_eq!(super::super::format_g(info.fps.unwrap()), "29.97");
    }

    #[test]
    fn hostile_streams() {
        let file = transport(false, 0x02, &sequence_header(720, 480, 4, None), 20);
        for cut in (0..file.len()).step_by(7) {
            let part = file[..cut].to_vec();
            let _ = read(&mut Cursor::new(part.clone()), cut as u64, &part[..cut.min(4096)]);
        }
        let mut ps = Vec::new();
        for i in 0..10u64 {
            ps.extend(pack());
            ps.extend(pes(0xE0, Some(i * 3600), &sequence_header(64, 64, 3, None)));
        }
        for cut in 0..ps.len() {
            let part = ps[..cut].to_vec();
            let _ = read(&mut Cursor::new(part.clone()), cut as u64, &part);
        }
        // Garbage that starts like a pack header, and endless zero-length packets.
        let mut junk = vec![0, 0, 1, 0xBA];
        junk.extend(std::iter::repeat_n([0, 0, 1, 0xE0, 0, 0], 1000).flatten());
        let len = junk.len() as u64;
        assert!(read(&mut Cursor::new(junk.clone()), len, &junk).is_ok());
        // A PMT claiming a huge program info length, and a PAT pointing nowhere.
        let pmt = psi(&[0x02, 0xBF, 0xFF, 0, 1, 0xC1, 0, 0, 0xE1, 0, 0xFF, 0xFF]);
        let mut bad = [ts(0, &pat_pmt(2).0, false), ts(0x1000, &pmt, false)].concat();
        bad.extend(ts(0x100, &pes(0xE0, Some(0), &[1, 2, 3]), false));
        let len = bad.len() as u64;
        assert!(read(&mut Cursor::new(bad.clone()), len, &bad).is_ok());
        assert!(read(&mut Cursor::new(vec![0x47; 10]), 10, &[0x47; 10]).is_err());
    }
}
