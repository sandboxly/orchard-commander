//! Everything that touches FFmpeg.

pub(crate) mod io;
pub(crate) mod probe;
pub(crate) mod samples;
pub(crate) mod waveform;

pub use probe::{PROBE_BUDGET, probe, probe_stream};

use std::sync::Once;

pub(crate) use ffmpeg_next as ff;

/// Initialise FFmpeg once and keep it quiet (garbage files would otherwise
/// print decoder complaints on stderr).
pub(crate) fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(e) = ff::init() {
            log::warn!("FFmpeg init: {e}");
        }
        ff::util::log::set_level(ff::util::log::Level::Fatal);
    });
}

/// FFmpeg's text for an error, in plain words for the common ones.
pub(crate) fn describe(e: &ff::Error) -> String {
    match e {
        ff::Error::StreamNotFound => "no audio or video in this file".to_string(),
        ff::Error::InvalidData => "the file is damaged or in a format that isn't supported".to_string(),
        ff::Error::DecoderNotFound => "its audio and video format isn't supported".to_string(),
        e => e.to_string(),
    }
}

/// Seconds of a timestamp in `tb` units.
pub(crate) fn seconds(ts: i64, tb: ff::Rational) -> f64 {
    if tb.denominator() == 0 { 0.0 } else { ts as f64 * tb.numerator() as f64 / tb.denominator() as f64 }
}
