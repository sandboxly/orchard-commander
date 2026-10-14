//! The decoder thread: demuxes, decodes, resamples audio to the output's
//! format, scales video for the UI, handles seeks, notices the end.
//!
//! Audio is the master clock. The decoder stays [`AUDIO_AHEAD`] seconds of
//! audio and a few video frames ahead of playback and waits otherwise.
//! Video frames that are already late are skipped before scaling (a few in a
//! row at most, so a slow machine still shows motion).

use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use super::video::{Scaler, fit_size};
use super::{Command, Event, MediaStatus, PlaybackState, PlayerOptions, Shared, output};
use crate::MediaSource;
use crate::ffmpeg::ff::ffi;
use crate::ffmpeg::ff::util::error::EAGAIN;
use crate::ffmpeg::io::Input;
use crate::ffmpeg::{describe, ff, io, probe, seconds};
use crate::threads::ShutdownWatch;

/// Seconds of decoded audio kept ahead of playback.
const AUDIO_AHEAD: f64 = 1.5;
/// Position events while playing.
const POSITION_TICK: Duration = Duration::from_millis(100);
/// Audio gaps longer than this are filled with silence (and the clock keeps running).
const MAX_GAP_FILL: f64 = 10.0;

pub(crate) fn run(source: MediaSource, shared: Arc<Shared>, commands: Receiver<Command>, opts: PlayerOptions, watch: ShutdownWatch) {
    match io::open_source(source, shared.abort.clone()) {
        Ok(input) if !shared.aborted() => match Engine::new(input, &shared, &opts, &watch) {
            Ok(mut engine) => engine.run(&commands),
            Err(message) => shared.fail(message),
        },
        Ok(_) => {}
        Err(e) => {
            if !shared.aborted() {
                shared.fail(e.to_string());
            }
        }
    }
    // Done (dropped, or failed): let the output thread go.
    shared.audio.stop.store(true, std::sync::atomic::Ordering::Release);
    shared.audio.kick_output();
}

struct Resampler {
    key: (ff::format::Sample, u32, u16),
    ctx: ff::software::resampling::Context,
}

struct AudioTrack {
    index: usize,
    decoder: ff::decoder::Audio,
    tb: ff::Rational,
    resampler: Option<Resampler>,
    /// The output's (rate, channels) and its generation.
    out: (u32, u16),
    generation: u64,
    frame: ff::frame::Audio,
    buf: Vec<f32>,
    /// Where the next chunk should start; gaps before a chunk get silence.
    expected: Option<f64>,
    /// After a seek: drop audio before this.
    floor: Option<f64>,
}

impl AudioTrack {
    fn open(input: &Input, index: usize) -> Result<Self, ff::Error> {
        let stream = input.stream(index).ok_or(ff::Error::StreamNotFound)?;
        let tb = stream.time_base();
        let mut context = ff::codec::context::Context::from_parameters(stream.parameters())?;
        // SAFETY: setting the packet time base on an unopened codec context.
        unsafe { (*context.as_mut_ptr()).pkt_timebase = tb.into() };
        let decoder = context.decoder().audio()?;
        Ok(AudioTrack {
            index,
            decoder,
            tb,
            resampler: None,
            out: (48000, 2),
            generation: 0,
            frame: ff::frame::Audio::empty(),
            buf: Vec::new(),
            expected: Some(0.0),
            floor: None,
        })
    }

    fn resampler(&mut self) -> Result<&mut Resampler, ff::Error> {
        let f = &self.frame;
        let key = (f.format(), f.rate(), f.channels());
        if self.resampler.as_ref().is_none_or(|r| r.key != key) {
            // SAFETY: reading the frame's channel layout order.
            let native = unsafe { (*f.as_ptr()).ch_layout.order == ffi::AVChannelOrder::AV_CHANNEL_ORDER_NATIVE };
            let layout = if native { f.channel_layout() } else { ff::ChannelLayout::default(f.channels() as i32) };
            let ctx = ff::software::resampling::Context::get(
                f.format(),
                layout,
                f.rate(),
                ff::format::Sample::F32(ff::format::sample::Type::Packed),
                ff::ChannelLayout::default(self.out.1 as i32),
                self.out.0,
            )?;
            self.resampler = Some(Resampler { key, ctx });
        }
        self.resampler.as_mut().ok_or(ff::Error::Bug)
    }

    /// Resample the decoded frame into `buf` (interleaved f32, output format).
    fn convert(&mut self) -> Result<(), ff::Error> {
        let channels = self.out.1 as usize;
        let in_count = self.frame.samples() as c_int;
        // SAFETY: the frame's extended_data holds its planes; `buf` is sized
        // for the most samples swr_convert may write.
        unsafe {
            let in_planes = (*self.frame.as_ptr()).extended_data as *const *const u8;
            let r = self.resampler()?;
            let ctx = r.ctx.as_mut_ptr();
            let max_out = ffi::swr_get_out_samples(ctx, in_count);
            if max_out < 0 {
                return Err(ff::Error::from(max_out));
            }
            self.buf.resize(max_out as usize * channels, 0.0);
            let out_planes = [self.buf.as_mut_ptr() as *mut u8];
            let n = ffi::swr_convert(ctx, out_planes.as_ptr(), max_out, in_planes, in_count);
            if n < 0 {
                return Err(ff::Error::from(n));
            }
            self.buf.truncate(n as usize * channels);
        }
        Ok(())
    }

    /// Hand the decoded frame to the output queue.
    fn process(&mut self, shared: &Shared, start: f64) {
        if let Err(e) = self.convert() {
            log::debug!("audio conversion: {e}");
            return;
        }
        let channels = self.out.1 as usize;
        let rate = self.out.0 as f64;
        let frames = self.buf.len() / channels;
        if frames == 0 {
            return;
        }
        let mut pts = self.frame.timestamp().map(|t| seconds(t, self.tb) - start).or(self.expected).unwrap_or(0.0);
        let dur = frames as f64 / rate;
        if let Some(floor) = self.floor {
            if pts + dur <= floor {
                self.expected = Some(pts + dur);
                return;
            }
            if pts < floor {
                let skip = (((floor - pts) * rate).round() as usize).min(frames);
                self.buf.drain(..skip * channels);
                pts = floor;
            }
            self.floor = None;
        }
        if let Some(expected) = self.expected {
            let gap = pts - expected;
            if gap > 0.03 && gap < MAX_GAP_FILL {
                // Late-starting audio, or a gap: keep the clock going through it.
                let silence = vec![0.0; (gap * rate) as usize * channels];
                shared.audio.push(expected, &silence);
            }
        }
        shared.audio.push(pts, &self.buf);
        self.expected = Some(pts + (self.buf.len() / channels) as f64 / rate);
    }
}

struct VideoTrack {
    index: usize,
    decoder: ff::decoder::Video,
    tb: ff::Rational,
    sar: Option<f64>,
    frame_dur: f64,
    frame: ff::frame::Video,
    scaler: Option<Scaler>,
    target: Option<(u32, u32)>,
    next_pts: Option<f64>,
    floor: Option<f64>,
    /// Late frames skipped in a row.
    late: u32,
}

fn ratio(r: ff::Rational) -> Option<f64> {
    (r.numerator() > 0 && r.denominator() > 0).then(|| f64::from(r))
}

impl VideoTrack {
    fn open(input: &Input, index: usize, target: Option<(u32, u32)>) -> Result<Self, ff::Error> {
        let stream = input.stream(index).ok_or(ff::Error::StreamNotFound)?;
        let tb = stream.time_base();
        let fps = ratio(stream.avg_frame_rate()).or_else(|| ratio(stream.rate())).filter(|f| (1.0..=1000.0).contains(f));
        // SAFETY: codecpar of an open input's stream.
        let sar = ratio(unsafe { (*(*stream.as_ptr()).codecpar).sample_aspect_ratio }.into());
        let mut context = ff::codec::context::Context::from_parameters(stream.parameters())?;
        // SAFETY: as for audio.
        unsafe { (*context.as_mut_ptr()).pkt_timebase = tb.into() };
        context.set_threading(ff::codec::threading::Config::kind(ff::codec::threading::Type::Frame));
        let decoder = context.decoder().video()?;
        Ok(VideoTrack {
            index,
            decoder,
            tb,
            sar,
            frame_dur: fps.map_or(0.04, |f| 1.0 / f),
            frame: ff::frame::Video::empty(),
            scaler: None,
            target,
            next_pts: None,
            floor: None,
            late: 0,
        })
    }

    fn process(&mut self, shared: &Shared, start: f64) {
        let pts = self.frame.timestamp().map(|t| seconds(t, self.tb) - start).or(self.next_pts).unwrap_or(0.0);
        self.next_pts = Some(pts + self.frame_dur);
        if let Some(floor) = self.floor {
            if pts + self.frame_dur <= floor + 1e-6 {
                return; // before the seek target
            }
            self.floor = None;
        }
        let (clock, playing) = {
            let st = shared.status();
            (shared.position_in(&st), st.state == PlaybackState::Playing)
        };
        let waiting = {
            let q = shared.frames();
            q.preroll && q.len() == 0
        };
        if playing && !waiting && pts + self.frame_dur < clock - 0.05 && self.late < 8 {
            self.late += 1; // too late to show: don't even scale it
            return;
        }
        self.late = 0;
        let sar = ratio(self.frame.aspect_ratio()).or(self.sar);
        let size = fit_size(self.frame.width(), self.frame.height(), sar, self.target);
        let mut buf = shared.frames().buffer(size.0 as usize * size.1 as usize * 4);
        if let Err(e) = Scaler::convert(&mut self.scaler, &self.frame, size, &mut buf) {
            log::debug!("video scaling: {e}");
            return;
        }
        if shared.frames().push(size.0, size.1, buf, pts) {
            shared.wake(); // the first frame after opening or seeking: show it now
        }
    }
}

struct Engine<'s> {
    shared: &'s Shared,
    input: Input<'static>,
    /// The container's start time (positions count from it).
    start: f64,
    seekable: bool,
    audio: Option<AudioTrack>,
    video: Option<VideoTrack>,
    eof: bool,
    decoded_any: bool,
    dead: bool,
    pending: VecDeque<Command>,
    last_tick: Instant,
    last_reported: f64,
    /// Since when audio has been empty while video piles up.
    starved_since: Option<Instant>,
}

impl<'s> Engine<'s> {
    fn new(mut input: Input<'static>, shared: &'s Shared, opts: &PlayerOptions, watch: &ShutdownWatch) -> Result<Self, String> {
        let seekable = input.seekable;
        let mut info = probe::info_from(&input, seekable);
        let (ai, vi) = probe::pick_streams(&input);
        if ai.is_none() && vi.is_none() {
            return Err("no audio or video in this file".into());
        }
        let audio = ai.and_then(|i| AudioTrack::open(&input, i).map_err(|e| log::info!("audio: {}", describe(&e))).ok());
        let video = vi.and_then(|i| VideoTrack::open(&input, i, opts.video_size).map_err(|e| log::info!("video: {}", describe(&e))).ok());
        if audio.is_none() && video.is_none() {
            return Err("its audio and video format isn't supported".into());
        }
        if audio.is_none() {
            info.audio = None;
        }
        if video.is_none() {
            info.video = None;
        }
        // SAFETY: plain fields of the open format context; streams we don't
        // play are skipped by the demuxer.
        let start = unsafe {
            let ctx = input.as_mut_ptr();
            let used = [audio.as_ref().map(|a| a.index), video.as_ref().map(|v| v.index)];
            for i in 0..(*ctx).nb_streams as usize {
                if !used.contains(&Some(i)) {
                    (*(*(*ctx).streams.add(i))).discard = ffi::AVDiscard::AVDISCARD_ALL;
                }
            }
            let t = (*ctx).start_time;
            if t == ffi::AV_NOPTS_VALUE { 0.0 } else { t as f64 / ffi::AV_TIME_BASE as f64 }
        };
        let mut engine = Engine {
            shared,
            input,
            start,
            seekable,
            audio,
            video,
            eof: false,
            decoded_any: false,
            dead: false,
            pending: VecDeque::new(),
            last_tick: Instant::now(),
            last_reported: -1.0,
            starved_since: None,
        };
        if let Some(a) = &mut engine.audio {
            output::start(opts.output, &shared.audio, watch);
            // Generation first: a device switch after these reads bumps it again.
            a.generation = shared.audio.generation();
            a.out = shared.audio.format();
        }
        let mut st = shared.status();
        st.duration = info.duration.map(|d| d.as_secs_f64());
        st.info = Some(info.clone());
        st.media = MediaStatus::Ready;
        st.audio_clock = engine.audio.is_some();
        let playing = st.state == PlaybackState::Playing;
        shared.set_running(&mut st, playing);
        drop(st);
        shared.emit(Event::Loaded(Box::new(info)));
        Ok(engine)
    }

    fn take_commands(&mut self) {
        while let Some(c) = self.pending.pop_front() {
            self.handle(c);
        }
    }

    fn handle(&mut self, c: Command) {
        match c {
            Command::Wake => {}
            Command::Seek { serial, pos } => self.seek(Some(serial), pos),
            Command::VideoSize(size) => {
                let Some(v) = &mut self.video else { return };
                if v.target == size {
                    return;
                }
                v.target = size;
                // Paused: decode the frame on screen again at the new size.
                let (paused, pos, target) = {
                    let st = self.shared.status();
                    (st.state != PlaybackState::Playing && st.media == MediaStatus::Ready, self.shared.position_in(&st), st.seek_target)
                };
                if paused && target.is_none() && self.seekable {
                    self.seek(None, pos);
                }
            }
        }
    }

    fn receive(&mut self, commands: &Receiver<Command>, wait: Option<Duration>) -> bool {
        if let Some(d) = wait {
            match commands.recv_timeout(d) {
                Ok(c) => self.pending.push_back(c),
                Err(RecvTimeoutError::Disconnected) => return false,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        loop {
            match commands.try_recv() {
                Ok(c) => self.pending.push_back(c),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        // Only the last of several queued seeks matters (scrubbing).
        if self.pending.iter().filter(|c| matches!(c, Command::Seek { .. })).count() > 1 {
            let last = self.pending.iter().rposition(|c| matches!(c, Command::Seek { .. }));
            let mut i = 0;
            self.pending.retain(|c| {
                let keep = !matches!(c, Command::Seek { .. }) || Some(i) == last;
                i += 1;
                keep
            });
        }
        true
    }

    fn run(&mut self, commands: &Receiver<Command>) {
        let mut wait = None;
        loop {
            if !self.receive(commands, wait.take()) || self.shared.aborted() {
                return;
            }
            self.take_commands();
            if self.shared.aborted() || self.dead {
                return;
            }
            self.follow_output_format();

            let (playing, ended, pos) = {
                let st = self.shared.status();
                (st.state == PlaybackState::Playing, st.media == MediaStatus::Ended, self.shared.position_in(&st))
            };
            self.shared.frames().advance(pos);
            if playing && self.last_tick.elapsed() >= POSITION_TICK {
                self.last_tick = Instant::now();
                if (pos - self.last_reported).abs() > 0.0005 {
                    self.last_reported = pos;
                    self.shared.emit(Event::Position(crate::time::secs(pos)));
                }
            }
            let idle = Some(Duration::from_millis(if playing { 10 } else { 50 }));
            if self.eof {
                if !ended {
                    self.check_end(pos, playing);
                }
                wait = idle;
                continue;
            }
            if self.buffers_full(playing) {
                wait = idle;
                continue;
            }
            self.read_packet();
        }
    }

    /// The output moved to a device with another format: resample anew and
    /// decode again from where playback is.
    fn follow_output_format(&mut self) {
        let Some(a) = &mut self.audio else { return };
        let generation = self.shared.audio.generation();
        if generation == a.generation {
            return;
        }
        a.generation = generation;
        a.out = self.shared.audio.format();
        a.resampler = None;
        let (pos, ended) = {
            let st = self.shared.status();
            (self.shared.position_in(&st), st.media == MediaStatus::Ended)
        };
        if self.seekable && !ended {
            self.seek(None, pos);
        }
    }

    fn buffers_full(&mut self, playing: bool) -> bool {
        let audio_buf = if self.audio.is_some() { self.shared.audio.buffered_secs() } else { f64::INFINITY };
        let (queued, waiting, ahead) = {
            let q = self.shared.frames();
            (q.len(), q.preroll && q.len() == 0, q.ahead())
        };
        let has_video = self.video.is_some();
        // Audio is plenty, unless the first frame (after opening or seeking) is still to come.
        if self.audio.is_some() && audio_buf >= AUDIO_AHEAD && !(has_video && waiting && audio_buf < 5.0) {
            return true;
        }
        if has_video && queued >= ahead {
            if self.audio.is_none() || audio_buf >= 0.3 {
                self.starved_since = None;
                return true;
            }
            // Video piles up but no audio comes: the audio track ended early
            // (or is far behind in the file). Let the wall clock take over.
            if playing && audio_buf == 0.0 {
                let since = *self.starved_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_millis(300) {
                    let mut st = self.shared.status();
                    if st.audio_clock {
                        let p = self.shared.audio.clock();
                        st.wall.set(p);
                        st.audio_clock = false;
                    }
                }
                return true;
            }
        }
        false
    }

    fn read_packet(&mut self) {
        let mut packet = ff::Packet::empty(); // fresh each time: av_read_frame doesn't unref
        match packet.read(&mut self.input) {
            Ok(()) => {
                let stream = packet.stream();
                if let Some(a) = self.audio.as_mut().filter(|a| a.index == stream) {
                    if a.decoder.send_packet(&packet).is_ok() {
                        self.drain_audio();
                    }
                } else if let Some(v) = self.video.as_mut().filter(|v| v.index == stream)
                    && v.decoder.send_packet(&packet).is_ok()
                {
                    self.drain_video();
                }
            }
            Err(ff::Error::Eof) => self.finish_decoding(),
            Err(ff::Error::Other { errno }) if errno == EAGAIN => std::thread::sleep(Duration::from_millis(5)),
            Err(ff::Error::Exit) => self.dead = true,
            Err(e) if !self.decoded_any => {
                self.shared.fail(describe(&e));
                self.dead = true;
            }
            Err(e) => {
                log::info!("playback stops at a read error: {}", describe(&e));
                self.finish_decoding();
            }
        }
    }

    fn finish_decoding(&mut self) {
        self.eof = true;
        if let Some(a) = &mut self.audio
            && a.decoder.send_eof().is_ok()
        {
            self.drain_audio();
        }
        if let Some(v) = &mut self.video
            && v.decoder.send_eof().is_ok()
        {
            self.drain_video();
        }
        if !self.decoded_any {
            self.shared.fail("nothing in this file can be played".into());
            self.dead = true;
        }
    }

    fn drain_audio(&mut self) {
        let Some(a) = &mut self.audio else { return };
        while a.decoder.receive_frame(&mut a.frame).is_ok() {
            self.decoded_any = true;
            a.process(self.shared, self.start);
        }
    }

    fn drain_video(&mut self) {
        let Some(v) = &mut self.video else { return };
        while v.decoder.receive_frame(&mut v.frame).is_ok() {
            self.decoded_any = true;
            v.process(self.shared, self.start);
        }
    }

    /// Everything decoded: finish (or loop) once it has been played.
    fn check_end(&mut self, pos: f64, playing: bool) {
        let audio_done = self.audio.is_none() || self.shared.audio.is_empty();
        if self.audio.is_some() && audio_done {
            // Video may go on after the audio: the wall clock takes over.
            let mut st = self.shared.status();
            if st.audio_clock {
                let p = self.shared.audio.clock();
                st.wall.set(p);
                st.audio_clock = false;
            }
        }
        let video_done = match &self.video {
            None => true,
            Some(v) => {
                let q = self.shared.frames();
                q.len() == 0 && q.current.as_ref().is_none_or(|f| pos + 1e-3 >= f.pts.as_secs_f64() + v.frame_dur)
            }
        };
        if !(audio_done && video_done && playing) {
            return;
        }
        let mut st = self.shared.status();
        if st.looping && self.seekable {
            drop(st);
            self.seek(None, 0.0);
            return;
        }
        st.media = MediaStatus::Ended;
        self.shared.set_state(&mut st, PlaybackState::Stopped);
        let end = self.shared.position_in(&st);
        drop(st); // the events below wake the UI
        self.shared.emit(Event::Position(crate::time::secs(end)));
        self.shared.emit(Event::Finished);
    }

    /// Go to `pos`. `serial` is the player's seek request (None: our own).
    fn seek(&mut self, serial: Option<u64>, pos: f64) {
        if self.seekable {
            let ts = ((pos + self.start) * ffi::AV_TIME_BASE as f64) as i64;
            if let Err(e) = self.input.seek(ts, ..ts).or_else(|_| self.input.seek(ts, ..)) {
                log::debug!("seek to {pos:.3}: {}", describe(&e));
            }
            if let Some(a) = &mut self.audio {
                a.decoder.flush();
                a.resampler = None;
                a.floor = Some(pos);
                a.expected = Some(pos);
            }
            if let Some(v) = &mut self.video {
                v.decoder.flush();
                v.floor = Some(pos);
                v.next_pts = None;
                v.late = 0;
            }
            self.eof = false;
            self.starved_since = None;
            self.shared.audio.flush(pos);
            self.shared.frames().clear();
        }
        let mut st = self.shared.status();
        if self.seekable {
            st.wall.set(pos);
            st.audio_clock = self.audio.is_some();
        }
        if serial.is_none_or(|s| s == st.seek_serial) {
            st.seek_target = None;
        }
        let now = self.shared.position_in(&st);
        drop(st);
        self.last_reported = now;
        self.shared.emit(Event::Position(crate::time::secs(now)));
    }
}
