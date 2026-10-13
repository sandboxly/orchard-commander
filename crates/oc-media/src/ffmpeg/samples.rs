//! Decoded audio frames as floats, without FFmpeg's resampler (which the
//! waveform avoids on purpose).

use super::ff;
use super::ff::format::Sample;

/// The planes of an audio frame as byte slices (one for packed formats).
fn planes(frame: &ff::frame::Audio, count: usize, bytes_per_plane: usize) -> Vec<&[u8]> {
    // SAFETY: an audio frame has `count` planes in extended_data, each at
    // least `bytes_per_plane` long (linesize[0] covers the plane), alive as
    // long as the frame.
    unsafe {
        let f = &*frame.as_ptr();
        let linesize = f.linesize[0].max(0) as usize;
        let len = bytes_per_plane.min(linesize);
        (0..count)
            .map(|i| {
                let p = *f.extended_data.add(i);
                if p.is_null() { &[][..] } else { std::slice::from_raw_parts(p, len) }
            })
            .collect()
    }
}

fn mix<const N: usize>(frame: &ff::frame::Audio, planar: bool, out: &mut Vec<f32>, conv: impl Fn([u8; N]) -> f32) {
    let channels = (frame.channels() as usize).max(1);
    let samples = frame.samples();
    let scale = 1.0 / channels as f32;
    let start = out.len();
    out.resize(start + samples, 0.0);
    let dest = &mut out[start..];
    let read = |bytes: &[u8], i: usize| -> f32 {
        bytes.get(i * N..i * N + N).map_or(0.0, |b| conv(b.try_into().unwrap_or([0; N])))
    };
    if planar {
        for plane in planes(frame, channels, samples * N) {
            for (i, d) in dest.iter_mut().enumerate() {
                *d += read(plane, i) * scale;
            }
        }
    } else if let Some(&data) = planes(frame, 1, samples * channels * N).first() {
        for (i, d) in dest.iter_mut().enumerate() {
            *d = (0..channels).map(|c| read(data, i * channels + c)).sum::<f32>() * scale;
        }
    }
}

/// Append the frame's samples, downmixed to mono (the channels' mean), to `out`.
/// Returns false for sample formats it doesn't know.
pub(crate) fn append_mono(frame: &ff::frame::Audio, out: &mut Vec<f32>) -> bool {
    let planar = frame.is_planar();
    match frame.format() {
        Sample::U8(_) => mix::<1>(frame, planar, out, |b| (b[0] as f32 - 128.0) / 128.0),
        Sample::I16(_) => mix::<2>(frame, planar, out, |b| i16::from_ne_bytes(b) as f32 / 32768.0),
        Sample::I32(_) => mix::<4>(frame, planar, out, |b| i32::from_ne_bytes(b) as f32 / 2_147_483_648.0),
        Sample::I64(_) => mix::<8>(frame, planar, out, |b| (i64::from_ne_bytes(b) as f64 / 9.223_372_036_854_776e18) as f32),
        Sample::F32(_) => mix::<4>(frame, planar, out, f32::from_ne_bytes),
        Sample::F64(_) => mix::<8>(frame, planar, out, |b| f64::from_ne_bytes(b) as f32),
        Sample::None => return false,
    }
    true
}
