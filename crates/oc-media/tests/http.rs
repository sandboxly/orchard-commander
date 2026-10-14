//! Streaming from a real server: a WebDAV share (wsgidav, as in the Python
//! tests' `test_media_streams_from_webdav`) read with HTTP range requests
//! over real sockets, the way server file systems feed the player.
//!
//! Needs `wsgidav` (from the Python version's venv, `$OC_WSGIDAV`, or the
//! PATH); skips without it.

mod common;

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::*;
use oc_media::{AudioOutput, Event, MediaSource, Player, PlayerOptions, WaveformAnalyzer, WaveformOptions, probe_stream};
use oc_vfs::ReadStream;

/// $OC_WSGIDAV, the one in $OC_TEST_PYTHON's venv or the repository's
/// `.venv`, else one on the PATH.
fn wsgidav() -> Option<PathBuf> {
    let in_venv = |python: PathBuf| python.parent().map(|bin| bin.join("wsgidav"));
    let repo_venv = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().map(|a| a.join(".venv/bin/wsgidav")).find(|p| p.is_file());
    let candidates =
        [std::env::var_os("OC_WSGIDAV").map(PathBuf::from), std::env::var_os("OC_TEST_PYTHON").map(PathBuf::from).and_then(in_venv), repo_venv];
    if let Some(p) = candidates.into_iter().flatten().find(|p| p.is_file()) {
        return Some(p);
    }
    Command::new("wsgidav").arg("--version").output().ok().filter(|o| o.status.success()).map(|_| PathBuf::from("wsgidav"))
}

/// A wsgidav server sharing a folder anonymously; killed on drop.
struct Server {
    child: Child,
    addr: SocketAddr,
}

impl Server {
    fn start(root: &Path) -> Option<Server> {
        let Some(exe) = wsgidav() else {
            eprintln!("skipped: no wsgidav");
            return None;
        };
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let child = Command::new(exe)
            .args(["-H", "127.0.0.1", "-p", &port.to_string(), "--auth", "anonymous", "--no-config", "-q", "-q", "-r"])
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = Server { child, addr: SocketAddr::from(([127, 0, 0, 1], port)) };
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect_timeout(&server.addr, Duration::from_millis(200)).is_err() {
            assert!(Instant::now() < deadline, "wsgidav did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(server)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A file on the server, read with one ranged GET per position (like a
/// WebDAV or S3 file system's read stream), with socket timeouts.
struct HttpStream {
    addr: SocketAddr,
    path: String,
    size: u64,
    pos: u64,
    body: Option<io::Take<BufReader<TcpStream>>>,
    requests: Arc<AtomicUsize>,
}

impl HttpStream {
    fn new(server: &Server, path: &str, size: u64) -> (HttpStream, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let s = HttpStream { addr: server.addr, path: path.into(), size, pos: 0, body: None, requests: requests.clone() };
        (s, requests)
    }

    fn request(&mut self) -> io::Result<()> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let mut sock = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))?;
        sock.set_read_timeout(Some(Duration::from_secs(10)))?;
        sock.set_write_timeout(Some(Duration::from_secs(10)))?;
        write!(sock, "GET {} HTTP/1.1\r\nHost: {}\r\nRange: bytes={}-\r\nConnection: close\r\n\r\n", self.path, self.addr, self.pos)?;
        let mut reader = BufReader::new(sock);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status: u16 = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut length = None;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':')
                && k.eq_ignore_ascii_case("content-length")
            {
                length = v.trim().parse::<u64>().ok();
            }
        }
        if status != 206 && !(status == 200 && self.pos == 0) {
            return Err(io::Error::other(format!("HTTP {status}")));
        }
        self.body = Some(reader.take(length.unwrap_or(self.size - self.pos)));
        Ok(())
    }
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        if self.body.is_none() {
            self.request()?;
        }
        let n = self.body.as_mut().map_or(Ok(0), |b| b.read(buf))?;
        self.pos += n as u64;
        if n == 0 {
            self.body = None;
        }
        Ok(n)
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.size as i64 + d,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "before the start"));
        }
        if new as u64 != self.pos {
            if let Some(b) = self.body.take() {
                let _ = b.into_inner().into_inner().shutdown(Shutdown::Both);
            }
            self.pos = new as u64;
        }
        Ok(self.pos)
    }
}

impl ReadStream for HttpStream {}

fn share() -> (tempfile::TempDir, Option<Server>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("media")).unwrap();
    for name in ["clip-160x90.mp4", "tone-then-silence.mp3"] {
        std::fs::copy(data(name), dir.path().join("media").join(name)).unwrap();
    }
    let server = Server::start(dir.path());
    (dir, server)
}

fn size(name: &str) -> u64 {
    std::fs::metadata(data(name)).unwrap().len()
}

#[test]
fn plays_seeks_and_probes_over_http() {
    let (_dir, Some(server)) = share() else { return };
    let name = "clip-160x90.mp4";

    // Metadata on the loader's thread; the stream starts over for the player.
    let (mut stream, _) = HttpStream::new(&server, "/media/clip-160x90.mp4", size(name));
    let info = probe_stream(&mut stream, Some(size(name)), name).unwrap();
    assert_eq!(info.title.as_deref(), Some("Test Clip"));
    assert_eq!(stream.stream_position().unwrap(), 0);

    let (stream, requests) = HttpStream::new(&server, "/media/clip-160x90.mp4", size(name));
    let options = PlayerOptions { output: AudioOutput::Null, ..PlayerOptions::default() };
    let p = Player::open(MediaSource::stream(Box::new(stream), Some(size(name)), name), options);
    let info = wait_loaded(&p);
    assert!(info.seekable && info.has_video());
    p.seek(Duration::from_millis(1200));
    assert!(wait_until(Duration::from_secs(5), || p.video_frame().is_some_and(|f| (f.pts.as_millis() as i64 - 1200).abs() <= 100)));
    p.play();
    assert!(wait_for_event(&p, Duration::from_secs(5), |e| *e == Event::Finished).is_some());
    // Read-ahead: a handful of requests, not one per FFmpeg read.
    let n = requests.load(Ordering::Relaxed);
    eprintln!("{n} HTTP requests");
    assert!((1..=12).contains(&n), "{n} requests");
    let watch = p.shutdown_watch();
    drop(p);
    assert!(watch.wait(Duration::from_secs(3)));
}

#[test]
fn waveform_over_http() {
    let (_dir, Some(server)) = share() else { return };
    let name = "tone-then-silence.mp3";
    let (stream, _) = HttpStream::new(&server, "/media/tone-then-silence.mp3", size(name));
    let a = WaveformAnalyzer::start(MediaSource::stream(Box::new(stream), Some(size(name)), name), WaveformOptions::default());
    assert!(a.wait_complete(Duration::from_secs(10)));
    let l = a.snapshot();
    assert_eq!(l.error, "");
    assert!((l.decoded_ms() - 2000).abs() <= 5 * oc_media::BLOCK_MS, "{}", l.decoded_ms());
    let third = l.peaks.len() / 3;
    assert!(l.peaks[5..third].iter().all(|&p| p > 0.3));
    assert!(l.peaks[l.peaks.len() - third..].iter().all(|&p| p < 0.01));
}

#[test]
fn a_missing_file_is_an_error_not_a_hang() {
    let (_dir, Some(server)) = share() else { return };
    let (stream, _) = HttpStream::new(&server, "/media/gone.mp4", 100_000);
    let options = PlayerOptions { output: AudioOutput::Null, autoplay: true, ..PlayerOptions::default() };
    let p = Player::open(MediaSource::stream(Box::new(stream), Some(100_000), "gone.mp4"), options);
    let seen = wait_for_event(&p, Duration::from_secs(10), |e| matches!(e, Event::Error(_))).expect("no error");
    assert!(matches!(seen.last(), Some(Event::Error(m)) if !m.is_empty()));
}

#[test]
fn seeks_across_read_ahead_blocks() {
    // Several MB with the index (moov) at the end: opening seeks to the end
    // and back, seeking jumps blocks; each is a new range request.
    let (dir, Some(server)) = share() else { return };
    let big = dir.path().join("media/big.mp4");
    let args = ["-f", "lavfi", "-i", "testsrc2=s=640x480:d=4:r=25", "-c:v", "mjpeg", "-q:v", "2", big.to_str().unwrap()];
    if ffmpeg(&args).is_none() {
        return;
    }
    let len = std::fs::metadata(&big).unwrap().len();
    assert!(len > 2 * 1024 * 1024, "{len}");
    let (stream, requests) = HttpStream::new(&server, "/media/big.mp4", len);
    let options = PlayerOptions { output: AudioOutput::Null, video_size: Some((160, 120)), ..PlayerOptions::default() };
    let p = Player::open(MediaSource::stream(Box::new(stream), Some(len), "big.mp4"), options);
    let info = wait_loaded(&p);
    assert_eq!(info.video.as_ref().map(|v| (v.width, v.height)), Some((640, 480)));
    for target in [3000i64, 500, 2500] {
        p.seek(Duration::from_millis(target as u64));
        assert!(
            wait_until(Duration::from_secs(5), || p.video_frame().is_some_and(|f| (f.pts.as_millis() as i64 - target).abs() <= 40 && f.width == 160)),
            "no frame at {target} ms"
        );
    }
    let n = requests.load(Ordering::Relaxed);
    assert!((3..=20).contains(&n), "{n} requests");
}
