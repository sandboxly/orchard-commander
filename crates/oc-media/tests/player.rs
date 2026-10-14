//! The player with real media and a silent output (no audio device needed).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::*;
use oc_media::spectrum::band_edges;
use oc_media::{AudioOutput, Event, MediaSource, MediaStatus, PlaybackState, Player, PlayerOptions, SEEK_STEP_MS};
use oc_vfs::FileSystem;
use oc_vfs::testing::MemFS;

fn options() -> PlayerOptions {
    PlayerOptions { output: AudioOutput::Null, ..PlayerOptions::default() }
}

fn playing() -> PlayerOptions {
    PlayerOptions { autoplay: true, ..options() }
}

fn clip() -> MediaSource {
    MediaSource::Path(data("clip-160x90.mp4"))
}

fn first_frame(p: &Player) -> Arc<oc_media::VideoFrame> {
    let mut frame = None;
    assert!(wait_until(Duration::from_secs(5), || {
        frame = p.video_frame();
        frame.is_some()
    }));
    frame.unwrap()
}

#[test]
fn loads_metadata_and_the_first_frame() {
    let p = Player::open(clip(), options());
    assert_eq!(p.status(), MediaStatus::Loading); // open never blocks
    let info = wait_loaded(&p);
    assert_eq!(info.title.as_deref(), Some("Test Clip"));
    assert!(p.has_video());
    assert!((secs(p.duration().unwrap()) - 2.0).abs() < 0.05);
    assert_eq!((p.status(), p.state()), (MediaStatus::Ready, PlaybackState::Stopped));
    // Paused/stopped still shows the first frame, at the video's size.
    let f = first_frame(&p);
    assert_eq!((f.width, f.height, f.rgba.len()), (160, 90, 160 * 90 * 4));
    assert!(f.pts < Duration::from_millis(150));
    assert_eq!(p.position(), Duration::ZERO);
}

#[test]
fn scales_frames_in_the_decoder() {
    let p = Player::open(clip(), PlayerOptions { video_size: Some((80, 80)), ..options() });
    wait_loaded(&p);
    let f = first_frame(&p);
    assert_eq!((f.width, f.height, f.rgba.len()), (80, 45, 80 * 45 * 4));
    // The test source is a gray-ish picture: not all black.
    assert!(f.rgba.chunks(4).any(|px| px[0] > 16 || px[1] > 16 || px[2] > 16));
    assert!(f.rgba.chunks(4).all(|px| px[3] == 255));
    // A new size while paused: the frame on screen is decoded again.
    p.set_video_size(Some((32, 32)));
    assert!(wait_until(Duration::from_secs(5), || p.video_frame().is_some_and(|f| f.width == 32 && f.height == 18)));
}

#[test]
fn plays_in_real_time_and_pauses() {
    let p = Player::open(clip(), playing());
    wait_loaded(&p);
    assert_eq!(p.state(), PlaybackState::Playing);
    std::thread::sleep(Duration::from_millis(600));
    let pos = secs(p.position());
    assert!((0.3..1.0).contains(&pos), "{pos}");
    p.pause();
    assert_eq!(p.state(), PlaybackState::Paused);
    let paused_at = p.position();
    std::thread::sleep(Duration::from_millis(200));
    assert!((secs(p.position()) - secs(paused_at)).abs() < 0.03);
    p.toggle_play();
    assert!(p.is_playing());
    let seen: Vec<Event> = std::iter::from_fn(|| p.poll_event()).collect();
    let states: Vec<_> = seen.iter().filter(|e| matches!(e, Event::StateChanged(_))).collect();
    assert_eq!(states, [&Event::StateChanged(PlaybackState::Paused), &Event::StateChanged(PlaybackState::Playing)]);
    assert!(seen.iter().any(|e| matches!(e, Event::Position(_)))); // ticks while playing
}

#[test]
fn video_follows_the_audio_clock() {
    let p = Player::open(clip(), playing());
    wait_loaded(&p);
    std::thread::sleep(Duration::from_millis(300));
    let mut checked = 0;
    let mut clock = Vec::new();
    // Until enough checks or the clip's end, not a fixed time: on a starved
    // CI runner a 30 ms sleep takes 150 ms (each check still holds).
    let start = Instant::now();
    while checked < 10 && start.elapsed() < Duration::from_secs(5) {
        if let Some(f) = p.video_frame() {
            let pos = secs(p.position());
            clock.push(pos);
            if pos >= 1.7 {
                break;
            }
            if pos > 0.2 {
                let drift = pos - secs(f.pts);
                // The frame on screen is the one for now (10 fps: ≤ 0.1 s old).
                assert!((-0.01..0.25).contains(&drift), "frame at {:?}, clock at {pos}", f.pts);
                checked += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(checked >= 5, "{checked} checks, clock at {clock:?}");
}

#[test]
fn seeks_and_clamps() {
    let p = Player::open(clip(), options());
    wait_loaded(&p);
    p.seek(Duration::from_millis(1000));
    assert_eq!(p.position(), Duration::from_millis(1000)); // at once
    assert!(wait_until(Duration::from_secs(5), || p.video_frame().is_some_and(|f| (f.pts.as_millis() as i64 - 1000).abs() <= 100)));
    assert!((secs(p.position()) - 1.0).abs() < 0.01);
    p.seek_by(-SEEK_STEP_MS);
    assert_eq!(p.position(), Duration::ZERO);
    p.seek_by(SEEK_STEP_MS);
    assert!((secs(p.position()) - 2.0).abs() < 0.05); // the end, not beyond
    p.seek(Duration::from_millis(500));
    p.play();
    std::thread::sleep(Duration::from_millis(300));
    let pos = secs(p.position());
    assert!((0.6..1.2).contains(&pos), "{pos}");
}

#[test]
fn finishes_and_plays_again_from_the_start() {
    let p = Player::open(clip(), playing());
    wait_loaded(&p);
    p.seek(Duration::from_millis(1500));
    let seen = wait_for_event(&p, Duration::from_secs(5), |e| *e == Event::Finished).expect("no Finished");
    assert!(seen.contains(&Event::StateChanged(PlaybackState::Stopped)));
    assert_eq!((p.state(), p.status()), (PlaybackState::Stopped, MediaStatus::Ended));
    assert!(secs(p.position()) > 1.8);
    p.play();
    assert!(wait_until(Duration::from_secs(2), || p.position() < Duration::from_millis(500)));
    assert!(p.is_playing() && p.status() == MediaStatus::Ready);
}

#[test]
fn loops_without_finishing() {
    let p = Player::open(clip(), PlayerOptions { looping: true, ..playing() });
    wait_loaded(&p);
    p.seek(Duration::from_millis(1600));
    std::thread::sleep(Duration::from_millis(900));
    let mut finished = false;
    while let Some(e) = p.poll_event() {
        finished |= e == Event::Finished;
    }
    assert!(!finished);
    assert!(p.is_playing());
    assert!(secs(p.position()) < 1.0, "{:?}", p.position()); // round again
    p.set_looping(false);
    assert!(wait_for_event(&p, Duration::from_secs(4), |e| *e == Event::Finished).is_some());
}

#[test]
fn audio_only_plays_and_feeds_the_spectrum() {
    let dir = tempfile::tempdir().unwrap();
    let wav = write_wav(&dir.path().join("1k.wav"), 3.0, 0.0, 0.5, 1000.0, 44100, 2);
    let p = Player::open(MediaSource::Path(wav), PlayerOptions { muted: true, ..playing() });
    let info = wait_loaded(&p);
    assert!(!info.has_video() && p.video_frame().is_none());
    assert!(p.is_muted());
    assert!(wait_until(Duration::from_secs(3), || p.spectrum().levels(32).is_some()));
    std::thread::sleep(Duration::from_millis(150));
    let levels = p.spectrum().levels(32).unwrap();
    let edges = band_edges(32, p.spectrum().rate());
    let loudest = levels.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
    assert!(edges[loudest] <= 1000.0 && 1000.0 < edges[loudest + 1], "band {loudest} of {levels:?}");
    assert!(!p.toggle_mute() && !p.is_muted());
    p.set_volume(0.5);
    assert_eq!(p.volume(), 0.5);
}

/// Frames seen while playing to the end: (distinct frames, last pts).
fn play_through(p: &Player, timeout: Duration) -> (usize, Duration) {
    let mut serials = std::collections::HashSet::new();
    let mut last = Duration::ZERO;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(f) = p.video_frame() {
            serials.insert(f.serial);
            last = last.max(f.pts);
        }
        while let Some(e) = p.poll_event() {
            if e == Event::Finished {
                return (serials.len(), last);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    panic!("never finished: {p:?}");
}

#[test]
fn video_without_audio_runs_on_the_wall_clock() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("silent.mp4");
    let args = ["-f", "lavfi", "-i", "testsrc=s=64x48:d=1.5:r=10", "-c:v", "libx264", "-pix_fmt", "yuv420p", out.to_str().unwrap()];
    if ffmpeg(&args).is_none() {
        return;
    }
    let p = Player::open(MediaSource::Path(out), playing());
    let info = wait_loaded(&p);
    assert!(info.audio.is_none() && info.has_video());
    let t = Instant::now();
    let (frames, last) = play_through(&p, Duration::from_secs(5));
    assert!(t.elapsed() > Duration::from_millis(1200), "{:?}", t.elapsed()); // real time, not as fast as possible
    assert!(frames >= 12, "{frames} frames");
    assert!(last >= Duration::from_millis(1300));
}

#[test]
fn video_goes_on_after_the_audio_ends() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("short-audio.mp4");
    let args = [
        "-f", "lavfi", "-i", "testsrc=s=64x48:d=2:r=10", "-f", "lavfi", "-i", "sine=f=440:d=0.5",
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", out.to_str().unwrap(),
    ];
    if ffmpeg(&args).is_none() {
        return;
    }
    let p = Player::open(MediaSource::Path(out), playing());
    wait_loaded(&p);
    let (frames, last) = play_through(&p, Duration::from_secs(6));
    assert!(frames >= 16, "{frames} frames");
    assert!(last >= Duration::from_millis(1800), "{last:?}");
}

#[test]
fn mp3_positions_count_from_the_start() {
    // The MP3 starts at 0.05 s (encoder delay); positions start at 0.
    let p = Player::open(MediaSource::Path(data("tone-then-silence.mp3")), options());
    wait_loaded(&p);
    p.seek(Duration::from_millis(1900));
    p.play();
    assert!(wait_for_event(&p, Duration::from_secs(3), |e| *e == Event::Finished).is_some());
    assert!(secs(p.position()) > 1.85, "{:?}", p.position());
}

#[test]
fn streams_from_another_file_system() {
    let bytes = std::fs::read(data("clip-160x90.mp4")).unwrap();
    let mem = Arc::new(MemFS::new());
    mem.add_file("videos/clip.mp4", bytes.clone());
    let stream = mem.open_read("videos/clip.mp4").unwrap();
    let p = Player::open(MediaSource::stream(stream, Some(bytes.len() as u64), "clip.mp4"), options());
    let info = wait_loaded(&p);
    assert!(info.seekable && p.can_seek());
    p.seek(Duration::from_millis(1200));
    assert!(wait_until(Duration::from_secs(5), || p.video_frame().is_some_and(|f| (f.pts.as_millis() as i64 - 1200).abs() <= 100)));
    p.play();
    assert!(wait_for_event(&p, Duration::from_secs(4), |e| *e == Event::Finished).is_some());
}

#[test]
fn streams_from_inside_archives() {
    // A real VFS backend: members of a ZIP (stored: seekable; deflated: not).
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(data("clip-160x90.mp4"), dir.path().join("clip.mp4")).unwrap();
    let have_zip = std::process::Command::new("zip").arg("-v").output().is_ok_and(|o| o.status.success());
    if !have_zip {
        eprintln!("skipped: no zip command");
        return;
    }
    for (name, level) in [("stored.zip", "-0"), ("deflated.zip", "-9")] {
        let ok = std::process::Command::new("zip")
            .current_dir(dir.path())
            .args(["-q", level, name, "clip.mp4"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let root = oc_archive::open_archive(&oc_local::local(dir.path().join(name))).unwrap();
        let member = root.child(&["clip.mp4"]);
        let size = member.stat().unwrap().size;
        let stream = member.open_read().unwrap();
        let seekable = stream.seekable();
        let p = Player::open(MediaSource::stream(stream, Some(size), "clip.mp4"), playing());
        let info = wait_loaded(&p);
        assert_eq!(info.seekable, seekable, "{name}");
        assert_eq!(first_frame(&p).width, 160);
        assert!(wait_for_event(&p, Duration::from_secs(5), |e| *e == Event::Finished).is_some(), "{name}");
    }
}

#[test]
fn unseekable_streams_play_without_seeking() {
    let bytes = std::fs::read(data("tone-then-silence.mp3")).unwrap();
    let p = Player::open(MediaSource::stream(Box::new(Unseekable(std::io::Cursor::new(bytes))), None, "a.mp3"), playing());
    let info = wait_loaded(&p);
    assert!(!info.seekable && !p.can_seek());
    std::thread::sleep(Duration::from_millis(300));
    let before = secs(p.position());
    p.seek(Duration::from_millis(1800)); // ignored
    assert!(secs(p.position()) < 1.0 && secs(p.position()) >= before);
    assert!(wait_for_event(&p, Duration::from_secs(4), |e| *e == Event::Finished).is_some());
}

#[test]
fn errors_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("garbage.mp4");
    std::fs::write(&bad, b"\x00\x01not video".repeat(300)).unwrap();
    let p = Player::open(MediaSource::Path(bad), playing());
    let seen = wait_for_event(&p, Duration::from_secs(5), |e| matches!(e, Event::Error(_))).expect("no error");
    let Some(Event::Error(msg)) = seen.last() else { panic!() };
    assert!(!msg.is_empty());
    assert!(oc_media::error_message(msg).starts_with("Can't play this file:\n"));
    assert_eq!((p.status(), p.state()), (MediaStatus::Invalid, PlaybackState::Stopped));
    p.play(); // nothing happens
    assert_eq!(p.state(), PlaybackState::Stopped);
    assert!(p.shutdown_watch().wait(Duration::from_secs(2)));
}

#[test]
fn wakes_the_ui() {
    let woken = Arc::new(AtomicUsize::new(0));
    let w = woken.clone();
    let waker: oc_media::Waker = Arc::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    });
    let p = Player::open(clip(), PlayerOptions { waker: Some(waker), ..options() });
    wait_loaded(&p);
    assert!(wait_until(Duration::from_secs(2), || woken.load(Ordering::Relaxed) >= 2)); // loaded + first frame
}

#[test]
fn a_waker_may_take_the_uis_lock() {
    // Like egui: the UI holds its context lock while it reads and drives the
    // player, and the waker (request_repaint) takes that lock too. Wake-ups
    // from inside oc-media's locks, or from the UI's own calls, deadlocked.
    let ui = Arc::new(std::sync::Mutex::new(()));
    let u = ui.clone();
    let waker: oc_media::Waker = Arc::new(move || drop(u.lock().unwrap_or_else(|e| e.into_inner())));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let p = Player::open(clip(), PlayerOptions { waker: Some(waker), ..playing() });
        let mut finished = 0;
        let deadline = Instant::now() + Duration::from_secs(8);
        while finished < 3 && Instant::now() < deadline {
            let frame = ui.lock().unwrap(); // a UI frame, its lock held throughout
            std::thread::sleep(Duration::from_millis(20));
            let _ = (p.state(), p.position(), p.video_frame());
            while let Some(e) = p.poll_event() {
                match e {
                    Event::Loaded(_) => p.seek(Duration::from_millis(1700)),
                    Event::Finished => {
                        finished += 1;
                        p.seek(Duration::from_millis(1700));
                        p.toggle_play();
                    }
                    _ => {}
                }
            }
            drop(frame);
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = tx.send(finished);
    });
    let finished = rx.recv_timeout(Duration::from_secs(15)).expect("deadlocked");
    assert_eq!(finished, 3);
}

#[test]
fn an_ended_unseekable_stream_does_not_finish_again() {
    let bytes = std::fs::read(data("tone-then-silence.mp3")).unwrap();
    let p = Player::open(MediaSource::stream(Box::new(Unseekable(std::io::Cursor::new(bytes))), None, "a.mp3"), playing());
    assert!(wait_for_event(&p, Duration::from_secs(5), |e| *e == Event::Finished).is_some());
    p.play(); // can't go back to the start: nothing to play
    assert!(wait_for_event(&p, Duration::from_millis(300), |e| *e == Event::Finished).is_none());
    assert_eq!((p.state(), p.status()), (PlaybackState::Stopped, MediaStatus::Ended));
}

#[test]
fn dropping_never_stalls() {
    // While playing.
    let p = Player::open(clip(), playing());
    wait_loaded(&p);
    std::thread::sleep(Duration::from_millis(100));
    let watch = p.shutdown_watch();
    assert!(watch.running() >= 2); // decoder + audio output
    let t = Instant::now();
    drop(p);
    assert!(t.elapsed() < Duration::from_millis(20), "drop took {:?}", t.elapsed());
    assert!(watch.wait(Duration::from_secs(2)), "{watch:?}");

    // While a slow server is still delivering the file.
    let bytes = std::fs::read(data("clip-160x90.mp4")).unwrap();
    let slow = SlowStream::new(bytes, Duration::from_millis(300));
    let p = Player::open(MediaSource::stream(Box::new(slow), None, "clip.mp4"), playing());
    std::thread::sleep(Duration::from_millis(50));
    let watch = p.shutdown_watch();
    let t = Instant::now();
    drop(p);
    assert!(t.elapsed() < Duration::from_millis(20), "drop took {:?}", t.elapsed());
    assert!(watch.wait(Duration::from_secs(3)), "{watch:?}");

    // A server that stopped answering: the drop is still immediate.
    let bytes = std::fs::read(data("clip-160x90.mp4")).unwrap();
    let mut stuck = SlowStream::new(bytes, Duration::ZERO);
    stuck.stall_after = Some(0);
    let release = stuck.release.clone();
    let p = Player::open(MediaSource::stream(Box::new(stuck), None, "clip.mp4"), playing());
    std::thread::sleep(Duration::from_millis(50));
    let watch = p.shutdown_watch();
    let t = Instant::now();
    drop(p);
    assert!(t.elapsed() < Duration::from_millis(20));
    release.store(true, Ordering::Release); // the read returns (with an error) …
    assert!(watch.wait(Duration::from_secs(3)), "{watch:?}"); // … and the thread ends
}

/// Plays through the real default output device; run with `--ignored`.
#[test]
#[ignore = "needs an audio device"]
fn plays_on_the_default_device() {
    let p = Player::open(MediaSource::Path(data("tone-then-silence.mp3")), PlayerOptions { output: AudioOutput::Default, autoplay: true, muted: true, ..PlayerOptions::default() });
    wait_loaded(&p);
    assert!(wait_for_event(&p, Duration::from_secs(5), |e| *e == Event::Finished).is_some());
}
