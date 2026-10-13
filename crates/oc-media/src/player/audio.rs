//! Decoded audio waiting to be played, and the clock it drives.
//!
//! The decoder pushes interleaved f32 chunks (already in the output's rate
//! and channel count) tagged with their presentation time; the output pulls
//! them in [`AudioShared::render`] from its real-time callback. The audio
//! clock is the time of the sample being played (minus the device latency
//! cpal reports), which video frames are timed against.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::Thread;

use crate::SpectrumAnalyzer;

#[derive(Default)]
struct Ring {
    samples: VecDeque<f32>,
    /// (frame index in the stream since the last flush, pts in seconds) of
    /// each pushed chunk, oldest first.
    marks: VecDeque<(u64, f64)>,
    /// Frames played since the last flush.
    read: u64,
    /// Time of the next frame to play.
    clock: f64,
    /// Where playback (re)started: the clock never reads earlier.
    base: f64,
    /// Seconds between rendering a frame and hearing it.
    latency: f64,
    rate: u32,
    channels: u16,
}

pub(crate) struct AudioShared {
    ring: Mutex<Ring>,
    pub(crate) playing: AtomicBool,
    volume: AtomicU32,
    muted: AtomicBool,
    /// Output threads end when set.
    pub(crate) stop: AtomicBool,
    /// Bumped when the output's rate or channel count changes (another device).
    generation: AtomicU64,
    output_thread: Mutex<Option<Thread>>,
    pub(crate) spectrum: Arc<SpectrumAnalyzer>,
}

impl AudioShared {
    pub(crate) fn new(spectrum: Arc<SpectrumAnalyzer>, volume: f32, muted: bool) -> Self {
        AudioShared {
            ring: Mutex::new(Ring { rate: 48000, channels: 2, ..Ring::default() }),
            playing: AtomicBool::new(false),
            volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
            muted: AtomicBool::new(muted),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            output_thread: Mutex::new(None),
            spectrum,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Ring> {
        self.ring.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub(crate) fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub(crate) fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    /// The output's (rate, channels).
    pub(crate) fn format(&self) -> (u32, u16) {
        let r = self.lock();
        (r.rate, r.channels)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The output changed format: drop what was decoded for the old one.
    pub(crate) fn set_format(&self, rate: u32, channels: u16) {
        let mut r = self.lock();
        if (r.rate, r.channels) == (rate, channels) {
            return;
        }
        r.rate = rate.max(1);
        r.channels = channels.max(1);
        let clock = r.clock;
        Self::reset(&mut r, clock);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn reset(r: &mut Ring, pos: f64) {
        r.samples.clear();
        r.marks.clear();
        r.read = 0;
        r.clock = pos;
        r.base = pos;
    }

    /// Drop everything queued; the clock reads `pos` until new audio plays.
    pub(crate) fn flush(&self, pos: f64) {
        Self::reset(&mut self.lock(), pos);
    }

    /// Queue interleaved samples (in the output format) starting at `pts` s.
    pub(crate) fn push(&self, pts: f64, samples: &[f32]) {
        let mut r = self.lock();
        let at = r.read + (r.samples.len() / r.channels as usize) as u64;
        r.marks.push_back((at, pts));
        r.samples.extend(samples);
    }

    pub(crate) fn buffered_secs(&self) -> f64 {
        let r = self.lock();
        r.samples.len() as f64 / r.channels as f64 / r.rate as f64
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lock().samples.is_empty()
    }

    /// The time being heard.
    pub(crate) fn clock(&self) -> f64 {
        let r = self.lock();
        (r.clock - r.latency).max(r.base)
    }

    /// Fill `out` (interleaved, `channels` per frame) from the queue; silence
    /// while paused or starved. Called from the audio device's thread.
    pub(crate) fn render(&self, out: &mut [f32], channels: usize, latency: f64) {
        if !self.playing.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }
        let (n, rate) = {
            let mut r = self.lock();
            r.latency = latency;
            if r.channels as usize != channels || channels == 0 {
                drop(r);
                out.fill(0.0); // switching devices
                return;
            }
            let frames = (out.len() / channels).min(r.samples.len() / channels);
            let n = frames * channels;
            for (o, s) in out[..n].iter_mut().zip(r.samples.drain(..n)) {
                *o = s;
            }
            if frames > 0 {
                r.read += frames as u64;
                let read = r.read;
                while r.marks.len() >= 2 && r.marks[1].0 <= read {
                    r.marks.pop_front();
                }
                if let Some(&(at, pts)) = r.marks.front() {
                    r.clock = pts + read.saturating_sub(at) as f64 / r.rate as f64;
                }
            }
            (n, r.rate)
        };
        out[n..].fill(0.0);
        // What's played, also while muted.
        self.spectrum.feed_f32(&out[..n], channels, rate);
        let gain = if self.muted() { 0.0 } else { self.volume() };
        if gain != 1.0 {
            out[..n].iter_mut().for_each(|s| *s *= gain);
        }
    }

    /// The output thread to wake on play/pause and shutdown.
    pub(crate) fn set_output_thread(&self, t: Option<Thread>) {
        *self.output_thread.lock().unwrap_or_else(|e| e.into_inner()) = t;
    }

    pub(crate) fn kick_output(&self) {
        if let Some(t) = &*self.output_thread.lock().unwrap_or_else(|e| e.into_inner()) {
            t.unpark();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> AudioShared {
        let a = AudioShared::new(Arc::new(SpectrumAnalyzer::new()), 1.0, false);
        a.set_format(10, 1);
        a.playing.store(true, Ordering::Release);
        a
    }

    #[test]
    fn the_clock_follows_what_is_played() {
        let a = shared();
        a.flush(5.0);
        assert_eq!(a.clock(), 5.0);
        a.push(5.0, &[0.1; 10]);
        a.push(7.0, &[0.2; 10]); // a gap: the next chunk starts later
        let mut out = [0.0; 5];
        a.render(&mut out, 1, 0.0);
        assert_eq!(out, [0.1; 5]);
        assert!((a.clock() - 5.5).abs() < 1e-9);
        let mut out = [0.0; 8];
        a.render(&mut out, 1, 0.0);
        assert!((a.clock() - 7.3).abs() < 1e-9);
        assert!((a.buffered_secs() - 0.7).abs() < 1e-9);
        let mut out = [1.0; 10];
        a.render(&mut out, 1, 0.0); // starved: silence after the rest
        assert_eq!(&out[7..], &[0.0; 3]);
        assert!(a.is_empty());
        assert!((a.clock() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn paused_muted_and_latency() {
        let a = shared();
        a.push(0.0, &[0.5; 10]);
        a.playing.store(false, Ordering::Release);
        let mut out = [1.0; 4];
        a.render(&mut out, 1, 0.0);
        assert_eq!((out, a.clock()), ([0.0; 4], 0.0)); // nothing consumed
        a.playing.store(true, Ordering::Release);
        a.set_muted(true);
        a.render(&mut out, 1, 0.2);
        assert_eq!(out, [0.0; 4]);
        assert!(a.spectrum.levels(4).is_some()); // the analyzer still hears it
        assert!((a.clock() - 0.2).abs() < 1e-9); // 0.4 played, 0.2 still on its way
        a.set_muted(false);
        a.set_volume(0.5);
        a.render(&mut out, 1, 0.0);
        assert_eq!(out, [0.25; 4]);
    }
}
