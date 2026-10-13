//! Decoding a file's audio for the waveform: mono floats at the file's own
//! rate (no resampling), reduced to block levels as they come.

use std::time::{Duration, Instant};

use super::ff;
use super::ff::util::error::EAGAIN;
use super::{describe, io, probe, samples};
use crate::MediaSource;
use crate::waveform::{BlockAccumulator, WaveShared};

/// Hand levels to the UI at most this often (decoding runs far faster than
/// real time; every frame would flood the UI with wake-ups).
const PUBLISH_EVERY: Duration = Duration::from_millis(50);

pub(crate) fn decode(source: MediaSource, shared: &WaveShared) {
    let result = run(source, shared);
    shared.finish(result);
}

/// Decoded blocks go to the shared levels at once (so a stalled source
/// still shows everything decoded before it); viewers are woken at most
/// every [`PUBLISH_EVERY`].
struct Batch {
    pending: bool,
    since: Instant,
}

impl Batch {
    fn add(&mut self, block: (Vec<f32>, Vec<f32>), shared: &WaveShared) {
        self.pending |= !block.0.is_empty();
        let notify = self.pending && self.since.elapsed() >= PUBLISH_EVERY;
        shared.push(block, notify);
        if notify {
            self.pending = false;
            self.since = Instant::now();
        }
    }

    fn flush(&mut self, shared: &WaveShared) {
        shared.push((Vec::new(), Vec::new()), self.pending);
        self.pending = false;
        self.since = Instant::now();
    }
}

fn run(source: MediaSource, shared: &WaveShared) -> Result<(Vec<f32>, Vec<f32>), String> {
    let mut input = io::open_source(source, shared.abort.clone()).map_err(|e| e.to_string())?;
    shared.progress();
    let index = probe::pick_streams(&input).0.ok_or("no audio in this file")?;
    let parameters = input.stream(index).ok_or("no audio in this file")?.parameters();
    let context = ff::codec::context::Context::from_parameters(parameters).map_err(|e| describe(&e))?;
    let mut decoder = context.decoder().audio().map_err(|e| describe(&e))?;

    let mut acc = BlockAccumulator::default();
    let mut batch = Batch { pending: false, since: Instant::now() };
    let mut frame = ff::frame::Audio::empty();
    let mut mono = Vec::new();
    let mut decoded = false;
    let mut drain = |decoder: &mut ff::decoder::Audio, batch: &mut Batch, decoded: &mut bool| {
        while decoder.receive_frame(&mut frame).is_ok() {
            mono.clear();
            if samples::append_mono(&frame, &mut mono) && frame.rate() > 0 {
                *decoded = true;
                batch.add(acc.push(&mono, frame.rate()), shared);
            }
        }
    };
    loop {
        if shared.aborted() {
            return Err("stopped".into());
        }
        let mut packet = ff::Packet::empty(); // fresh each time: av_read_frame doesn't unref
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ff::Error::Eof) => break,
            Err(ff::Error::Other { errno }) if errno == EAGAIN => continue,
            Err(e) if decoded => {
                log::debug!("waveform: stopping at a read error: {e}");
                break;
            }
            Err(e) => return Err(describe(&e)),
        }
        if packet.stream() != index {
            continue;
        }
        if decoder.send_packet(&packet).is_ok() {
            drain(&mut decoder, &mut batch, &mut decoded);
        }
    }
    if decoder.send_eof().is_ok() {
        drain(&mut decoder, &mut batch, &mut decoded);
    }
    if !decoded {
        return Err("the audio can't be decoded".into());
    }
    batch.flush(shared);
    Ok(acc.finish())
}
