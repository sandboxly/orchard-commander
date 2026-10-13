//! Time readouts.

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
}
