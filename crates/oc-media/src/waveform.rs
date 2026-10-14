//! The waveform of an audio file: per-20 ms peak and RMS levels, decoded in
//! the background, drawn by the UI as mirrored bars that double as the seek
//! bar.
//!
//! The decoder downmixes to mono floats at the file's own sample rate and
//! never resamples (asking Qt's FFmpeg decoder to resample made it hang on
//! MP3s in 1.x). A watchdog gives up when no audio arrives for
//! [`WaveformOptions::stall`] (a stuck server read, a decoder that stops
//! without an error), showing what was decoded so far. The levels fill in
//! left to right while decoding; [`WaveformAnalyzer::generation`] changes
//! with every update.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::threads::ShutdownWatch;
use crate::{MediaSource, Waker};

/// One peak/RMS pair per 20 ms of audio.
pub const BLOCK_MS: i64 = 20;
/// No data for this long while decoding: give up.
pub const STALL: Duration = Duration::from_secs(8);
/// Shown instead of a waveform for audio on servers above the size limit.
pub const REMOTE_NOTE: &str = "No waveform for large files on servers";

/// Peak and RMS (0..1, may exceed 1 slightly after downmixing) of each whole
/// block of `per_block` samples, and the samples left over for the next call.
pub fn block_levels<S: Copy + Into<f64>>(samples: &[S], per_block: usize, full_scale: f64) -> (Vec<f32>, Vec<f32>, &[S]) {
    let per_block = per_block.max(1);
    let whole = samples.len() - samples.len() % per_block;
    let mut peaks = Vec::with_capacity(whole / per_block);
    let mut rms = Vec::with_capacity(whole / per_block);
    for block in samples[..whole].chunks_exact(per_block) {
        let (mut hi, mut lo, mut sq) = (f64::MIN, f64::MAX, 0.0);
        for &s in block {
            let v: f64 = s.into();
            hi = hi.max(v);
            lo = lo.min(v);
            sq += v * v;
        }
        peaks.push((hi.max(-lo) / full_scale) as f32);
        rms.push(((sq / per_block as f64).sqrt() / full_scale) as f32);
    }
    (peaks, rms, &samples[whole..])
}

/// Turns a stream of mono samples into block levels.
#[derive(Debug, Default)]
#[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
pub(crate) struct BlockAccumulator {
    rate: u32,
    per_block: usize,
    pending: Vec<f32>,
}

#[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
impl BlockAccumulator {
    /// Add mono samples at `rate`; returns the levels of the blocks completed.
    pub(crate) fn push(&mut self, mono: &[f32], rate: u32) -> (Vec<f32>, Vec<f32>) {
        if rate != self.rate {
            self.rate = rate;
            self.per_block = (rate as i64 * BLOCK_MS / 1000).max(1) as usize;
            self.pending.clear();
        }
        self.pending.extend_from_slice(mono);
        let (peaks, rms, rest) = block_levels(&self.pending, self.per_block, 1.0);
        let rest = rest.len();
        let done = self.pending.len() - rest;
        self.pending.drain(..done);
        (peaks, rms)
    }

    /// The last, partial block.
    pub(crate) fn finish(&mut self) -> (Vec<f32>, Vec<f32>) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let (peaks, rms, _) = block_levels(&pending, pending.len(), 1.0);
        (peaks, rms)
    }
}

/// What has been decoded so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WaveformLevels {
    pub peaks: Vec<f32>,
    pub rms: Vec<f32>,
    /// Decoding ended (finished, failed or gave up).
    pub complete: bool,
    /// Why there is no (complete) waveform; "" if none.
    pub error: String,
}

impl WaveformLevels {
    pub fn decoded_ms(&self) -> i64 {
        self.peaks.len() as i64 * BLOCK_MS
    }

    /// The time span the width stands for: what was decoded once complete,
    /// else the player's duration (the bars fill in from the left).
    pub fn total_ms(&self, duration_ms: i64) -> i64 {
        if self.complete && !self.peaks.is_empty() { self.decoded_ms() } else { duration_ms.max(self.decoded_ms()) }
    }

    /// (peak, mean RMS) per column for `count` columns across
    /// [`total_ms`](Self::total_ms); None where nothing is decoded yet.
    pub fn columns(&self, count: usize, duration_ms: i64) -> Vec<Option<(f32, f32)>> {
        let total_blocks = (self.total_ms(duration_ms) / BLOCK_MS).max(1) as usize;
        (0..count)
            .map(|i| {
                let lo = i * total_blocks / count;
                let hi = (lo + 1).max((i + 1) * total_blocks / count);
                if lo >= self.peaks.len() {
                    return None;
                }
                let hi = hi.min(self.peaks.len());
                let peak = self.peaks[lo..hi].iter().copied().fold(f32::MIN, f32::max);
                let rms = self.rms[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
                Some((peak, rms))
            })
            .collect()
    }

    /// The text shown instead of bars while there are none: the host's
    /// `note` (e.g. [`REMOTE_NOTE`]), the error, or "Analyzing…".
    pub fn status_text(&self, note: &str) -> String {
        if !note.is_empty() {
            note.to_string()
        } else if !self.error.is_empty() {
            format!("No waveform: {}", self.error)
        } else if self.complete {
            String::new()
        } else {
            "Analyzing…".to_string()
        }
    }
}

/// The time at `x` of a waveform `width` wide that stands for `total_ms`.
pub fn x_to_ms(x: f32, width: f32, total_ms: i64) -> i64 {
    ((x / width.max(1.0)).clamp(0.0, 1.0) as f64 * total_ms as f64).round() as i64
}

/// Where the playhead is: x of `position_ms` on a `width` wide waveform.
pub fn played_x(position_ms: i64, width: f32, total_ms: i64) -> f32 {
    if total_ms <= 0 { 0.0 } else { width * (position_ms as f32 / total_ms as f32).min(1.0) }
}

#[derive(Clone)]
pub struct WaveformOptions {
    /// Give up when no audio arrives for this long.
    pub stall: Duration,
    /// Called whenever the levels change.
    pub waker: Option<Waker>,
}

impl Default for WaveformOptions {
    fn default() -> Self {
        WaveformOptions { stall: STALL, waker: None }
    }
}

pub(crate) struct WaveShared {
    levels: Mutex<WaveformLevels>,
    changed: Condvar,
    generation: AtomicU64,
    pub(crate) abort: Arc<AtomicBool>,
    started: Instant,
    /// ms since `started` of the last audio decoded.
    last_progress: AtomicU64,
    waker: Option<Waker>,
}

#[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
impl WaveShared {
    fn lock(&self) -> MutexGuard<'_, WaveformLevels> {
        self.levels.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_all();
        if let Some(w) = &self.waker {
            w();
        }
    }

    pub(crate) fn aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }

    /// The decoder is alive (also when a frame had no whole block yet).
    pub(crate) fn progress(&self) {
        self.last_progress.store(self.started.elapsed().as_millis() as u64, Ordering::Release);
    }

    /// Add decoded blocks; `notify` tells the viewers (the decoder batches
    /// that). Unnotified blocks are still there when the watchdog gives up.
    pub(crate) fn push(&self, (peaks, rms): (Vec<f32>, Vec<f32>), notify: bool) {
        self.progress();
        if !peaks.is_empty() {
            let mut l = self.lock();
            if l.complete {
                return; // gave up meanwhile
            }
            l.peaks.extend(peaks);
            l.rms.extend(rms);
        }
        if notify {
            self.bump();
        }
    }

    /// Decoding ended: the last partial block, or why it failed.
    pub(crate) fn finish(&self, result: Result<(Vec<f32>, Vec<f32>), String>) {
        {
            let mut l = self.lock();
            if l.complete {
                return;
            }
            match result {
                Ok((peaks, rms)) => {
                    l.peaks.extend(peaks);
                    l.rms.extend(rms);
                }
                Err(e) => l.error = if e.is_empty() { "the audio can't be read".into() } else { e },
            }
            l.complete = true;
        }
        self.bump();
    }

    /// The watchdog: give up when nothing arrives for `stall`.
    fn watch(&self, stall: Duration) {
        let mut l = self.lock();
        loop {
            if l.complete || self.aborted() {
                return;
            }
            let last = Duration::from_millis(self.last_progress.load(Ordering::Acquire));
            let idle = self.started.elapsed().saturating_sub(last);
            if idle >= stall {
                // Show what was decoded; without anything, say why.
                if l.peaks.is_empty() {
                    l.error = "reading the audio stopped".into();
                }
                l.complete = true;
                self.abort.store(true, Ordering::Release);
                drop(l);
                self.bump();
                return;
            }
            l = self.changed.wait_timeout(l, stall - idle).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Decodes a file's audio in the background and collects its levels.
/// Dropping it stops the decoder (without waiting for it).
pub struct WaveformAnalyzer {
    shared: Arc<WaveShared>,
    watch: ShutdownWatch,
}

impl std::fmt::Debug for WaveformAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let l = self.shared.lock();
        write!(f, "WaveformAnalyzer({} blocks, complete: {})", l.peaks.len(), l.complete)
    }
}

impl WaveformAnalyzer {
    fn new(options: &WaveformOptions) -> Self {
        WaveformAnalyzer {
            shared: Arc::new(WaveShared {
                levels: Mutex::new(WaveformLevels::default()),
                changed: Condvar::new(),
                generation: AtomicU64::new(0),
                abort: Arc::new(AtomicBool::new(false)),
                started: Instant::now(),
                last_progress: AtomicU64::new(0),
                waker: options.waker.clone(),
            }),
            watch: ShutdownWatch::new(),
        }
    }

    /// No waveform (e.g. a large file on a server): complete, without levels.
    pub fn none() -> Self {
        let a = Self::new(&WaveformOptions::default());
        a.shared.lock().complete = true;
        a
    }

    /// Analyze a local file or a stream (which this then owns) in the background.
    pub fn start(source: MediaSource, options: WaveformOptions) -> Self {
        let a = Self::new(&options);
        let shared = a.shared.clone();
        let spawned = a.watch.spawn("oc-media waveform", move || {
            #[cfg(feature = "ffmpeg")]
            crate::ffmpeg::waveform::decode(source, &shared);
            #[cfg(not(feature = "ffmpeg"))]
            {
                drop(source);
                shared.finish(Err(crate::MediaError::Unsupported.to_string()));
            }
        });
        if let Err(e) = spawned {
            a.shared.finish(Err(e.to_string()));
            return a;
        }
        let shared = a.shared.clone();
        if let Err(e) = a.watch.spawn("oc-media waveform watchdog", move || shared.watch(options.stall)) {
            log::warn!("waveform watchdog: {e}");
        }
        a
    }

    /// Run `f` on the current levels (holds a lock: keep it short).
    pub fn with_levels<R>(&self, f: impl FnOnce(&WaveformLevels) -> R) -> R {
        f(&self.shared.lock())
    }

    /// A copy of the current levels.
    pub fn snapshot(&self) -> WaveformLevels {
        self.shared.lock().clone()
    }

    pub fn is_complete(&self) -> bool {
        self.shared.lock().complete
    }

    /// Changes whenever the levels do (cheap to poll on redraw).
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// Block until decoding is complete; false on timeout (tests, tools).
    pub fn wait_complete(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut l = self.shared.lock();
        while !l.complete {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            l = self.shared.changed.wait_timeout(l, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// The decoder's threads, to wait for them after dropping this.
    pub fn shutdown_watch(&self) -> ShutdownWatch {
        self.watch.clone()
    }
}

impl Drop for WaveformAnalyzer {
    fn drop(&mut self) {
        self.shared.abort.store(true, Ordering::Release);
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_levels_like_python() {
        let samples = [0.5f32, -1.0, 0.25, 0.0, 0.1];
        let (peaks, rms, rest) = block_levels(&samples, 2, 1.0);
        assert_eq!(peaks, [1.0, 0.25]);
        assert!((rms[0] as f64 - ((0.25 + 1.0) / 2.0f64).sqrt()).abs() < 1e-6);
        assert_eq!(rest, [0.1]);
        let (peaks, _, _) = block_levels(&[16384i16, -32768], 2, 32768.0);
        assert_eq!(peaks, [1.0]);
    }

    #[test]
    fn accumulator_carries_partial_blocks() {
        let mut acc = BlockAccumulator::default();
        let (p, _) = acc.push(&[0.5; 300], 10_000); // 200 per block
        assert_eq!(p.len(), 1);
        let (p, _) = acc.push(&[0.25; 150], 10_000);
        assert_eq!(p, [0.5]); // 100 left over + 150 = one block, mixed
        let (p, r) = acc.finish();
        assert_eq!((p, r.len()), (vec![0.25], 1));
        assert_eq!(acc.finish().0.len(), 0);
    }

    #[test]
    fn columns_and_geometry() {
        let mut l = WaveformLevels { peaks: vec![1.0; 50], rms: vec![0.5; 50], ..Default::default() };
        l.peaks.extend([0.0; 50]);
        l.rms.extend([0.0; 50]);
        // Decoding: half of a 4 s file so far.
        assert_eq!(l.total_ms(4000), 4000);
        let cols = l.columns(20, 4000);
        assert!(cols[..5].iter().all(|c| c == &Some((1.0, 0.5))));
        assert!(cols[10..].iter().all(Option::is_none));
        l.complete = true;
        assert_eq!(l.total_ms(4000), 2000);
        assert!(l.columns(100, 0).iter().all(Option::is_some));
        assert_eq!(x_to_ms(450.0, 600.0, 2000), 1500);
        assert_eq!(x_to_ms(-5.0, 600.0, 2000), 0);
        assert_eq!(played_x(1000, 600.0, 2000), 300.0);
        assert_eq!(played_x(1000, 600.0, 0), 0.0);
    }

    #[test]
    fn status_texts() {
        let mut l = WaveformLevels::default();
        assert_eq!(l.status_text(""), "Analyzing…");
        assert_eq!(l.status_text(REMOTE_NOTE), REMOTE_NOTE);
        l.complete = true;
        l.error = "boom".into();
        assert_eq!(l.status_text(""), "No waveform: boom");
        assert!(WaveformAnalyzer::none().is_complete());
    }
}
