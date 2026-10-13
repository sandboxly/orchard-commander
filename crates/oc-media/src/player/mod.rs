//! The media player: a decoder thread, an audio output thread, and a handle
//! the UI keeps.
//!
//! [`Player::open`] returns at once; the file is opened and probed on the
//! decoder thread ([`Event::Loaded`] or [`Event::Error`] follow). Calls like
//! [`Player::play`] or [`Player::seek`] only set state and send a command,
//! so they never block the UI. Dropping the player tells its threads to
//! stop and returns immediately; streams and the audio device are closed on
//! those threads.

mod audio;
mod engine;
mod output;
mod video;

pub use output::AudioOutput;
pub use video::VideoFrame;

use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use self::audio::AudioShared;
use self::video::FrameQueue;
use crate::threads::ShutdownWatch;
use crate::{MediaInfo, MediaSource, SpectrumAnalyzer, Waker};

/// Playing, paused, or stopped (not started, at the end, or failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

/// How far the file is: opening, ready, played to the end, or unplayable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaStatus {
    Loading,
    Ready,
    Ended,
    Invalid,
}

/// What the player reports; read with [`Player::poll_event`].
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Opened: tags, duration, whether there is video.
    Loaded(Box<MediaInfo>),
    StateChanged(PlaybackState),
    /// About ten times a second while playing, and after seeks.
    Position(Duration),
    /// Played to the end (not sent while looping): time to play the next file.
    Finished,
    /// The file can't be played; show [`crate::error_message`] of it.
    Error(String),
}

#[derive(Clone)]
pub struct PlayerOptions {
    pub output: AudioOutput,
    /// Start playing as soon as the file is open.
    pub autoplay: bool,
    pub muted: bool,
    /// 0..=1.
    pub volume: f32,
    /// Repeat at the end instead of finishing (see [`crate::should_loop`]).
    pub looping: bool,
    /// Scale video to fit this size (physical pixels); None: the video's own size.
    pub video_size: Option<(u32, u32)>,
    pub waker: Option<Waker>,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions { output: AudioOutput::Default, autoplay: false, muted: false, volume: 1.0, looping: false, video_size: None, waker: None }
    }
}

pub(crate) enum Command {
    Wake,
    Seek { serial: u64, pos: f64 },
    VideoSize(Option<(u32, u32)>),
}

/// The fallback clock (video without audio, or after the audio ended).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WallClock {
    base: f64,
    started: Option<Instant>,
}

impl WallClock {
    pub(crate) fn now(&self) -> f64 {
        self.base + self.started.map_or(0.0, |s| s.elapsed().as_secs_f64())
    }

    pub(crate) fn run(&mut self, on: bool) {
        match (on, self.started) {
            (true, None) => self.started = Some(Instant::now()),
            (false, Some(_)) => {
                self.base = self.now();
                self.started = None;
            }
            _ => {}
        }
    }

    pub(crate) fn set(&mut self, pos: f64) {
        self.base = pos;
        if self.started.is_some() {
            self.started = Some(Instant::now());
        }
    }
}

pub(crate) struct Status {
    pub(crate) state: PlaybackState,
    pub(crate) media: MediaStatus,
    pub(crate) info: Option<MediaInfo>,
    pub(crate) error: Option<String>,
    pub(crate) looping: bool,
    /// A seek not yet done by the decoder: the position reads as its target.
    pub(crate) seek_target: Option<f64>,
    pub(crate) seek_serial: u64,
    pub(crate) wall: WallClock,
    /// Time comes from the audio output (else from `wall`).
    pub(crate) audio_clock: bool,
    pub(crate) duration: Option<f64>,
}

pub(crate) struct Shared {
    pub(crate) audio: Arc<AudioShared>,
    pub(crate) video: Mutex<FrameQueue>,
    status: Mutex<Status>,
    pub(crate) abort: Arc<std::sync::atomic::AtomicBool>,
    events: Sender<Event>,
    waker: Option<Waker>,
}

impl Shared {
    pub(crate) fn status(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn frames(&self) -> MutexGuard<'_, FrameQueue> {
        self.video.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn wake(&self) {
        if let Some(w) = &self.waker {
            w();
        }
    }

    pub(crate) fn emit(&self, e: Event) {
        let _ = self.events.send(e);
        self.wake();
    }

    pub(crate) fn aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }

    /// Seconds from the start of the file.
    pub(crate) fn position_in(&self, st: &Status) -> f64 {
        let pos = match st.seek_target {
            Some(t) => t,
            None if st.audio_clock => self.audio.clock(),
            None => st.wall.now(),
        };
        match st.duration {
            Some(d) => pos.clamp(0.0, d),
            None => pos.max(0.0),
        }
    }

    pub(crate) fn position(&self) -> f64 {
        self.position_in(&self.status())
    }

    /// Start or stop the clocks and the output (state already decided).
    pub(crate) fn set_running(&self, st: &mut Status, running: bool) {
        self.audio.playing.store(running && st.media == MediaStatus::Ready, Ordering::Release);
        st.wall.run(running && st.media == MediaStatus::Ready);
        self.audio.kick_output();
    }

    pub(crate) fn set_state(&self, st: &mut Status, state: PlaybackState) {
        self.set_running(st, state == PlaybackState::Playing);
        if st.state != state {
            st.state = state;
            self.emit(Event::StateChanged(state));
        }
    }

    pub(crate) fn fail(&self, message: String) {
        let mut st = self.status();
        st.media = MediaStatus::Invalid;
        st.error = Some(message.clone());
        self.set_state(&mut st, PlaybackState::Stopped);
        drop(st);
        self.emit(Event::Error(message));
    }
}

/// Plays one file. See the module docs.
pub struct Player {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    events: Receiver<Event>,
    spectrum: Arc<SpectrumAnalyzer>,
    watch: ShutdownWatch,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Player({:?}, {:?})", self.state(), self.status())
    }
}

impl Player {
    /// Open `source` in the background (never blocks).
    pub fn open(source: MediaSource, options: PlayerOptions) -> Player {
        let spectrum = Arc::new(SpectrumAnalyzer::new());
        let (events_tx, events) = mpsc::channel();
        let (commands, commands_rx) = mpsc::channel();
        let state = if options.autoplay { PlaybackState::Playing } else { PlaybackState::Stopped };
        let shared = Arc::new(Shared {
            audio: Arc::new(AudioShared::new(spectrum.clone(), options.volume, options.muted)),
            video: Mutex::new(FrameQueue::new()),
            status: Mutex::new(Status {
                state,
                media: MediaStatus::Loading,
                info: None,
                error: None,
                looping: options.looping,
                seek_target: None,
                seek_serial: 0,
                wall: WallClock::default(),
                audio_clock: false,
                duration: None,
            }),
            abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            events: events_tx,
            waker: options.waker.clone(),
        });
        let watch = ShutdownWatch::new();
        let s = shared.clone();
        let w = watch.clone();
        let opts = options.clone();
        if let Err(e) = watch.spawn("oc-media player", move || engine::run(source, s, commands_rx, opts, w)) {
            shared.fail(e.to_string());
        }
        Player { shared, commands, events, spectrum, watch }
    }

    fn send(&self, c: Command) {
        let _ = self.commands.send(c);
    }

    // -- state ------------------------------------------------------------

    pub fn state(&self) -> PlaybackState {
        self.shared.status().state
    }

    pub fn status(&self) -> MediaStatus {
        self.shared.status().media
    }

    pub fn is_playing(&self) -> bool {
        self.state() == PlaybackState::Playing
    }

    /// Tags and stream facts, once loaded.
    pub fn info(&self) -> Option<MediaInfo> {
        self.shared.status().info.clone()
    }

    /// Why the file can't be played.
    pub fn error(&self) -> Option<String> {
        self.shared.status().error.clone()
    }

    pub fn has_video(&self) -> bool {
        self.shared.status().info.as_ref().is_some_and(MediaInfo::has_video)
    }

    pub fn duration(&self) -> Option<Duration> {
        self.shared.status().duration.map(Duration::from_secs_f64)
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.shared.position())
    }

    /// Whether seeking works (false for unseekable streams; true while loading).
    pub fn can_seek(&self) -> bool {
        self.shared.status().info.as_ref().is_none_or(|i| i.seekable)
    }

    // -- control ----------------------------------------------------------

    /// Play (from the start again after the end). Before the file is open,
    /// it starts playing once it is.
    pub fn play(&self) {
        let mut st = self.shared.status();
        match st.media {
            MediaStatus::Invalid => return,
            MediaStatus::Ended => {
                st.media = MediaStatus::Ready;
                self.seek_locked(&mut st, 0.0);
            }
            _ => {}
        }
        self.shared.set_state(&mut st, PlaybackState::Playing);
        drop(st);
        self.send(Command::Wake);
    }

    pub fn pause(&self) {
        let mut st = self.shared.status();
        if st.state == PlaybackState::Playing {
            self.shared.set_state(&mut st, PlaybackState::Paused);
        }
    }

    /// Space / K.
    pub fn toggle_play(&self) {
        if self.is_playing() { self.pause() } else { self.play() }
    }

    /// Stop and go back to the start.
    pub fn stop(&self) {
        let mut st = self.shared.status();
        self.shared.set_state(&mut st, PlaybackState::Stopped);
        if st.media == MediaStatus::Ready || st.media == MediaStatus::Ended {
            st.media = MediaStatus::Ready;
            self.seek_locked(&mut st, 0.0);
        }
    }

    fn seek_locked(&self, st: &mut Status, pos: f64) {
        if st.info.as_ref().is_some_and(|i| !i.seekable) {
            return;
        }
        let pos = st.duration.map_or(pos, |d| pos.min(d)).max(0.0);
        st.seek_serial += 1;
        st.seek_target = Some(pos);
        if st.media == MediaStatus::Ended {
            st.media = MediaStatus::Ready; // play() continues from here
        }
        self.send(Command::Seek { serial: st.seek_serial, pos });
    }

    /// Jump to `pos` (clamped to the duration). Ignored for unseekable streams.
    pub fn seek(&self, pos: Duration) {
        let mut st = self.shared.status();
        self.seek_locked(&mut st, pos.as_secs_f64());
    }

    /// Jump by `delta_ms` (← / → use ±[`crate::SEEK_STEP_MS`]), within 0..duration.
    pub fn seek_by(&self, delta_ms: i64) {
        let mut st = self.shared.status();
        let pos = self.shared.position_in(&st) + delta_ms as f64 / 1000.0;
        self.seek_locked(&mut st, pos);
    }

    pub fn volume(&self) -> f32 {
        self.shared.audio.volume()
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.audio.set_volume(v);
    }

    pub fn is_muted(&self) -> bool {
        self.shared.audio.muted()
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.audio.set_muted(muted);
    }

    /// M. Returns the new state (remember it with [`crate::MediaPrefs::set`]).
    pub fn toggle_mute(&self) -> bool {
        let m = !self.is_muted();
        self.set_muted(m);
        m
    }

    pub fn set_looping(&self, on: bool) {
        self.shared.status().looping = on;
    }

    pub fn is_looping(&self) -> bool {
        self.shared.status().looping
    }

    // -- output -----------------------------------------------------------

    /// The frame to show now (advances to the current clock). Compare
    /// [`VideoFrame::serial`] to upload only changed frames.
    pub fn video_frame(&self) -> Option<Arc<VideoFrame>> {
        let pos = self.shared.position();
        let mut q = self.shared.frames();
        q.advance(pos);
        q.current.clone()
    }

    /// How long until the next frame is due (schedule the next redraw).
    pub fn next_frame_in(&self) -> Option<Duration> {
        let pos = self.shared.position();
        self.shared.frames().next_due(pos).map(Duration::from_secs_f64)
    }

    /// Scale video to fit this size (physical pixels) from now on.
    pub fn set_video_size(&self, size: Option<(u32, u32)>) {
        self.send(Command::VideoSize(size));
    }

    /// The live spectrum of what's being played.
    pub fn spectrum(&self) -> &Arc<SpectrumAnalyzer> {
        &self.spectrum
    }

    /// The next event, if any (never blocks).
    pub fn poll_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Wait up to `timeout` for the next event (tests, tools).
    pub fn wait_event(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    /// The player's threads, to wait for them after dropping it.
    pub fn shutdown_watch(&self) -> ShutdownWatch {
        self.watch.clone()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.abort.store(true, Ordering::Release);
        self.shared.audio.playing.store(false, Ordering::Release);
        self.shared.audio.stop.store(true, Ordering::Release);
        self.shared.audio.kick_output();
        self.send(Command::Wake);
    }
}
