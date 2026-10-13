//! Audio output: the system's default device (cpal), or a silent sink that
//! consumes audio in real time (tests, machines without a device).
//!
//! Every device stream lives on one audio thread for the whole process. It
//! owns the cpal host (on Windows the COM apartment that watches the default
//! device lives on the thread that first touches cpal, and must outlive
//! every player), serves all players, pauses a stream while its player is
//! paused, and follows the system's default output device: when it changes
//! (headphones connecting, a switch in the system settings) or a stream
//! breaks, the stream is rebuilt on the new default. It checks every 1.5 s,
//! and at once when the system reports a route change. Without a device a
//! player keeps time silently and the thread keeps looking for one.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use super::audio::AudioShared;
use crate::threads::ShutdownWatch;

/// Where the audio goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioOutput {
    /// The system's default output device (plays silently while there is
    /// none, and moves to one when it appears).
    #[default]
    Default,
    /// Nowhere: audio is consumed in real time and the clock runs, silently.
    Null,
}

/// The null sink's format.
const NULL_FORMAT: (u32, u16) = (48000, 2);
/// How long a player waits at start for its device before decoding for the
/// silent sink (it switches when the device opens later).
#[cfg_attr(not(feature = "output"), allow(dead_code))]
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the default device is compared with the open one, and how
/// often a missing device is looked for.
#[cfg_attr(not(feature = "output"), allow(dead_code))]
const FOLLOW_EVERY: Duration = Duration::from_millis(1500);

/// Consumes audio at the real-time rate without a device.
struct NullSink {
    rate: u32,
    channels: u16,
    last: Instant,
    buf: Vec<f32>,
}

impl NullSink {
    fn new((rate, channels): (u32, u16)) -> Self {
        NullSink { rate, channels, last: Instant::now(), buf: Vec::new() }
    }

    fn tick(&mut self, audio: &AudioShared) {
        if !audio.playing.load(Ordering::Acquire) {
            self.last = Instant::now();
            return;
        }
        let elapsed = self.last.elapsed().min(Duration::from_millis(100));
        let frames = (elapsed.as_secs_f64() * self.rate as f64) as usize;
        if frames == 0 {
            return;
        }
        self.last += Duration::from_secs_f64(frames as f64 / self.rate as f64);
        if self.last < Instant::now() - Duration::from_millis(100) {
            self.last = Instant::now(); // stalled (a sleeping machine): don't catch up
        }
        self.buf.resize(frames * self.channels as usize, 0.0);
        audio.render(&mut self.buf, self.channels as usize, 0.0);
    }
}

fn run_null(audio: &AudioShared) {
    let mut sink = NullSink::new(NULL_FORMAT);
    while !audio.stop.load(Ordering::Acquire) {
        sink.tick(audio);
        thread::park_timeout(Duration::from_millis(10));
    }
}

/// Start the player's output. Afterwards [`AudioShared::format`] is the
/// format the decoder must deliver; it changes (with the generation) when
/// the output moves to another device.
pub(crate) fn start(kind: AudioOutput, audio: &Arc<AudioShared>, watch: &ShutdownWatch) {
    #[cfg(feature = "output")]
    if kind == AudioOutput::Default {
        match device::register(audio) {
            Ok(()) => return,
            Err(e) => log::warn!("audio thread: {e}; playing silently"),
        }
    }
    let _ = kind;
    start_null(audio, watch);
}

fn start_null(audio: &Arc<AudioShared>, watch: &ShutdownWatch) {
    audio.set_format(NULL_FORMAT.0, NULL_FORMAT.1);
    let a = audio.clone();
    let spawned = watch.spawn("oc-media null audio", move || {
        a.set_output_thread(Some(thread::current()));
        run_null(&a);
    });
    if let Err(e) = spawned {
        log::warn!("audio thread: {e}");
    }
}

/// What following the default device needs from the system: cpal in the
/// app, a scripted fake in the tests.
trait Backend {
    type Stream: DeviceStream;
    /// The system's default output device, if there is one.
    fn default_id(&mut self) -> Option<String>;
    /// Open a stream on the default device, paused, rendering from `audio`.
    fn open(&mut self, audio: &Arc<AudioShared>) -> Result<Self::Stream, String>;
}

trait DeviceStream {
    fn id(&self) -> Option<&str>;
    fn format(&self) -> (u32, u16);
    /// The stream stopped working (device gone, stream invalidated).
    fn broken(&self) -> bool;
    /// The system reported a route change since the last call.
    fn take_changed(&self) -> bool;
    fn set_playing(&mut self, playing: bool) -> Result<(), String>;
}

/// One player's output on the audio thread: its device stream, or the
/// silent sink while there is none.
struct Follower<S> {
    audio: Arc<AudioShared>,
    current: Option<(S, bool)>,
    null: NullSink,
    last_check: Option<Instant>,
    /// Why the last open failed (logged once per outage, not per retry).
    last_error: Option<String>,
}

impl<S: DeviceStream> Follower<S> {
    fn new(audio: Arc<AudioShared>) -> Self {
        Follower { audio, current: None, null: NullSink::new(NULL_FORMAT), last_check: None, last_error: None }
    }

    fn stopped(&self) -> bool {
        self.audio.stop.load(Ordering::Acquire)
    }

    fn has_device(&self) -> bool {
        self.current.is_some()
    }

    /// Follow the default device and the play state; `now` is when to judge
    /// whether a check is due.
    fn step<B: Backend<Stream = S>>(&mut self, backend: &mut B, now: Instant) {
        let due = self.last_check.is_none_or(|t| now.duration_since(t) >= FOLLOW_EVERY);
        let rebuild = match &self.current {
            Some((s, _)) if s.broken() => Some("the stream stopped"),
            Some((s, _)) => {
                let changed = s.take_changed();
                if due || changed {
                    self.last_check = Some(now);
                    // No default at all is usually a moment in a handover:
                    // keep the working stream until there is a new one.
                    match backend.default_id() {
                        Some(id) if Some(id.as_str()) != s.id() => Some("the default device changed"),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            None if due => Some("looking for a device"),
            None => None,
        };
        if let Some(why) = rebuild {
            self.last_check = Some(now);
            self.reopen(backend, why);
        }
        let playing = self.audio.playing.load(Ordering::Acquire);
        match &mut self.current {
            Some((s, on)) => {
                if playing != *on {
                    if let Err(e) = s.set_playing(playing) {
                        log::debug!("audio output: {e}");
                    }
                    *on = playing;
                }
            }
            None => self.null.tick(&self.audio),
        }
    }

    fn reopen<B: Backend<Stream = S>>(&mut self, backend: &mut B, why: &str) {
        let old = self.current.take().map(|(s, _)| s); // stops the old stream before opening a new one
        let old_id = old.as_ref().map(|s| s.id().unwrap_or("?").to_string());
        drop(old);
        match backend.open(&self.audio) {
            Ok(s) => {
                let (rate, channels) = s.format();
                log::info!("audio output: {} → {} ({rate} Hz, {channels} channels; {why})", old_id.as_deref().unwrap_or("none"), s.id().unwrap_or("?"));
                self.audio.set_format(rate, channels);
                self.current = Some((s, false));
                self.last_error = None;
            }
            Err(e) => {
                if old_id.is_some() || self.last_error.as_deref() != Some(e.as_str()) {
                    log::info!("audio output: {} → none ({e}; {why}): playing silently", old_id.as_deref().unwrap_or("none"));
                }
                self.last_error = Some(e);
                self.audio.set_format(NULL_FORMAT.0, NULL_FORMAT.1);
                self.null = NullSink::new(NULL_FORMAT);
            }
        }
    }
}

#[cfg(feature = "output")]
mod device {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, OnceLock, mpsc};

    /// A player joining the audio thread; `ready` hears when its device has
    /// been tried.
    struct Join {
        audio: Arc<AudioShared>,
        ready: mpsc::SyncSender<()>,
    }

    struct Service {
        joins: Mutex<mpsc::Sender<Join>>,
        thread: thread::Thread,
    }

    static SERVICE: OnceLock<Result<Service, String>> = OnceLock::new();

    fn service() -> Result<&'static Service, String> {
        SERVICE
            .get_or_init(|| {
                let (tx, rx) = mpsc::channel();
                // Never ends: the cpal host (and on Windows its COM
                // apartment) must outlive every player.
                thread::Builder::new()
                    .name("oc-media audio".into())
                    .spawn(move || serve(Cpal { host: cpal::default_host() }, rx))
                    .map(|h| Service { joins: Mutex::new(tx), thread: h.thread().clone() })
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| e.clone())
    }

    /// Hand the player's output to the audio thread and wait (a while) for
    /// its device.
    pub(super) fn register(audio: &Arc<AudioShared>) -> Result<(), String> {
        let service = service()?;
        audio.set_output_thread(Some(service.thread.clone()));
        let (ready, answer) = mpsc::sync_channel(1);
        service.joins.lock().unwrap_or_else(|e| e.into_inner()).send(Join { audio: audio.clone(), ready }).map_err(|e| e.to_string())?;
        service.thread.unpark();
        if answer.recv_timeout(OPEN_TIMEOUT).is_err() {
            log::info!("audio device not ready after {OPEN_TIMEOUT:?}: playing silently until it is");
        }
        Ok(())
    }

    fn serve<B: Backend>(mut backend: B, joins: mpsc::Receiver<Join>) {
        let mut players: Vec<Follower<B::Stream>> = Vec::new();
        loop {
            while let Ok(join) = joins.try_recv() {
                let mut f = Follower::new(join.audio);
                f.step(&mut backend, Instant::now());
                let _ = join.ready.send(());
                players.push(f);
            }
            players.retain(|f| !f.stopped()); // drops their streams
            let now = Instant::now();
            for f in &mut players {
                f.step(&mut backend, now);
            }
            let wait = match players.len() {
                0 => Duration::from_secs(5),
                _ if players.iter().any(|f| !f.has_device()) => Duration::from_millis(10), // silent sinks keep time
                _ => Duration::from_millis(100),
            };
            thread::park_timeout(wait);
        }
    }

    struct Cpal {
        host: cpal::Host,
    }

    impl Backend for Cpal {
        type Stream = Open;

        fn default_id(&mut self) -> Option<String> {
            self.host.default_output_device().and_then(|d| d.id().ok()).map(|id| id.to_string())
        }

        fn open(&mut self, audio: &Arc<AudioShared>) -> Result<Open, String> {
            let device = self.host.default_output_device().ok_or("no output device")?;
            let supported = device.default_output_config().map_err(|e| e.to_string())?;
            let config = StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
            let flags = Flags::default();
            // f32 everywhere it's offered; the device's own format otherwise.
            let stream = build::<f32>(&device, config, audio, &flags)
                .or_else(|e| match supported.sample_format() {
                    SampleFormat::I16 => build::<i16>(&device, config, audio, &flags),
                    SampleFormat::I32 => build::<i32>(&device, config, audio, &flags),
                    SampleFormat::U16 => build::<u16>(&device, config, audio, &flags),
                    _ => Err(e),
                })
                .map_err(|e| e.to_string())?;
            // Streams start running on some systems: start paused, the
            // follower plays it when the player is playing.
            let _ = stream.pause();
            Ok(Open { stream, id: device.id().ok().map(|id| id.to_string()), format: (config.sample_rate, config.channels), flags })
        }
    }

    #[derive(Clone, Default)]
    struct Flags {
        broken: Arc<AtomicBool>,
        changed: Arc<AtomicBool>,
    }

    struct Open {
        stream: Stream,
        id: Option<String>,
        format: (u32, u16),
        flags: Flags,
    }

    impl DeviceStream for Open {
        fn id(&self) -> Option<&str> {
            self.id.as_deref()
        }

        fn format(&self) -> (u32, u16) {
            self.format
        }

        fn broken(&self) -> bool {
            self.flags.broken.load(Ordering::Acquire)
        }

        fn take_changed(&self) -> bool {
            self.flags.changed.swap(false, Ordering::AcqRel)
        }

        fn set_playing(&mut self, playing: bool) -> Result<(), String> {
            if playing { self.stream.play() } else { self.stream.pause() }.map_err(|e| e.to_string())
        }
    }

    fn latency(info: &cpal::OutputCallbackInfo) -> f64 {
        let ts = info.timestamp();
        ts.playback.checked_duration_since(ts.callback).map_or(0.0, |d| d.as_secs_f64().min(1.0))
    }

    fn build<T>(device: &cpal::Device, config: StreamConfig, audio: &Arc<AudioShared>, flags: &Flags) -> Result<Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels = config.channels as usize;
        let a = audio.clone();
        let mut scratch: Vec<f32> = Vec::new();
        let flags = flags.clone();
        let owner = thread::current(); // the audio thread
        device.build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                scratch.resize(data.len(), 0.0);
                a.render(&mut scratch, channels, latency(info));
                for (d, s) in data.iter_mut().zip(&scratch) {
                    *d = T::from_sample(*s);
                }
            },
            move |e: cpal::Error| {
                use cpal::ErrorKind::*;
                match e.kind() {
                    StreamInvalidated | DeviceNotAvailable => {
                        log::info!("audio output: {e}");
                        flags.broken.store(true, Ordering::Release);
                        owner.unpark();
                    }
                    // cpal may have rerouted the stream itself; the follower
                    // checks the default device now instead of in 1.5 s.
                    DeviceChanged => {
                        log::info!("audio output: {e}");
                        flags.changed.store(true, Ordering::Release);
                        owner.unpark();
                    }
                    _ => log::debug!("audio output: {e}"),
                }
            },
            Some(OPEN_TIMEOUT),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpectrumAnalyzer;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::AtomicBool;

    fn shared() -> Arc<AudioShared> {
        Arc::new(AudioShared::new(Arc::new(SpectrumAnalyzer::new()), 1.0, false))
    }

    #[test]
    fn the_null_sink_plays_in_real_time() {
        let audio = shared();
        let watch = ShutdownWatch::new();
        start(AudioOutput::Null, &audio, &watch);
        let (rate, ch) = audio.format();
        audio.push(0.0, &vec![0.0; rate as usize * ch as usize]); // 1 s
        audio.playing.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(300));
        let clock = audio.clock();
        assert!((0.2..0.5).contains(&clock), "{clock}");
        audio.stop.store(true, Ordering::Release);
        audio.kick_output();
        assert!(watch.wait(Duration::from_secs(1)));
    }

    /// The system as the tests script it.
    #[derive(Default)]
    struct System {
        /// The default device: (id, rate, channels).
        default: Option<(&'static str, u32, u16)>,
        /// Opens that fail before one succeeds.
        failing_opens: usize,
        opens: Vec<&'static str>,
        /// Streams still alive, with their play state.
        live: Vec<(&'static str, Rc<RefCell<bool>>)>,
        streams: VecDeque<FakeStream>,
    }

    #[derive(Clone)]
    struct FakeStream {
        id: &'static str,
        format: (u32, u16),
        broken: Rc<AtomicBool>,
        changed: Rc<AtomicBool>,
        playing: Rc<RefCell<bool>>,
        system: Rc<RefCell<System>>,
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            let p = self.playing.clone();
            if let Ok(mut s) = self.system.try_borrow_mut() {
                s.live.retain(|(_, q)| !Rc::ptr_eq(q, &p));
            }
        }
    }

    impl DeviceStream for FakeStream {
        fn id(&self) -> Option<&str> {
            Some(self.id)
        }
        fn format(&self) -> (u32, u16) {
            self.format
        }
        fn broken(&self) -> bool {
            self.broken.load(Ordering::Acquire)
        }
        fn take_changed(&self) -> bool {
            self.changed.swap(false, Ordering::AcqRel)
        }
        fn set_playing(&mut self, playing: bool) -> Result<(), String> {
            *self.playing.borrow_mut() = playing;
            Ok(())
        }
    }

    struct Fake(Rc<RefCell<System>>);

    impl Backend for Fake {
        type Stream = FakeStream;
        fn default_id(&mut self) -> Option<String> {
            self.0.borrow().default.map(|d| d.0.to_string())
        }
        fn open(&mut self, _audio: &Arc<AudioShared>) -> Result<FakeStream, String> {
            let mut sys = self.0.borrow_mut();
            let (id, rate, channels) = sys.default.ok_or("no output device")?;
            if sys.failing_opens > 0 {
                sys.failing_opens -= 1;
                return Err("device busy".into());
            }
            sys.opens.push(id);
            let playing = Rc::new(RefCell::new(false));
            sys.live.push((id, playing.clone()));
            let s = FakeStream { id, format: (rate, channels), broken: Rc::default(), changed: Rc::default(), playing, system: self.0.clone() };
            sys.streams.push_back(s.clone());
            Ok(s)
        }
    }

    struct Rig {
        sys: Rc<RefCell<System>>,
        backend: Fake,
        audio: Arc<AudioShared>,
        f: Follower<FakeStream>,
        now: Instant,
    }

    impl Rig {
        fn new(default: Option<(&'static str, u32, u16)>) -> Rig {
            let sys = Rc::new(RefCell::new(System { default, ..System::default() }));
            let audio = shared();
            Rig { backend: Fake(sys.clone()), f: Follower::new(audio.clone()), audio, sys, now: Instant::now() }
        }
        fn step(&mut self, after: Duration) {
            self.now += after;
            self.f.step(&mut self.backend, self.now);
        }
        fn set_default(&mut self, d: Option<(&'static str, u32, u16)>) {
            self.sys.borrow_mut().default = d;
        }
        /// The handle to the open stream with this id, without keeping it alive.
        fn last_stream(&self) -> FakeStream {
            let mut sys = self.sys.borrow_mut();
            let s = sys.streams.back().cloned().expect("a stream");
            sys.streams.clear();
            s
        }
        fn opens(&self) -> Vec<&'static str> {
            self.sys.borrow().opens.clone()
        }
        /// Ids of the streams still alive, and whether they play.
        fn live(&self) -> Vec<(&'static str, bool)> {
            self.sys.borrow().live.iter().map(|(id, p)| (*id, *p.borrow())).collect()
        }
        fn play(&self, on: bool) {
            self.audio.playing.store(on, Ordering::Release);
        }
    }

    const SPEAKERS: Option<(&str, u32, u16)> = Some(("speakers", 48000, 2));
    const HEADPHONES: Option<(&str, u32, u16)> = Some(("headphones", 44100, 2));
    const SOON: Duration = Duration::from_millis(100);

    #[test]
    fn opens_the_default_device_and_follows_play_and_pause() {
        let mut r = Rig::new(SPEAKERS);
        r.step(SOON);
        r.sys.borrow_mut().streams.clear();
        assert_eq!(r.live(), [("speakers", false)]);
        assert_eq!(r.audio.format(), (48000, 2));
        r.play(true);
        r.step(SOON);
        assert_eq!(r.live(), [("speakers", true)]);
        r.play(false);
        r.step(SOON);
        assert_eq!(r.live(), [("speakers", false)]);
    }

    #[test]
    fn moves_to_a_new_default_device_while_playing() {
        let mut r = Rig::new(SPEAKERS);
        r.play(true);
        r.step(SOON);
        let generation = r.audio.generation();
        r.set_default(HEADPHONES);
        r.step(SOON);
        assert_eq!(r.opens(), ["speakers"], "not before the next check");
        r.step(FOLLOW_EVERY);
        assert_eq!(r.opens(), ["speakers", "headphones"]);
        assert_eq!(r.live(), [("headphones", true)], "the old stream is gone, the new one plays");
        assert_eq!(r.audio.format(), (44100, 2));
        assert!(r.audio.generation() > generation, "the decoder resamples for the new device");
    }

    #[test]
    fn follows_the_device_while_paused_too() {
        let mut r = Rig::new(SPEAKERS);
        r.step(SOON);
        r.set_default(HEADPHONES);
        r.step(FOLLOW_EVERY);
        assert_eq!(r.live(), [("headphones", false)]);
        r.play(true);
        r.step(SOON);
        assert_eq!(r.live(), [("headphones", true)], "resuming plays on the new device right away");
    }

    #[test]
    fn a_reported_route_change_is_checked_at_once() {
        let mut r = Rig::new(SPEAKERS);
        r.play(true);
        r.step(SOON);
        let s = r.last_stream();
        r.set_default(HEADPHONES);
        s.changed.store(true, Ordering::Release);
        drop(s);
        r.step(SOON);
        assert_eq!(r.opens(), ["speakers", "headphones"]);
        assert_eq!(r.live(), [("headphones", true)]);
    }

    #[test]
    fn a_route_change_to_the_same_device_keeps_the_stream() {
        let mut r = Rig::new(SPEAKERS);
        r.play(true);
        r.step(SOON);
        let s = r.last_stream();
        s.changed.store(true, Ordering::Release);
        drop(s);
        r.step(SOON);
        r.step(FOLLOW_EVERY);
        assert_eq!(r.opens(), ["speakers"]);
    }

    #[test]
    fn a_broken_stream_is_rebuilt_at_once() {
        let mut r = Rig::new(SPEAKERS);
        r.play(true);
        r.step(SOON);
        let s = r.last_stream();
        s.broken.store(true, Ordering::Release);
        drop(s);
        r.step(SOON);
        assert_eq!(r.opens(), ["speakers", "speakers"]);
        assert_eq!(r.live(), [("speakers", true)]);
    }

    #[test]
    fn a_moment_without_a_default_device_keeps_the_stream() {
        let mut r = Rig::new(SPEAKERS);
        r.play(true);
        r.step(SOON);
        r.set_default(None); // a handover between devices
        r.step(FOLLOW_EVERY);
        assert_eq!(r.live(), [("speakers", true)]);
        r.set_default(HEADPHONES);
        r.step(FOLLOW_EVERY);
        assert_eq!(r.live(), [("headphones", true)]);
    }

    #[test]
    fn the_device_disappearing_plays_silently_until_another_appears() {
        let mut r = Rig::new(HEADPHONES);
        r.play(true);
        r.step(SOON);
        let s = r.last_stream();
        r.set_default(None);
        s.broken.store(true, Ordering::Release);
        drop(s);
        r.step(SOON);
        assert_eq!(r.live(), []);
        assert!(!r.f.has_device());
        assert_eq!(r.audio.format(), NULL_FORMAT);
        r.step(FOLLOW_EVERY);
        assert!(!r.f.has_device(), "still none");
        r.set_default(SPEAKERS);
        r.step(FOLLOW_EVERY);
        assert_eq!(r.live(), [("speakers", true)]);
        assert_eq!(r.audio.format(), (48000, 2));
    }

    #[test]
    fn a_failing_open_is_retried() {
        let mut r = Rig::new(HEADPHONES);
        r.sys.borrow_mut().failing_opens = 2; // waking up
        r.play(true);
        r.step(SOON);
        assert!(!r.f.has_device());
        r.step(SOON);
        assert!(!r.f.has_device(), "not retried in a busy loop");
        r.step(FOLLOW_EVERY);
        assert!(!r.f.has_device());
        r.step(FOLLOW_EVERY);
        assert_eq!(r.live(), [("headphones", true)]);
    }

    #[test]
    fn a_device_appearing_late_is_picked_up() {
        let mut r = Rig::new(None);
        r.step(SOON);
        assert!(!r.f.has_device());
        r.set_default(HEADPHONES);
        r.step(FOLLOW_EVERY);
        assert_eq!(r.live(), [("headphones", false)]);
        assert_eq!(r.audio.format(), (44100, 2));
    }

    #[test]
    fn silence_keeps_time_without_a_device() {
        let mut r = Rig::new(None);
        r.audio.push(0.0, &vec![0.0; 48000 * 2]); // 1 s
        r.play(true);
        for _ in 0..25 {
            thread::sleep(Duration::from_millis(10)); // the audio thread ticks every 10 ms
            r.step(Duration::from_millis(10));
        }
        let clock = r.audio.clock();
        assert!((0.15..0.5).contains(&clock), "{clock}");
    }
}
