//! FLV: the `onMetaData` script tag (AMF0), read like hachoir did — width,
//! height, duration and frame rate. Files without those values get the size
//! from the first video frame (H.264 configuration record or Sorenson H.263
//! header) and the length from the last tag's timestamp.

use std::io;

use super::bytes::{Bits, Cur, read_at};
use super::media::VideoInfo;
use super::mpeg;
use super::ReadSeek;

/// The script tag and the first video frames are near the start.
const HEAD: usize = 1 << 20;
const MAX_TAGS: usize = 256;
const MAX_DEPTH: u32 = 32;

pub(crate) fn is_flv(head: &[u8]) -> bool {
    head.len() >= 9 && head.starts_with(b"FLV") && head[3] == 1
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// The values hachoir took from `onMetaData`.
#[derive(Debug, Default, PartialEq)]
struct ScriptData {
    duration: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
    framerate: Option<f64>,
}

pub(crate) fn read(r: &mut dyn ReadSeek, len: u64) -> io::Result<VideoInfo> {
    let head = read_at(r, 0, HEAD)?;
    if !is_flv(&head) {
        return Err(bad("not an FLV file"));
    }
    let mut c = Cur::new(&head);
    c.skip(5);
    let data_offset = c.u32_be().unwrap_or(9) as usize;
    let mut c = Cur::new(head.get(data_offset..).unwrap_or_default());
    c.skip(4); // PreviousTagSize0

    let mut script: Option<ScriptData> = None;
    let mut frame: Option<VideoInfo> = None;
    for _ in 0..MAX_TAGS {
        let (Some(kind), Some(size), Some(_time), Some(_ext), Some(_stream)) =
            (c.u8(), c.u24_be(), c.u24_be(), c.u8(), c.u24_be())
        else {
            break;
        };
        // A tag cut off by the end of what was read is still parsed as far as it goes.
        let data = c.take((size as usize).min(c.remaining())).unwrap_or_default();
        match kind & 0x1F {
            18 if script.is_none() => script = on_metadata(data),
            9 if frame.is_none() => frame = video_frame(data),
            _ => {}
        }
        let have_size = script.as_ref().is_some_and(|s| s.width.is_some() && s.height.is_some());
        if script.is_some() && (have_size || frame.is_some()) {
            break;
        }
        if c.skip(4).is_none() {
            break; // PreviousTagSize
        }
    }

    let mut info = VideoInfo::default();
    let script = script.unwrap_or_default();
    // hachoir: int() of the numbers.
    let dim = |v: Option<f64>| v.filter(|v| v.is_finite() && *v >= 1.0 && *v < 1e9).map(|v| v.trunc() as u64);
    if let (Some(w), Some(h)) = (dim(script.width), dim(script.height)) {
        info.size = Some((w, h));
    }
    // Live recordings often say 0 (or nothing): then the frames and last tag tell.
    let positive = |v: Option<f64>| v.filter(|v| *v > 0.0);
    info.seconds = positive(script.duration);
    info.fps = positive(script.framerate);
    if let Some(frame) = frame {
        info.size = info.size.or(frame.size);
        info.fps = info.fps.or(frame.fps);
    }
    if info.seconds.is_none() {
        info.seconds = last_timestamp(r, len)?;
    }
    Ok(info)
}

/// The length from the last tag's timestamp (`PreviousTagSize` at the very end points to it).
fn last_timestamp(r: &mut dyn ReadSeek, len: u64) -> io::Result<Option<f64>> {
    if len < 13 + 11 + 4 {
        return Ok(None);
    }
    let tail = read_at(r, len - 4, 4)?;
    let Some(size) = Cur::new(&tail).u32_be() else { return Ok(None) };
    let size = size as u64;
    if size < 11 || size + 4 > len - 13 {
        return Ok(None);
    }
    let tag = read_at(r, len - 4 - size, 11)?;
    let mut c = Cur::new(&tag);
    let (Some(kind), Some(data_size), Some(time), Some(ext)) = (c.u8(), c.u24_be(), c.u24_be(), c.u8()) else {
        return Ok(None);
    };
    if !matches!(kind & 0x1F, 8 | 9 | 18) || data_size as u64 + 11 != size {
        return Ok(None);
    }
    let ms = (((ext as u32) << 24) | time) as f64;
    Ok(Some(ms / 1000.0))
}

/// `onMetaData` followed by an ECMA array (or object) of values.
fn on_metadata(data: &[u8]) -> Option<ScriptData> {
    let mut c = Cur::new(data);
    match value(&mut c, 0)? {
        Amf::Str(name) if name == b"onMetaData" => {}
        _ => return None,
    }
    let mut out = ScriptData::default();
    let mut record = |key: &[u8], v: Amf<'_>| {
        if let Amf::Number(n) = v {
            match key {
                b"duration" => out.duration = Some(n),
                b"width" => out.width = Some(n),
                b"height" => out.height = Some(n),
                b"framerate" => out.framerate = Some(n),
                b"videoframerate" if out.framerate.is_none() => out.framerate = Some(n),
                _ => {}
            }
        }
    };
    // A truncated array still yields the values before the cut.
    match c.u8() {
        Some(8) => {
            if c.u32_be().is_some() {
                let _ = entries(&mut c, 1, &mut record);
            }
        }
        Some(3) => {
            let _ = entries(&mut c, 1, &mut record);
        }
        _ => return None,
    }
    Some(out)
}

#[derive(Debug)]
enum Amf<'a> {
    Number(f64),
    Str(&'a [u8]),
    Other,
}

/// One AMF0 value; `None` on malformed or truncated data.
fn value<'a>(c: &mut Cur<'a>, depth: u32) -> Option<Amf<'a>> {
    if depth > MAX_DEPTH {
        return None;
    }
    Some(match c.u8()? {
        0 => Amf::Number(c.f64_be()?),
        1 => {
            c.u8()?;
            Amf::Other
        }
        2 | 4 => {
            let n = c.u16_be()?;
            Amf::Str(c.take(n as usize)?)
        }
        3 => {
            entries(c, depth + 1, &mut |_, _| {})?;
            Amf::Other
        }
        5 | 6 | 13 => Amf::Other,
        7 => {
            c.u16_be()?;
            Amf::Other
        }
        8 => {
            c.u32_be()?;
            entries(c, depth + 1, &mut |_, _| {})?;
            Amf::Other
        }
        10 => {
            let n = c.u32_be()?;
            // Every value takes at least a byte: the loop ends with the data.
            for _ in 0..n {
                value(c, depth + 1)?;
            }
            Amf::Other
        }
        11 => {
            c.skip(10)?;
            Amf::Other
        }
        12 | 15 => {
            let n = c.u32_be()?;
            c.skip(n as usize)?;
            Amf::Other
        }
        16 => {
            let n = c.u16_be()?;
            c.skip(n as usize)?;
            entries(c, depth + 1, &mut |_, _| {})?;
            Amf::Other
        }
        _ => return None,
    })
}

/// Key/value pairs up to the object end marker (an empty key and 0x09).
fn entries<'a>(c: &mut Cur<'a>, depth: u32, each: &mut dyn FnMut(&[u8], Amf<'a>)) -> Option<()> {
    loop {
        let len = c.u16_be()?;
        if len == 0 {
            let mut peek = *c;
            if peek.u8()? == 9 {
                *c = peek;
                return Some(());
            }
        }
        let key = c.take(len as usize)?;
        let v = value(c, depth)?;
        each(key, v);
    }
}

/// Size (and maybe frame rate) from a video tag's first frame.
fn video_frame(data: &[u8]) -> Option<VideoInfo> {
    let (&first, rest) = data.split_first()?;
    let params = match first & 0x0F {
        // AVC: an AVCDecoderConfigurationRecord with the SPS.
        7 => {
            let mut c = Cur::new(rest);
            if c.u8()? != 0 {
                return None; // not the sequence header
            }
            c.skip(3 + 5)?; // composition time, version, profile, compatibility, level, length size
            let sps_count = c.u8()? & 0x1F;
            if sps_count == 0 {
                return None;
            }
            let len = c.u16_be()?;
            mpeg::h264_sps(c.take(len as usize)?)?
        }
        // Sorenson H.263
        2 => h263(rest)?,
        _ => return None,
    };
    Some(VideoInfo { size: params.size, fps: params.fps, ..VideoInfo::default() })
}

/// Sorenson Spark (FLV's H.263) picture header.
fn h263(data: &[u8]) -> Option<mpeg::VideoParams> {
    let mut b = Bits::new(data);
    if b.bits(17)? != 1 {
        return None;
    }
    b.skip(5 + 8)?; // version, temporal reference
    let size = match b.bits(3)? {
        0 => (b.bits(8)?, b.bits(8)?),
        1 => (b.bits(16)?, b.bits(16)?),
        2 => (352, 288),
        3 => (176, 144),
        4 => (128, 96),
        5 => (320, 240),
        6 => (160, 120),
        _ => return None,
    };
    (size.0 > 0 && size.1 > 0).then_some(mpeg::VideoParams { size: Some((size.0 as u64, size.1 as u64)), fps: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn amf_str(s: &str) -> Vec<u8> {
        let mut out = vec![2];
        out.extend((s.len() as u16).to_be_bytes());
        out.extend(s.as_bytes());
        out
    }

    fn amf_num(key: &str, n: f64) -> Vec<u8> {
        let mut out = (key.len() as u16).to_be_bytes().to_vec();
        out.extend(key.as_bytes());
        out.push(0);
        out.extend(n.to_be_bytes());
        out
    }

    fn tag(kind: u8, time_ms: u32, data: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend(&(data.len() as u32).to_be_bytes()[1..]);
        out.extend(&(time_ms & 0xFF_FFFF).to_be_bytes()[1..]);
        out.push((time_ms >> 24) as u8);
        out.extend([0, 0, 0]);
        out.extend(data);
        out.extend(((data.len() + 11) as u32).to_be_bytes());
        out
    }

    fn flv(tags: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"FLV\x01\x05".to_vec();
        out.extend(9u32.to_be_bytes());
        out.extend(0u32.to_be_bytes());
        out.extend(tags.concat());
        out
    }

    fn metadata(pairs: &[Vec<u8>]) -> Vec<u8> {
        let mut data = amf_str("onMetaData");
        data.push(8);
        data.extend((pairs.len() as u32).to_be_bytes());
        // A nested object and a strict array before the numbers, to be skipped.
        data.extend([0, 1, b'o', 3, 0, 1, b'k', 1, 1, 0, 0, 9]);
        data.extend([0, 1, b'a', 10, 0, 0, 0, 2, 0]);
        data.extend(1.0f64.to_be_bytes());
        data.extend([5]);
        data.extend(pairs.concat());
        data.extend([0, 0, 9]);
        data
    }

    #[test]
    fn script_tag_values() {
        let meta = metadata(&[
            amf_num("duration", 125.5),
            amf_num("width", 1280.0),
            amf_num("height", 720.9),
            amf_num("framerate", 29.97),
        ]);
        let file = flv(&[tag(18, 0, &meta), tag(9, 0, &[0x22, 0, 0])]);
        let info = read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap();
        assert_eq!(info.size, Some((1280, 720))); // int() truncates
        assert_eq!(info.seconds, Some(125.5));
        assert_eq!(info.fps, Some(29.97));
    }

    #[test]
    fn without_metadata_from_frames_and_last_tag() {
        // Sorenson H.263, custom 8-bit size 176x100: start code, version 0, tr 0, size code 0.
        // start code, version, temporal reference, size code (8-bit custom), width, height
        let bits = crate::metadata::bytes::tests::pack(&[(1, 17), (0, 5), (0, 8), (0, 3), (176, 8), (100, 8)]);
        let total = 17 + 5 + 8 + 3 + 16;
        let header = (bits << (64 - total)).to_be_bytes();
        let mut video = vec![0x12];
        video.extend(header);
        let file = flv(&[tag(9, 0, &video), tag(8, 40, &[0x2F, 1, 2]), tag(9, 3_600_250, &[0x22])]);
        let info = read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap();
        assert_eq!(info.size, Some((176, 100)));
        assert_eq!(info.seconds, Some(3600.25));
        assert_eq!(info.fps, None);
    }

    #[test]
    fn zero_duration_in_metadata_falls_back() {
        // Live recordings write duration 0 (and sometimes framerate 0).
        let meta = metadata(&[
            amf_num("duration", 0.0),
            amf_num("width", 320.0),
            amf_num("height", 240.0),
            amf_num("framerate", 0.0),
        ]);
        let file = flv(&[tag(18, 0, &meta), tag(9, 0, &[0x22, 0, 0]), tag(9, 7_040, &[0x22, 0, 0])]);
        let info = read(&mut Cursor::new(file.clone()), file.len() as u64).unwrap();
        assert_eq!(info.size, Some((320, 240)));
        assert_eq!(info.seconds, Some(7.04));
        assert_eq!(info.fps, None);
    }

    #[test]
    fn truncated_and_hostile() {
        let meta = metadata(&[amf_num("width", 640.0), amf_num("height", 480.0), amf_num("duration", 9.0)]);
        let file = flv(&[tag(18, 0, &meta), tag(9, 0, &[0x22, 0, 0])]);
        for cut in 0..file.len() {
            let part = file[..cut].to_vec();
            let _ = read(&mut Cursor::new(part), cut as u64);
        }
        // Values before the cut survive.
        let cut = flv(&[tag(18, 0, &meta)]);
        let cut = &cut[..cut.len() - 17]; // into the duration
        let info = read(&mut Cursor::new(cut.to_vec()), cut.len() as u64).unwrap();
        assert_eq!(info.size, Some((640, 480)));
        // Deep nesting is refused, not recursed into.
        let mut deep = amf_str("onMetaData");
        deep.push(3);
        for _ in 0..10_000 {
            deep.extend([0, 1, b'x', 3]);
        }
        let file = flv(&[tag(18, 0, &deep)]);
        assert!(read(&mut Cursor::new(file.clone()), file.len() as u64).is_ok());
        // A huge strict array count ends with the data.
        let mut arr = amf_str("onMetaData");
        arr.extend([8, 0, 0, 0, 1, 0, 1, b'a', 10, 0xFF, 0xFF, 0xFF, 0xFF, 5, 5, 5]);
        let file = flv(&[tag(18, 0, &arr)]);
        assert!(read(&mut Cursor::new(file.clone()), file.len() as u64).is_ok());
        assert!(read(&mut Cursor::new(b"FLV".to_vec()), 3).is_err());
    }
}
