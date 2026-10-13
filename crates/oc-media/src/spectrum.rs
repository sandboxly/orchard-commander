//! A live spectrum analyzer for the audio being played.
//!
//! The player's audio output hands every block it plays to a
//! [`SpectrumAnalyzer`] (also while muted), which keeps the latest samples as
//! mono floats. At each animation frame the UI asks for the levels of
//! logarithmically spaced bands, computed with a Hann-windowed FFT, and runs
//! them through a [`SpectrumAnimator`] for segmented "LED" bars with falling
//! peak caps.

use std::sync::{Arc, Mutex, MutexGuard};

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

pub const MIN_HZ: f64 = 30.0;
pub const MAX_HZ: f64 = 16000.0;
/// Quieter than this is an unlit bar.
pub const DB_FLOOR: f64 = -60.0;
/// A full bar (single frequencies in music rarely get louder).
pub const DB_CEILING: f64 = -8.0;
/// Music has less energy up high; lift it around 1 kHz.
pub const TILT_DB_PER_OCTAVE: f64 = 3.0;
/// FFT length: ~46 ms, a power of two in samples.
pub const WINDOW_SECONDS: f64 = 0.046;
/// Animation frames per second while playing.
pub const FPS: u32 = 60;
/// At most this many bars, however wide the view.
pub const MAX_BARS: usize = 96;

/// A Hann-windowed FFT of a fixed power-of-two size.
pub struct Fft {
    n: usize,
    plan: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
}

impl std::fmt::Debug for Fft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fft({})", self.n)
    }
}

impl Fft {
    /// Panics unless `n` is a power of two.
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two(), "FFT size must be a power of two");
        let plan = FftPlanner::new().plan_fft_forward(n);
        let denom = (n.max(2) - 1) as f64;
        let window =
            (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / denom).cos()) as f32).collect();
        Fft { n, plan, window }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The plain (unwindowed) transform of `x` (`n` samples).
    pub fn transform(&self, x: &[f32]) -> Vec<Complex<f32>> {
        let mut buf: Vec<Complex<f32>> = (0..self.n).map(|i| Complex::new(x.get(i).copied().unwrap_or(0.0), 0.0)).collect();
        self.plan.process(&mut buf);
        buf
    }

    /// Amplitude per bin 0..=n/2 (a full-scale sine reads about 1.0).
    pub fn magnitudes(&self, samples: &[f32]) -> Vec<f32> {
        let mut buf: Vec<Complex<f32>> = (0..self.n)
            .map(|i| Complex::new(samples.get(i).copied().unwrap_or(0.0) * self.window[i], 0.0))
            .collect();
        self.plan.process(&mut buf);
        let scale = 4.0 / self.n as f32; // 2 for one side of the spectrum, 2 for the Hann window's gain
        buf[..=self.n / 2].iter().map(|c| c.norm() * scale).collect()
    }
}

/// The `count + 1` band edges (Hz), logarithmic from [`MIN_HZ`] to
/// [`MAX_HZ`] (or the Nyquist frequency, if lower).
pub fn band_edges(count: usize, rate: u32) -> Vec<f64> {
    let top = MAX_HZ.min(rate as f64 / 2.0);
    (0..=count).map(|i| MIN_HZ * (top / MIN_HZ).powf(i as f64 / count.max(1) as f64)).collect()
}

/// 0..1 per logarithmic band: the loudest bin in the band, between
/// [`DB_FLOOR`] and [`DB_CEILING`], tilted by [`TILT_DB_PER_OCTAVE`].
pub fn band_levels(magnitudes: &[f32], rate: u32, count: usize) -> Vec<f32> {
    if magnitudes.len() < 2 || rate == 0 {
        return vec![0.0; count];
    }
    let n = ((magnitudes.len() - 1) * 2) as f64;
    let edges = band_edges(count, rate);
    edges
        .windows(2)
        .map(|w| {
            let (lo_hz, hi_hz) = (w[0], w[1]);
            let (lo, hi) = (lo_hz * n / rate as f64, hi_hz * n / rate as f64);
            let mag = if hi.floor() > lo.floor() {
                // Whole bins in the band.
                let start = (lo as usize + 1).min(magnitudes.len());
                let end = (hi as usize + 1).min(magnitudes.len());
                magnitudes[start..end].iter().copied().fold(0.0f32, f32::max) as f64
            } else {
                // A band narrower than a bin (bass): interpolate at its centre.
                let c = (lo + hi) / 2.0;
                let i = (c as usize).min(magnitudes.len() - 2);
                let (a, b) = (magnitudes[i] as f64, magnitudes[i + 1] as f64);
                a + (b - a) * (c - i as f64)
            };
            let centre = (lo_hz * hi_hz).sqrt();
            let db = 20.0 * (mag + 1e-12).log10() + TILT_DB_PER_OCTAVE * (centre / 1000.0).log2();
            ((db - DB_FLOOR) / (DB_CEILING - DB_FLOOR)).clamp(0.0, 1.0) as f32
        })
        .collect()
}

/// How many bars of `bar` px with `gap` px between fit in `width` (1..=[`MAX_BARS`]).
pub fn bar_count(width: f32, bar: f32, gap: f32) -> usize {
    (((width + gap) / (bar + gap)).floor().max(1.0) as usize).min(MAX_BARS)
}

#[derive(Default)]
struct Window {
    /// The last `n` mono samples, a ring written at `pos`.
    ring: Vec<f32>,
    pos: usize,
    filled: usize,
    rate: u32,
    fft: Option<Arc<Fft>>,
}

/// The latest played samples (mono), and their band levels on request.
/// Thread-safe: the audio output feeds it, the UI reads it; feeding never
/// allocates once the window is set up and never runs the FFT.
#[derive(Default)]
pub struct SpectrumAnalyzer {
    inner: Mutex<Window>,
}

impl std::fmt::Debug for SpectrumAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SpectrumAnalyzer({} Hz)", self.rate())
    }
}

/// The FFT size for a sample rate: ~[`WINDOW_SECONDS`], at least 256.
pub fn window_size(rate: u32) -> usize {
    let exp = (rate as f64 * WINDOW_SECONDS).log2().round().max(8.0) as u32;
    1 << exp.min(16)
}

impl SpectrumAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Window> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget everything (a new file, or stopped).
    pub fn reset(&self) {
        *self.lock() = Window::default();
    }

    /// Sample rate of what was fed last (0 before any audio).
    pub fn rate(&self) -> u32 {
        self.lock().rate
    }

    /// Feed interleaved float samples (full scale 1.0).
    pub fn feed_f32(&self, interleaved: &[f32], channels: usize, rate: u32) {
        self.feed(interleaved, channels, rate, |s| s);
    }

    /// Feed interleaved 16-bit samples (full scale 32768).
    pub fn feed_i16(&self, interleaved: &[i16], channels: usize, rate: u32) {
        self.feed(interleaved, channels, rate, |s| s as f32 / 32768.0);
    }

    fn feed<S: Copy>(&self, data: &[S], channels: usize, rate: u32, to_f32: impl Fn(S) -> f32) {
        if channels == 0 || rate == 0 {
            return;
        }
        let mut w = self.lock();
        if w.rate != rate {
            let n = window_size(rate);
            *w = Window { ring: vec![0.0; n], rate, ..Window::default() };
        }
        let n = w.ring.len();
        // Only the last n frames matter.
        let frames = data.len() / channels;
        let skip = frames.saturating_sub(n);
        for frame in data.chunks_exact(channels).skip(skip) {
            let mono = if channels == 1 {
                to_f32(frame[0])
            } else {
                frame.iter().map(|&s| to_f32(s)).sum::<f32>() / channels as f32
            };
            let pos = w.pos;
            w.ring[pos] = mono;
            w.pos = (pos + 1) % n;
        }
        w.filled = (w.filled + frames - skip).min(n);
    }

    /// Band levels of the most recent window, or None before any audio.
    pub fn levels(&self, count: usize) -> Option<Vec<f32>> {
        let (samples, rate, fft) = {
            let w = self.lock();
            if w.filled == 0 || w.ring.is_empty() {
                return None;
            }
            let n = w.ring.len();
            // Oldest first, zeros in front of a partly filled window.
            let mut samples = vec![0.0f32; n - w.filled];
            let start = (w.pos + n - w.filled) % n;
            samples.extend((0..w.filled).map(|i| w.ring[(start + i) % n]));
            (samples, w.rate, w.fft.clone())
        };
        let fft = match fft {
            Some(f) if f.len() == samples.len() => f,
            _ => {
                let f = Arc::new(Fft::new(samples.len()));
                let mut w = self.lock();
                if w.ring.len() == f.len() {
                    w.fft = Some(f.clone());
                }
                f
            }
        };
        Some(band_levels(&fft.magnitudes(&samples), rate, count))
    }
}

/// The bars' motion: rise quickly, fall at a steady pace; peak caps hold a
/// moment, then fall faster and faster.
#[derive(Clone, Debug, Default)]
pub struct SpectrumAnimator {
    pub bars: Vec<f32>,
    pub peaks: Vec<f32>,
    peak_age: Vec<f32>,
    peak_speed: Vec<f32>,
    /// Playing: follow the music. Otherwise the bars fall.
    pub active: bool,
}

impl SpectrumAnimator {
    /// Seconds a peak cap stays before falling.
    pub const PEAK_HOLD: f32 = 0.35;
    /// Bar fall speed, in heights per second.
    pub const FALL: f32 = 1.6;
    /// Peak caps fall faster and faster (heights per second²).
    pub const GRAVITY: f32 = 3.0;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Advance by `dt` seconds towards `levels` (ignored unless active; None
    /// reads as silence). Returns whether to keep animating: false once
    /// inactive and everything has settled.
    pub fn tick(&mut self, dt: f32, levels: Option<&[f32]>, count: usize) -> bool {
        let dt = dt.clamp(0.0, 0.1);
        if self.bars.len() != count {
            self.bars = vec![0.0; count];
            self.peaks = vec![0.0; count];
            self.peak_age = vec![0.0; count];
            self.peak_speed = vec![0.0; count];
        }
        let levels = if self.active { levels } else { None };
        for i in 0..count {
            let level = levels.and_then(|l| l.get(i)).copied().unwrap_or(0.0);
            let mut bar = self.bars[i];
            // Rise quickly (a little smoothing against flicker), fall at a steady pace.
            bar = if level > bar { bar + (level - bar) * 0.7 } else { level.max(bar - Self::FALL * dt) };
            self.bars[i] = bar;
            if bar >= self.peaks[i] {
                self.peaks[i] = bar;
                self.peak_age[i] = 0.0;
                self.peak_speed[i] = 0.0;
            } else {
                self.peak_age[i] += dt;
                if self.peak_age[i] > Self::PEAK_HOLD {
                    self.peak_speed[i] += Self::GRAVITY * dt;
                    self.peaks[i] = bar.max(self.peaks[i] - self.peak_speed[i] * dt);
                }
            }
        }
        self.active || self.peaks.iter().copied().fold(0.0, f32::max) > 0.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 44100;

    fn sine(freq: f64, n: usize, amplitude: f64) -> Vec<f32> {
        (0..n).map(|i| (amplitude * (2.0 * std::f64::consts::PI * freq * i as f64 / RATE as f64).sin()) as f32).collect()
    }

    #[test]
    fn fft_matches_the_dft() {
        let n = 64;
        let x: Vec<f32> = (0..n).map(|i| ((i * 7919 % 113) as f32 / 56.0) - 1.0).collect();
        let result = Fft::new(n).transform(&x);
        for k in [0, 1, 5, 31, 63] {
            let dft: Complex<f64> = (0..n)
                .map(|i| {
                    let a = -2.0 * std::f64::consts::PI * (k * i) as f64 / n as f64;
                    Complex::new(a.cos(), a.sin()) * x[i] as f64
                })
                .sum();
            let got = Complex::new(result[k].re as f64, result[k].im as f64);
            assert!((got - dft).norm() < 1e-4, "bin {k}: {got} vs {dft}");
        }
    }

    #[test]
    fn a_full_scale_sine_reads_about_one() {
        let fft = Fft::new(2048);
        let bin = 93; // a frequency exactly on a bin
        let mags = fft.magnitudes(&sine(bin as f64 * RATE as f64 / 2048.0, 2048, 1.0));
        assert!((mags[bin] - 1.0).abs() < 0.02, "{}", mags[bin]);
        let others = mags[..bin - 3].iter().chain(&mags[bin + 4..]).copied().fold(0.0, f32::max);
        assert!(others < 0.01, "{others}");
    }

    #[test]
    fn a_tone_lights_its_band() {
        let count = 40;
        let levels = band_levels(&Fft::new(2048).magnitudes(&sine(1000.0, 2048, 0.5)), RATE, count);
        let edges = band_edges(count, RATE);
        let band = (0..count).find(|&i| edges[i] <= 1000.0 && 1000.0 < edges[i + 1]).unwrap();
        let loudest = levels.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(loudest, band);
        assert!(levels[band] > 0.8);
        assert_eq!((levels[0], levels[count - 1]), (0.0, 0.0)); // far away: unlit
        assert!(levels.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn band_edges_stop_at_nyquist() {
        let edges = band_edges(10, 22050);
        assert_eq!(edges.len(), 11);
        assert!((edges[0] - MIN_HZ).abs() < 1e-9 && (edges[10] - 11025.0).abs() < 1e-6);
        assert!((band_edges(4, 48000)[4] - MAX_HZ).abs() < 1e-6);
    }

    #[test]
    fn analyzer_takes_played_buffers() {
        let analyzer = SpectrumAnalyzer::new();
        assert!(analyzer.levels(32).is_none());
        let tone = sine(2000.0, 6000, 0.5);
        for chunk in tone.chunks(576) {
            // Stereo 16-bit, the size the player delivers.
            let stereo: Vec<i16> = chunk.iter().flat_map(|&s| [(s * 32767.0) as i16; 2]).collect();
            analyzer.feed_i16(&stereo, 2, RATE);
        }
        assert_eq!(analyzer.rate(), RATE);
        let levels = analyzer.levels(32).unwrap();
        let edges = band_edges(32, RATE);
        let loudest = levels.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert!(edges[loudest] <= 2000.0 && 2000.0 < edges[loudest + 1]);
        analyzer.reset();
        assert!(analyzer.levels(32).is_none());
    }

    #[test]
    fn a_short_feed_is_padded() {
        let analyzer = SpectrumAnalyzer::new();
        analyzer.feed_f32(&sine(440.0, 100, 0.5), 1, RATE);
        assert_eq!(analyzer.levels(8).unwrap().len(), 8);
        assert_eq!(window_size(44100), 2048);
        assert_eq!(window_size(48000), 2048);
        assert_eq!(window_size(8000), 512);
    }

    #[test]
    fn animator_rises_while_playing_and_settles_when_paused() {
        let mut anim = SpectrumAnimator::new();
        anim.active = true;
        let target = vec![0.8; 10];
        for _ in 0..10 {
            assert!(anim.tick(1.0 / 60.0, Some(&target), 10));
        }
        assert!(anim.bars.iter().all(|&b| b > 0.5));
        assert!(anim.peaks.iter().zip(&anim.bars).all(|(p, b)| p >= b));
        anim.active = false;
        let mut frames = 0;
        while anim.tick(1.0 / 60.0, Some(&target), 10) {
            frames += 1;
            assert!(frames < 600, "never settles");
        }
        assert!(anim.bars.iter().all(|&b| b == 0.0));
        assert_eq!(bar_count(500.0, 12.0, 3.0), 33);
        assert_eq!(bar_count(5000.0, 4.0, 2.0), MAX_BARS);
        assert_eq!(bar_count(0.0, 4.0, 2.0), 1);
    }
}
