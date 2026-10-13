#![allow(dead_code)]

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oc_media::{Event, MediaInfo, Player};
use oc_vfs::ReadStream;

pub fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

/// A `freq` Hz tone at `amplitude` for `tone` seconds, then `silence`
/// seconds of silence; 16-bit PCM WAV (as the Python tests' write_wav).
pub fn write_wav(path: &Path, tone: f64, silence: f64, amplitude: f64, freq: f64, rate: u32, channels: u16) -> PathBuf {
    let frames = (rate as f64 * (tone + silence)) as usize;
    let mut pcm = Vec::with_capacity(frames * channels as usize * 2);
    for i in 0..frames {
        let v = if (i as f64) < rate as f64 * tone {
            amplitude * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin()
        } else {
            0.0
        };
        for _ in 0..channels {
            pcm.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
        }
    }
    let mut f = std::fs::File::create(path).unwrap();
    let block = channels as u32 * 2;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + pcm.len() as u32).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * block).to_le_bytes()).unwrap();
    f.write_all(&(block as u16).to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&(pcm.len() as u32).to_le_bytes()).unwrap();
    f.write_all(&pcm).unwrap();
    path.to_path_buf()
}

/// 1 s of 440 Hz at half volume, then 1 s of silence; 22.05 kHz stereo.
pub fn tone_wav(dir: &Path) -> PathBuf {
    write_wav(&dir.join("tone.wav"), 1.0, 1.0, 0.5, 440.0, 22050, 2)
}

pub fn has_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// Run the ffmpeg command with `args` (output file last); None without ffmpeg.
pub fn ffmpeg(args: &[&str]) -> Option<()> {
    if !has_ffmpeg() {
        eprintln!("skipped: no ffmpeg command");
        return None;
    }
    let out = Command::new("ffmpeg").args(["-y", "-hide_banner", "-loglevel", "error"]).args(args).output().unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Some(())
}

pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    f()
}

/// Events until `Loaded` (panics on an error or after 10 s).
pub fn wait_loaded(player: &Player) -> MediaInfo {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match player.wait_event(Duration::from_millis(100)) {
            Some(Event::Loaded(info)) => return *info,
            Some(Event::Error(e)) => panic!("player error: {e}"),
            _ => {}
        }
    }
    panic!("not loaded: {player:?}");
}

/// Events until `pred` matches one; returns everything seen.
pub fn wait_for_event(player: &Player, timeout: Duration, pred: impl Fn(&Event) -> bool) -> Option<Vec<Event>> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Some(e) = player.wait_event(Duration::from_millis(20)) {
            let hit = pred(&e);
            seen.push(e);
            if hit {
                return Some(seen);
            }
        }
    }
    None
}

pub fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

/// A stream that can't seek (like a member of a compressed archive).
pub struct Unseekable(pub io::Cursor<Vec<u8>>);

impl Read for Unseekable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Seek for Unseekable {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "can't seek"))
    }
}

impl ReadStream for Unseekable {
    fn seekable(&self) -> bool {
        false
    }
}

/// A slow server: every read waits `delay`; after `stall_after` bytes,
/// reads hang until `release` is set (or 20 s pass), then fail.
pub struct SlowStream {
    pub inner: io::Cursor<Vec<u8>>,
    pub delay: Duration,
    pub stall_after: Option<u64>,
    pub release: Arc<AtomicBool>,
    pub reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl SlowStream {
    pub fn new(data: Vec<u8>, delay: Duration) -> Self {
        SlowStream {
            inner: io::Cursor::new(data),
            delay,
            stall_after: None,
            release: Arc::new(AtomicBool::new(false)),
            reads: Arc::default(),
        }
    }
}

impl Read for SlowStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(self.delay);
        if let Some(limit) = self.stall_after {
            let pos = self.inner.position();
            if pos >= limit {
                let start = Instant::now();
                while !self.release.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(20) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(io::Error::new(io::ErrorKind::TimedOut, "server stopped responding"));
            }
            let n = buf.len().min((limit - pos) as usize);
            return self.inner.read(&mut buf[..n]);
        }
        // Small pieces, like a network socket.
        let n = buf.len().min(16 * 1024);
        self.inner.read(&mut buf[..n])
    }
}

impl Seek for SlowStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl ReadStream for SlowStream {}
