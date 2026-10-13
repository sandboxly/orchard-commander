//! Metadata: tags, duration and stream facts of an opened input.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use oc_vfs::ReadStream;

use super::ff;
use super::ff::format::stream::Disposition;
use super::ff::media::Type;
use super::io;
use crate::{AudioInfo, MediaError, MediaInfo, VideoInfo};

/// Read a local file's [`MediaInfo`] (opens and probes it; no decoding).
pub fn probe(path: &Path) -> Result<MediaInfo, MediaError> {
    let input = io::open_file(path, Arc::new(AtomicBool::new(false)))?;
    Ok(info_from(&input, true))
}

/// Probing a stream reads at most this much (previews of server files must
/// stay cheap; a file that needs more gets no info, the player still plays it).
pub const PROBE_BUDGET: u64 = 1536 * 1024;

/// A reader that fails once it has delivered its budget.
struct Budget<'a> {
    inner: &'a mut dyn ReadStream,
    left: u64,
}

impl Read for Budget<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other("probe budget exhausted"));
        }
        let n = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..n])?;
        self.left -= n as u64;
        Ok(n)
    }
}

impl Seek for Budget<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Read a stream's [`MediaInfo`], reading at most [`PROBE_BUDGET`] bytes in
/// small requests; a seekable stream is rewound afterwards so it can be
/// handed on to the player.
pub fn probe_stream(stream: &mut dyn ReadStream, size: Option<u64>, name: &str) -> Result<MediaInfo, MediaError> {
    let seekable = stream.seekable();
    let info = {
        let budget = Budget { inner: &mut *stream, left: PROBE_BUDGET };
        let options = io::ReadOptions { read_ahead: 64 * 1024, probesize: Some(PROBE_BUDGET as i64 / 2) };
        let input = io::open_reader(budget, seekable, size, name, Arc::new(AtomicBool::new(false)), options)?;
        info_from(&input, seekable)
    };
    if seekable {
        stream.seek(SeekFrom::Start(0))?;
    }
    Ok(info)
}

/// The streams the player uses: the best audio stream, and the best video
/// stream that isn't cover art.
pub(crate) fn pick_streams(input: &ff::format::context::Input) -> (Option<usize>, Option<usize>) {
    let audio = input.streams().best(Type::Audio).map(|s| s.index());
    let is_picture = |s: &ff::format::stream::Stream| s.disposition().contains(Disposition::ATTACHED_PIC);
    let video = match input.streams().best(Type::Video) {
        Some(s) if !is_picture(&s) => Some(s.index()),
        _ => input
            .streams()
            .find(|s| s.parameters().medium() == Type::Video && !is_picture(s))
            .map(|s| s.index()),
    };
    (audio, video)
}

fn tag(dicts: &[ff::DictionaryRef], key: &str) -> Option<String> {
    dicts.iter().find_map(|d| d.get(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string))
}

fn valid(r: ff::Rational) -> Option<f64> {
    (r.numerator() > 0 && r.denominator() > 0).then(|| f64::from(r))
}

pub(crate) fn info_from(input: &ff::format::context::Input, seekable: bool) -> MediaInfo {
    let (audio, video) = pick_streams(input);
    // Ogg/Opus keep tags on the audio stream.
    let streams: Vec<_> = [audio, video].into_iter().flatten().filter_map(|i| input.stream(i)).collect();
    let mut dicts = vec![input.metadata()];
    dicts.extend(streams.iter().map(|s| s.metadata()));
    let duration = if input.duration() > 0 {
        Some(Duration::from_micros(input.duration() as u64))
    } else {
        input
            .streams()
            .filter_map(|s| (s.duration() > 0).then(|| super::seconds(s.duration(), s.time_base())))
            .filter(|s| s.is_finite() && *s > 0.0)
            .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))))
            .map(Duration::from_secs_f64)
    };
    let video = video.and_then(|i| input.stream(i)).map(|s| {
        // SAFETY: codecpar of a stream of an open input is valid.
        let par = unsafe { &*(*s.as_ptr()).codecpar };
        let (w, h) = (par.width.max(0) as u32, par.height.max(0) as u32);
        let sar = valid(par.sample_aspect_ratio.into()).filter(|r| (0.1..=10.0).contains(r)).unwrap_or(1.0);
        let (dw, dh) = if sar >= 1.0 { ((w as f64 * sar).round() as u32, h) } else { (w, (h as f64 / sar).round() as u32) };
        VideoInfo {
            width: w,
            height: h,
            display_width: dw,
            display_height: dh,
            fps: valid(s.avg_frame_rate()).or_else(|| valid(s.rate())).filter(|f| *f < 1000.0),
            codec: ff::codec::Id::from(par.codec_id).name().to_string(),
        }
    });
    let audio = audio.and_then(|i| input.stream(i)).map(|s| {
        // SAFETY: as above.
        let par = unsafe { &*(*s.as_ptr()).codecpar };
        AudioInfo {
            codec: ff::codec::Id::from(par.codec_id).name().to_string(),
            sample_rate: par.sample_rate.max(0) as u32,
            channels: par.ch_layout.nb_channels.max(0) as u16,
        }
    });
    MediaInfo {
        title: tag(&dicts, "title"),
        artist: tag(&dicts, "artist"),
        album_artist: tag(&dicts, "album_artist"),
        album: tag(&dicts, "album"),
        duration,
        container: input.format().name().to_string(),
        video,
        audio,
        seekable,
    }
}
