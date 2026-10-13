//! Video frames for the UI: scaled to the size it asked for and converted to
//! RGBA on the decoder thread, queued with their presentation times, and
//! shown when the clock reaches them.
//!
//! Presentation is lazy: whoever looks at the queue (the UI on redraw, the
//! decoder before queueing more) advances it to the current clock, so frames
//! are never shown early and a hidden UI never stalls decoding.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::ffmpeg::ff;

/// One decoded frame: straight RGBA8, rows of `width * 4` bytes.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// When it is shown, from the start of the file.
    pub pts: Duration,
    /// Increases with every frame of a player: upload when it changes.
    pub serial: u64,
}

impl fmt::Debug for VideoFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VideoFrame({}×{} at {:?}, #{})", self.width, self.height, self.pts, self.serial)
    }
}

/// Decoded frames ahead of the clock (at most this many while audio is
/// buffered well; the decoder waits then)…
pub(crate) const AHEAD: usize = 8;
/// … and at most this many bytes of them (big frames: fewer).
const AHEAD_BYTES: usize = 64 << 20;
/// Never more than this, whatever happens (audio starved by interleaving)…
const HARD_LIMIT: usize = 32;
/// … or this many bytes.
const HARD_LIMIT_BYTES: usize = 256 << 20;
/// Buffers kept for reuse.
const POOL: usize = 4;

#[derive(Default)]
pub(crate) struct FrameQueue {
    pending: VecDeque<Arc<VideoFrame>>,
    pub(crate) current: Option<Arc<VideoFrame>>,
    /// Show the next frame right away (after opening or seeking, also paused).
    pub(crate) preroll: bool,
    serial: u64,
    pool: Vec<Vec<u8>>,
    /// Size of the last frame queued.
    frame_bytes: usize,
}

impl FrameQueue {
    pub(crate) fn new() -> Self {
        FrameQueue { preroll: true, ..Self::default() }
    }

    pub(crate) fn len(&self) -> usize {
        self.pending.len()
    }

    /// How many frames to decode ahead (fewer when they are big).
    pub(crate) fn ahead(&self) -> usize {
        (AHEAD_BYTES / self.frame_bytes.max(1)).clamp(2, AHEAD)
    }

    fn hard_limit(&self) -> usize {
        (HARD_LIMIT_BYTES / self.frame_bytes.max(1)).clamp(4, HARD_LIMIT)
    }

    /// A buffer of `len` bytes, recycled when possible.
    pub(crate) fn buffer(&mut self, len: usize) -> Vec<u8> {
        let mut b = self.pool.pop().unwrap_or_default();
        b.clear();
        b.resize(len, 0);
        b
    }

    fn recycle(&mut self, f: Arc<VideoFrame>) {
        if let Ok(f) = Arc::try_unwrap(f)
            && self.pool.len() < POOL
        {
            self.pool.push(f.rgba);
        }
    }

    /// Queue a frame (serial assigned here). Returns true if it was the
    /// preroll frame, which the UI should show right away.
    pub(crate) fn push(&mut self, width: u32, height: u32, rgba: Vec<u8>, pts: f64) -> bool {
        self.frame_bytes = rgba.len();
        while self.pending.len() >= self.hard_limit()
            && let Some(old) = self.pending.pop_front()
        {
            self.recycle(old);
        }
        self.serial += 1;
        let frame = VideoFrame { width, height, rgba, pts: crate::time::secs(pts), serial: self.serial };
        self.pending.push_back(Arc::new(frame));
        self.preroll
    }

    /// Show the newest frame that is due at `clock` (seconds).
    pub(crate) fn advance(&mut self, clock: f64) -> bool {
        let mut changed = false;
        while let Some(front) = self.pending.front() {
            let due = front.pts.as_secs_f64() <= clock + 0.004;
            if !(self.preroll || self.current.is_none() || due) {
                break;
            }
            let shown = self.pending.pop_front();
            if let Some(old) = std::mem::replace(&mut self.current, shown) {
                self.recycle(old);
            }
            self.preroll = false;
            changed = true;
        }
        changed
    }

    /// Seconds until the next frame is due.
    pub(crate) fn next_due(&self, clock: f64) -> Option<f64> {
        self.pending.front().map(|f| (f.pts.as_secs_f64() - clock).max(0.0))
    }

    /// Drop queued frames (a seek); the current one stays on screen until
    /// the first new one replaces it.
    pub(crate) fn clear(&mut self) {
        while let Some(f) = self.pending.pop_front() {
            self.recycle(f);
        }
        self.preroll = true;
    }
}

/// Frames are never bigger than this on their longer side (an 8K video, or
/// a file claiming 10:1 pixels, would otherwise queue ~130 MB–1.3 GB frames).
pub(crate) const MAX_SIDE: u32 = 4096;

/// The size frames are scaled to: the display size (pixel aspect applied),
/// shrunk to fit `target` (and [`MAX_SIDE`]) but never enlarged.
pub(crate) fn fit_size(width: u32, height: u32, sar: Option<f64>, target: Option<(u32, u32)>) -> (u32, u32) {
    let sar = sar.filter(|r| r.is_finite() && (0.1..=10.0).contains(r)).unwrap_or(1.0);
    let (dw, dh) = if sar >= 1.0 { (width as f64 * sar, height as f64) } else { (width as f64, height as f64 / sar) };
    let (tw, th) = target.unwrap_or((MAX_SIDE, MAX_SIDE));
    let (tw, th) = (tw.min(MAX_SIDE) as f64, th.min(MAX_SIDE) as f64);
    let scale = if dw > 0.0 && dh > 0.0 { (tw / dw).min(th / dh).min(1.0) } else { 1.0 };
    (((dw * scale).round() as u32).max(1), ((dh * scale).round() as u32).max(1))
}

/// swscale from the decoder's format to RGBA at the wanted size.
pub(crate) struct Scaler {
    key: (ff::format::Pixel, u32, u32, u32, u32),
    ctx: ff::software::scaling::Context,
    out: ff::frame::Video,
}

impl Scaler {
    /// Convert `frame` to tightly packed RGBA of `size`, reusing this scaler
    /// when the formats match.
    pub(crate) fn convert(
        cached: &mut Option<Scaler>,
        frame: &ff::frame::Video,
        size: (u32, u32),
        into: &mut Vec<u8>,
    ) -> Result<(), ff::Error> {
        let key = (frame.format(), frame.width(), frame.height(), size.0, size.1);
        if cached.as_ref().is_none_or(|s| s.key != key) {
            let shrinking = size.0 * 2 < frame.width() || size.1 * 2 < frame.height();
            let flags = if shrinking { ff::software::scaling::Flags::AREA } else { ff::software::scaling::Flags::BILINEAR };
            let ctx = ff::software::scaling::Context::get(key.0, key.1, key.2, ff::format::Pixel::RGBA, size.0, size.1, flags)?;
            *cached = Some(Scaler { key, ctx, out: ff::frame::Video::empty() });
        }
        let Some(s) = cached.as_mut() else { return Err(ff::Error::Bug) };
        s.ctx.run(frame, &mut s.out)?;
        let row = size.0 as usize * 4;
        let stride = s.out.stride(0);
        let data = s.out.data(0);
        into.clear();
        into.reserve(row * size.1 as usize);
        for y in 0..size.1 as usize {
            match data.get(y * stride..y * stride + row) {
                Some(line) => into.extend_from_slice(line),
                None => into.resize(into.len() + row, 0),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_show_when_due() {
        let mut q = FrameQueue::new();
        for i in 0..5 {
            q.push(1, 1, vec![0; 4], 1.0 + i as f64 * 0.1);
        }
        assert!(q.advance(0.0)); // preroll: the first frame at once
        assert_eq!(q.current.as_ref().unwrap().pts, Duration::from_secs(1));
        assert!(!q.advance(0.5));
        assert!(q.advance(1.25)); // skips to the newest due frame
        assert_eq!(q.current.as_ref().unwrap().pts, Duration::from_millis(1200));
        assert!((q.next_due(1.25).unwrap() - 0.05).abs() < 1e-9);
        q.clear();
        assert_eq!(q.len(), 0);
        assert!(q.current.is_some()); // stays until replaced
        q.push(1, 1, vec![0; 4], 9.0);
        assert!(q.advance(0.0));
        assert_eq!(q.current.as_ref().unwrap().serial, 6);
    }

    #[test]
    fn big_frames_queue_less() {
        let mut q = FrameQueue::new();
        q.push(160, 90, vec![0; 160 * 90 * 4], 0.0);
        assert_eq!(q.ahead(), AHEAD);
        q.push(3840, 2160, vec![0; 3840 * 2160 * 4], 0.1); // 33 MB
        assert_eq!((q.ahead(), q.hard_limit()), (2, 8));
        for i in 0..20 {
            q.push(3840, 2160, vec![0; 3840 * 2160 * 4], 0.2 + i as f64);
        }
        assert_eq!(q.len(), 8);
    }

    #[test]
    fn fits_without_enlarging() {
        assert_eq!(fit_size(160, 90, None, None), (160, 90));
        assert_eq!(fit_size(160, 90, None, Some((80, 80))), (80, 45));
        assert_eq!(fit_size(160, 90, None, Some((1000, 1000))), (160, 90));
        assert_eq!(fit_size(720, 576, Some(16.0 / 15.0), None), (768, 576)); // PAL 4:3
        assert_eq!(fit_size(100, 100, Some(0.5), Some((50, 400))), (50, 100));
        // Huge: capped, also when the UI asks for more.
        assert_eq!(fit_size(7680, 4320, None, None), (4096, 2304));
        assert_eq!(fit_size(7680, 4320, None, Some((10_000, 10_000))), (4096, 2304));
        assert_eq!(fit_size(4000, 100, Some(10.0), None), (4096, 10));
    }
}
