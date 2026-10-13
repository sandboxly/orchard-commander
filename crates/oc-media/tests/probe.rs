//! Metadata of real media files.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use oc_media::{MediaSource, probe, probe_stream};
use oc_vfs::FileSystem;
use oc_vfs::testing::MemFS;

#[test]
fn video_metadata() {
    let info = probe(&data("clip-160x90.mp4")).unwrap();
    assert_eq!(info.title.as_deref(), Some("Test Clip"));
    assert_eq!(info.display_title("clip.mp4"), "Test Clip");
    let d = info.duration.unwrap();
    assert!((d.as_secs_f64() - 2.0).abs() < 0.05, "{d:?}");
    let v = info.video.as_ref().unwrap();
    assert_eq!((v.width, v.height, v.display_width, v.display_height), (160, 90, 160, 90));
    assert_eq!(v.codec, "h264");
    assert!((v.fps.unwrap() - 10.0).abs() < 0.01);
    let a = info.audio.as_ref().unwrap();
    assert_eq!((a.codec.as_str(), a.sample_rate, a.channels), ("aac", 44100, 1));
    assert!(info.has_video() && info.seekable);
    assert!(info.container.contains("mp4"));
}

#[test]
fn audio_metadata_and_tags() {
    let info = probe(&data("tone-then-silence.mp3")).unwrap();
    assert!(!info.has_video());
    assert_eq!(info.audio.as_ref().unwrap().sample_rate, 22050);
    assert!((info.duration.unwrap().as_secs_f64() - 2.0).abs() < 0.1);
    assert_eq!(info.display_title("tone.mp3"), "tone.mp3");

    let dir = tempfile::tempdir().unwrap();
    let tagged = dir.path().join("tagged.mp3");
    let src = data("tone-then-silence.mp3");
    let meta = ["-metadata", "title=Song", "-metadata", "artist=Singer", "-metadata", "album=Album", "-metadata", "album_artist=Band"];
    let mut args = vec!["-i", src.to_str().unwrap(), "-c", "copy"];
    args.extend(meta);
    args.push(tagged.to_str().unwrap());
    if ffmpeg(&args).is_none() {
        return;
    }
    let info = probe(&tagged).unwrap();
    assert_eq!(info.display_title("x"), "Song");
    assert_eq!(info.display_subtitle(), "Singer · Album");
    assert_eq!(info.album_artist.as_deref(), Some("Band"));
}

#[test]
fn tags_on_the_audio_stream_ogg_opus() {
    let dir = tempfile::tempdir().unwrap();
    let ogg = dir.path().join("t.opus");
    let wav = tone_wav(dir.path());
    let args = ["-i", wav.to_str().unwrap(), "-c:a", "libopus", "-metadata", "title=Vorbis Song", "-metadata", "ARTIST=Vorbis Artist", ogg.to_str().unwrap()];
    if ffmpeg(&args).is_none() {
        return;
    }
    let info = probe(&ogg).unwrap();
    assert_eq!(info.title.as_deref(), Some("Vorbis Song"));
    assert_eq!(info.artist.as_deref(), Some("Vorbis Artist"));
}

#[test]
fn wma_tags() {
    let dir = tempfile::tempdir().unwrap();
    let wma = dir.path().join("t.wma");
    let wav = tone_wav(dir.path());
    let args = ["-i", wav.to_str().unwrap(), "-c:a", "wmav2", "-metadata", "title=WMA Song", "-metadata", "artist=WMA Artist", "-metadata", "album=WMA Album", wma.to_str().unwrap()];
    if ffmpeg(&args).is_none() {
        return;
    }
    let info = probe(&wma).unwrap();
    assert_eq!(info.display_title("x"), "WMA Song");
    assert_eq!(info.display_subtitle(), "WMA Artist · WMA Album");
    assert!((info.duration.unwrap().as_secs_f64() - 2.0).abs() < 0.2);
}

#[test]
fn cover_art_is_not_video() {
    let dir = tempfile::tempdir().unwrap();
    let cover = dir.path().join("cover.png");
    let out = dir.path().join("with-cover.mp3");
    let src = data("tone-then-silence.mp3");
    if ffmpeg(&["-f", "lavfi", "-i", "color=c=red:s=32x32:d=1", "-frames:v", "1", cover.to_str().unwrap()]).is_none() {
        return;
    }
    ffmpeg(&[
        "-i", src.to_str().unwrap(), "-i", cover.to_str().unwrap(), "-map", "0:a", "-map", "1:v", "-c:a", "copy", "-c:v", "png",
        "-disposition:v", "attached_pic", out.to_str().unwrap(),
    ])
    .unwrap();
    let info = probe(&out).unwrap();
    assert!(!info.has_video(), "{info:?}");
    assert!(info.audio.is_some());
}

#[test]
fn non_square_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("anamorphic.mkv");
    let args = ["-f", "lavfi", "-i", "testsrc=s=160x120:d=0.5:r=10", "-vf", "setsar=2/1", "-c:v", "mpeg4", out.to_str().unwrap()];
    if ffmpeg(&args).is_none() {
        return;
    }
    let v = probe(&out).unwrap().video.unwrap();
    assert_eq!((v.width, v.height, v.display_width, v.display_height), (160, 120, 320, 120));
}

#[test]
fn other_containers_get_resolution_and_duration() {
    // The rename metadata gaps of 1.x (WMV, FLV, MPEG): FFmpeg knows them all.
    let dir = tempfile::tempdir().unwrap();
    for (name, codec) in [("a.wmv", "wmv2"), ("a.flv", "flv1"), ("a.mpg", "mpeg2video")] {
        let out = dir.path().join(name);
        let args = ["-f", "lavfi", "-i", "testsrc=s=64x48:d=1:r=25", "-c:v", codec, out.to_str().unwrap()];
        if ffmpeg(&args).is_none() {
            return;
        }
        let info = probe(&out).unwrap();
        let v = info.video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (64, 48), "{name}");
        // FFmpeg estimates MPEG-PS durations from the bit rate (ffprobe agrees).
        let tolerance = if name.ends_with(".mpg") { 0.5 } else { 0.15 };
        assert!((info.duration.unwrap().as_secs_f64() - 1.0).abs() < tolerance, "{name}: {:?}", info.duration);
    }
}

#[test]
fn probing_a_stream_rewinds_it() {
    let bytes = std::fs::read(data("clip-160x90.mp4")).unwrap();
    let mem = Arc::new(MemFS::new());
    mem.add_file("dir/clip.mp4", bytes.clone());
    let mut stream = mem.open_read("dir/clip.mp4").unwrap();
    let info = probe_stream(&mut *stream, Some(bytes.len() as u64), "clip.mp4").unwrap();
    assert_eq!(info.video.unwrap().width, 160);
    let mut again = Vec::new();
    std::io::Read::read_to_end(&mut stream, &mut again).unwrap();
    assert_eq!(again, bytes); // handed on from the start

    // Unseekable: metadata still; seeking isn't offered.
    let mut piped = Unseekable(std::io::Cursor::new(bytes));
    let info = probe_stream(&mut piped, None, "clip.mp4").unwrap();
    assert!(!info.seekable && info.has_video());
    assert!(!MediaSource::stream(Box::new(Unseekable(std::io::Cursor::new(vec![]))), None, "x").seekable());
}

/// Counts the bytes read through it.
struct Counting<R>(R, Arc<std::sync::atomic::AtomicU64>);

impl<R: std::io::Read> std::io::Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        self.1.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: std::io::Seek> std::io::Seek for Counting<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

impl<R: std::io::Read + std::io::Seek + Send> oc_vfs::ReadStream for Counting<R> {}

#[test]
fn probing_streams_reads_little() {
    use std::sync::atomic::{AtomicU64, Ordering};
    // A big MP4 with its index (moov) at the end: found by seeking, not reading it all.
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.mp4");
    let args = ["-f", "lavfi", "-i", "testsrc2=s=640x480:d=8:r=25", "-c:v", "mjpeg", "-q:v", "2", big.to_str().unwrap()];
    if ffmpeg(&args).is_none() {
        return;
    }
    let bytes = std::fs::read(&big).unwrap();
    assert!(bytes.len() > 3 * 1024 * 1024);
    let read = Arc::new(AtomicU64::new(0));
    let mut s = Counting(std::io::Cursor::new(bytes.clone()), read.clone());
    let info = probe_stream(&mut s, Some(bytes.len() as u64), "big.mp4").unwrap();
    assert_eq!(info.video.unwrap().width, 640);
    assert!(read.load(Ordering::Relaxed) < 1024 * 1024, "{} bytes", read.load(Ordering::Relaxed));

    // Garbage named like a video: gives up within the budget.
    let garbage: Vec<u8> = (0..8_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let read = Arc::new(AtomicU64::new(0));
    let mut s = Counting(std::io::Cursor::new(garbage), read.clone());
    assert!(probe_stream(&mut s, Some(8_000_000), "huge.mp4").is_err());
    assert!(read.load(Ordering::Relaxed) <= oc_media::PROBE_BUDGET, "{} bytes", read.load(Ordering::Relaxed));
    // Rewound all the same: the player gets the stream next.
    assert_eq!(std::io::Seek::stream_position(&mut s).unwrap(), 0);
}

#[test]
fn garbage_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("garbage.mp3");
    std::fs::write(&bad, b"\x00\x01not audio".repeat(300)).unwrap();
    let e = probe(&bad).unwrap_err().to_string();
    assert!(!e.is_empty());
    assert!(probe(&dir.path().join("missing.mp4")).is_err());
    let _ = Duration::ZERO;
}
