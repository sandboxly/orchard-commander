//! Waveforms of real audio (port of the Python test_waveform.py analyzer tests).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::*;
use oc_media::waveform::BLOCK_MS;
use oc_media::{MediaSource, WaveformAnalyzer, WaveformOptions};
use oc_vfs::FileSystem;
use oc_vfs::testing::MemFS;

fn analyze(source: MediaSource) -> WaveformAnalyzer {
    let a = WaveformAnalyzer::start(source, WaveformOptions::default());
    assert!(a.wait_complete(Duration::from_secs(10)), "never completes");
    a
}

#[test]
fn finds_the_loud_and_the_quiet_part() {
    let dir = tempfile::tempdir().unwrap();
    let a = analyze(MediaSource::Path(tone_wav(dir.path())));
    let l = a.snapshot();
    assert_eq!(l.error, "");
    assert!((l.decoded_ms() - 2000).abs() <= 2 * BLOCK_MS, "{}", l.decoded_ms());
    let half = l.peaks.len() / 2;
    let loud = &l.peaks[..half - 2];
    let quiet = &l.peaks[half + 2..];
    assert!(loud.iter().copied().fold(1.0, f32::min) > 0.3);
    assert!(quiet.iter().copied().fold(0.0, f32::max) < 0.01);
    assert!(l.peaks.iter().zip(&l.rms).all(|(p, r)| *r <= p + 1e-6));
    // A full-scale-ish sine: RMS ≈ peak / √2.
    assert!((l.rms[10] / l.peaks[10] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.05);
}

#[test]
fn decodes_mp3_without_hanging() {
    // Asking Qt's decoder to resample MP3s made it hang: stuck at "Analyzing…".
    let a = analyze(MediaSource::Path(data("tone-then-silence.mp3")));
    let l = a.snapshot();
    assert_eq!(l.error, "");
    assert!((l.decoded_ms() - 2000).abs() <= 5 * BLOCK_MS, "{}", l.decoded_ms()); // MP3 adds encoder padding
    let third = l.peaks.len() / 3;
    assert!(l.peaks[5..third].iter().copied().fold(1.0, f32::min) > 0.3);
    assert!(l.peaks[l.peaks.len() - third..].iter().copied().fold(0.0, f32::max) < 0.01);
    // What the UI draws: the loud columns first, then quiet ones.
    let cols = l.columns(100, 2000);
    assert!(cols.iter().all(Option::is_some));
    assert!(cols[10].unwrap().0 > 0.3 && cols[90].unwrap().0 < 0.01);
}

#[test]
fn reads_streams() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(tone_wav(dir.path())).unwrap();
    let mem = Arc::new(MemFS::new());
    mem.add_file("song.wav", bytes.clone());
    let stream = mem.open_read("song.wav").unwrap();
    let a = analyze(MediaSource::stream(stream, Some(bytes.len() as u64), "song.wav"));
    assert!((a.snapshot().decoded_ms() - 2000).abs() <= 2 * BLOCK_MS);

    // Unseekable streams too (e.g. inside a compressed archive).
    let mp3 = std::fs::read(data("tone-then-silence.mp3")).unwrap();
    let a = analyze(MediaSource::stream(Box::new(Unseekable(std::io::Cursor::new(mp3))), None, "song.mp3"));
    assert!((a.snapshot().decoded_ms() - 2000).abs() <= 5 * BLOCK_MS);
}

#[test]
fn fills_in_while_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_wav(&dir.path().join("long.wav"), 30.0, 0.0, 0.5, 440.0, 22050, 1);
    let bytes = std::fs::read(&path).unwrap();
    let woken = Arc::new(AtomicUsize::new(0));
    let w = woken.clone();
    let options = WaveformOptions { waker: Some(Arc::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    })), ..Default::default() };
    // A slow server: the levels arrive in pieces.
    let a = WaveformAnalyzer::start(MediaSource::stream(Box::new(SlowStream::new(bytes, Duration::from_millis(2))), None, "long.wav"), options);
    assert!(wait_until(Duration::from_secs(10), || {
        let l = a.snapshot();
        !l.complete && !l.peaks.is_empty()
    }));
    let g = a.generation();
    assert!(a.wait_complete(Duration::from_secs(20)));
    assert!(a.generation() > g);
    assert!((a.snapshot().decoded_ms() - 30_000).abs() <= 2 * BLOCK_MS);
    let wakes = woken.load(Ordering::Relaxed);
    assert!((2..2000).contains(&wakes), "{wakes} wake-ups"); // batched, not per frame
}

#[test]
fn gives_up_when_the_source_stalls() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(tone_wav(dir.path())).unwrap();
    let options = || WaveformOptions { stall: Duration::from_millis(300), ..Default::default() };

    // Nothing at all arrives.
    let mut nothing = SlowStream::new(bytes.clone(), Duration::ZERO);
    nothing.stall_after = Some(0);
    let release = nothing.release.clone();
    let t = Instant::now();
    let a = WaveformAnalyzer::start(MediaSource::stream(Box::new(nothing), None, "a.wav"), options());
    assert!(a.wait_complete(Duration::from_secs(5)));
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(a.snapshot().error, "the decoder stopped responding");
    assert_eq!(a.snapshot().status_text(""), "No waveform: the decoder stopped responding");
    let watch = a.shutdown_watch();
    drop(a);
    release.store(true, Ordering::Release);
    assert!(watch.wait(Duration::from_secs(5)), "decoder thread still running");

    // Part arrives (more than one 1 MiB read-ahead block): show that part.
    let long = std::fs::read(write_wav(&dir.path().join("long.wav"), 6.0, 6.0, 0.5, 440.0, 44100, 2)).unwrap();
    let mut part = SlowStream::new(long.clone(), Duration::ZERO);
    part.stall_after = Some(long.len() as u64 * 3 / 4);
    let release = part.release.clone();
    let a = WaveformAnalyzer::start(MediaSource::stream(Box::new(part), None, "a.wav"), options());
    assert!(a.wait_complete(Duration::from_secs(5)));
    let l = a.snapshot();
    assert_eq!(l.error, "");
    assert!(l.decoded_ms() > 3000 && l.decoded_ms() < 11000, "{}", l.decoded_ms());
    release.store(true, Ordering::Release);
}

#[test]
fn fails_quietly_on_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("garbage.mp3");
    std::fs::write(&bad, b"\x00\x01not audio".repeat(300)).unwrap();
    let a = analyze(MediaSource::Path(bad));
    let l = a.snapshot();
    assert!(!l.error.is_empty() && l.peaks.is_empty());
    // A video without sound has no waveform either.
    let a = analyze(MediaSource::Path(dir.path().join("missing.wav")));
    assert!(!a.snapshot().error.is_empty());
}

#[test]
fn dropping_does_not_wait_for_the_decoder() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(tone_wav(dir.path())).unwrap();
    let a = WaveformAnalyzer::start(MediaSource::stream(Box::new(SlowStream::new(bytes, Duration::from_millis(50))), None, "a.wav"), WaveformOptions::default());
    std::thread::sleep(Duration::from_millis(30));
    let watch = a.shutdown_watch();
    let t = Instant::now();
    drop(a);
    assert!(t.elapsed() < Duration::from_millis(20), "{:?}", t.elapsed());
    assert!(watch.wait(Duration::from_secs(2)), "threads still running: {watch:?}");
}
