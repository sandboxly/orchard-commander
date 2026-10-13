//! Time readouts.

use std::time::Duration;

/// Seconds as a Duration, never panicking: negative and NaN read as zero,
/// absurdly large values (hostile timestamps) as [`Duration::MAX`].
#[cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]
pub(crate) fn secs(s: f64) -> Duration {
    if s.is_nan() || s <= 0.0 { Duration::ZERO } else { Duration::try_from_secs_f64(s).unwrap_or(Duration::MAX) }
}

/// "01:23", or "1:02:03" from an hour on (negative times read as zero).
pub fn format_time(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (h, rem) = (s / 3600, s % 3600);
    let (m, s) = (rem / 60, rem % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_the_player() {
        assert_eq!(format_time(0), "00:00");
        assert_eq!(format_time(-5), "00:00");
        assert_eq!(format_time(83_000), "01:23");
        assert_eq!(format_time(83_999), "01:23");
        assert_eq!(format_time(3_723_000), "1:02:03");
    }

    #[test]
    fn seconds_never_panic() {
        assert_eq!(secs(1.5), Duration::from_millis(1500));
        assert_eq!(secs(-3.0), Duration::ZERO);
        assert_eq!(secs(f64::NAN), Duration::ZERO);
        assert_eq!(secs(1e30), Duration::MAX);
        assert_eq!(secs(f64::INFINITY), Duration::MAX);
    }
}
