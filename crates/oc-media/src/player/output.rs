//! Audio output: the system's default device (cpal), or a silent sink that
//! consumes audio in real time (tests, machines without a device).
//!
//! The output lives on its own thread, which owns the device stream (cpal
//! streams aren't `Send` everywhere), pauses the device while the player is
//! paused, and follows the system's default output device: when it changes
//! (AirPods connecting, a switch in Control Center) or the stream breaks, the
//! stream is rebuilt on the new default. Like 1.x it checks every 1.5 s while
//! playing.

use std::sync::Arc;
use std::sync::atomic::Ordering;
#[cfg(feature = "output")]
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::audio::AudioShared;
use crate::threads::ShutdownWatch;

/// Where the audio goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioOutput {
    /// The system's default output device (falls back to [`AudioOutput::Null`]
    /// when there is none).
    #[default]
    Default,
    /// Nowhere: audio is consumed in real time and the clock runs, silently.
    Null,
}

/// The null sink's format.
const NULL_FORMAT: (u32, u16) = (48000, 2);
/// How long to wait for a device to open before playing silently.
#[cfg(feature = "output")]
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "output")]
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

/// Start the output thread; returns the format the decoder must deliver.
pub(crate) fn start(kind: AudioOutput, audio: &Arc<AudioShared>, watch: &ShutdownWatch) -> (u32, u16) {
    #[cfg(feature = "output")]
    if kind == AudioOutput::Default {
        let (tx, rx) = mpsc::sync_channel(1);
        let a = audio.clone();
        // Set when we stop waiting: a device opening late must not play
        // alongside the silent sink that replaces it.
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = cancel.clone();
        let spawned = watch.spawn("oc-media audio", move || {
            a.set_output_thread(Some(thread::current()));
            device::run(&a, &c, tx);
        });
        match spawned.map_err(|e| e.to_string()).and_then(|()| rx.recv_timeout(OPEN_TIMEOUT).map_err(|e| e.to_string())) {
            Ok(Ok(format)) => {
                audio.set_format(format.0, format.1);
                return format;
            }
            Ok(Err(e)) | Err(e) => {
                log::info!("no audio device ({e}): playing silently");
                cancel.store(true, Ordering::Release);
            }
        }
    }
    let _ = kind;
    start_null(audio, watch)
}

fn start_null(audio: &Arc<AudioShared>, watch: &ShutdownWatch) -> (u32, u16) {
    audio.set_format(NULL_FORMAT.0, NULL_FORMAT.1);
    let a = audio.clone();
    let spawned = watch.spawn("oc-media null audio", move || {
        a.set_output_thread(Some(thread::current()));
        run_null(&a);
    });
    if let Err(e) = spawned {
        log::warn!("audio thread: {e}");
    }
    NULL_FORMAT
}

#[cfg(feature = "output")]
mod device {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
    use std::sync::atomic::AtomicBool;

    struct Open {
        stream: Stream,
        id: Option<String>,
        format: (u32, u16),
        broken: Arc<AtomicBool>,
        playing: bool,
    }

    fn latency(info: &cpal::OutputCallbackInfo) -> f64 {
        let ts = info.timestamp();
        ts.playback.checked_duration_since(ts.callback).map_or(0.0, |d| d.as_secs_f64().min(1.0))
    }

    fn build<T>(device: &cpal::Device, config: StreamConfig, audio: &Arc<AudioShared>, broken: &Arc<AtomicBool>) -> Result<Stream, cpal::Error>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels = config.channels as usize;
        let a = audio.clone();
        let mut scratch: Vec<f32> = Vec::new();
        let flag = broken.clone();
        let owner = thread::current();
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
                        flag.store(true, Ordering::Release);
                        owner.unpark();
                    }
                    _ => log::debug!("audio output: {e}"),
                }
            },
            Some(OPEN_TIMEOUT),
        )
    }

    fn open(host: &cpal::Host, audio: &Arc<AudioShared>) -> Result<Open, String> {
        let device = host.default_output_device().ok_or("no output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let config = StreamConfig { channels: supported.channels(), sample_rate: supported.sample_rate(), buffer_size: cpal::BufferSize::Default };
        let broken = Arc::new(AtomicBool::new(false));
        // f32 everywhere it's offered; the device's own format otherwise.
        let stream = build::<f32>(&device, config, audio, &broken)
            .or_else(|e| match supported.sample_format() {
                SampleFormat::I16 => build::<i16>(&device, config, audio, &broken),
                SampleFormat::I32 => build::<i32>(&device, config, audio, &broken),
                SampleFormat::U16 => build::<u16>(&device, config, audio, &broken),
                _ => Err(e),
            })
            .map_err(|e| e.to_string())?;
        Ok(Open {
            stream,
            id: device.id().ok().map(|id| id.to_string()),
            format: (config.sample_rate, config.channels),
            broken,
            playing: false,
        })
    }

    fn default_id(host: &cpal::Host) -> Option<String> {
        host.default_output_device().and_then(|d| d.id().ok()).map(|id| id.to_string())
    }

    pub(super) fn run(audio: &Arc<AudioShared>, cancel: &AtomicBool, ready: mpsc::SyncSender<Result<(u32, u16), String>>) {
        let host = cpal::default_host();
        let mut current = match open(&host, audio) {
            Ok(o) => {
                if ready.send(Ok(o.format)).is_err() {
                    return; // the player gave up waiting
                }
                Some(o)
            }
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let mut null = NullSink::new(NULL_FORMAT);
        let mut last_check = Instant::now();
        while !audio.stop.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
            let playing = audio.playing.load(Ordering::Acquire);
            let due = last_check.elapsed() >= FOLLOW_EVERY;
            let rebuild = match &current {
                Some(o) => o.broken.load(Ordering::Acquire) || (playing && due && default_id(&host) != o.id),
                None => due, // no device: look for one now and then
            };
            if due {
                last_check = Instant::now();
            }
            if rebuild {
                current = None; // stops the old stream before opening a new one
                match open(&host, audio) {
                    Ok(o) => {
                        audio.set_format(o.format.0, o.format.1);
                        current = Some(o);
                    }
                    Err(e) => {
                        log::debug!("audio output unavailable: {e}");
                        audio.set_format(NULL_FORMAT.0, NULL_FORMAT.1);
                    }
                }
            }
            match &mut current {
                Some(o) => {
                    if playing != o.playing {
                        let r = if playing { o.stream.play() } else { o.stream.pause() };
                        if let Err(e) = r {
                            log::debug!("audio output: {e}");
                        }
                        o.playing = playing;
                    }
                    thread::park_timeout(Duration::from_millis(100));
                }
                None => {
                    // Keep time silently until a device shows up.
                    null.tick(audio);
                    thread::park_timeout(Duration::from_millis(10));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpectrumAnalyzer;

    #[test]
    fn the_null_sink_plays_in_real_time() {
        let audio = Arc::new(AudioShared::new(Arc::new(SpectrumAnalyzer::new()), 1.0, false));
        let watch = ShutdownWatch::new();
        let (rate, ch) = start(AudioOutput::Null, &audio, &watch);
        audio.push(0.0, &vec![0.0; rate as usize * ch as usize]); // 1 s
        audio.playing.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(300));
        let clock = audio.clock();
        assert!((0.2..0.5).contains(&clock), "{clock}");
        audio.stop.store(true, Ordering::Release);
        audio.kick_output();
        assert!(watch.wait(Duration::from_secs(1)));
    }
}
