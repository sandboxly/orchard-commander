//! Dates as servers send them: HTTP dates (RFC 1123/850/asctime, as
//! `email.utils.parsedate_to_datetime` reads them), ISO 8601 (S3 listings)
//! and the compact SigV4 stamp. No calendar crate needed: UTC only.

use std::time::{SystemTime, UNIX_EPOCH};

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]; // 1970-01-01 was a Thursday

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Broken-down UTC time of a Unix timestamp (whole seconds, floored).
fn utc(t: f64) -> (i64, u32, u32, u32, u32, u32, i64) {
    let secs = t.floor() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32, days)
}

fn timestamp(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<f64> {
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 24 || mi > 59 || s > 61 {
        return None;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(s)) as f64)
}

/// `email.utils.formatdate(t, usegmt=True)`: "Sun, 06 Nov 1994 08:49:37 GMT".
pub fn format_http_date(t: f64) -> String {
    let (y, mo, d, h, mi, s, days) = utc(t);
    let month = MONTHS[(mo - 1) as usize];
    let month = format!("{}{}", month[..1].to_uppercase(), &month[1..]);
    format!("{}, {d:02} {month} {y:04} {h:02}:{mi:02}:{s:02} GMT", DAYS[days.rem_euclid(7) as usize])
}

/// The SigV4 stamp "20130524T000000Z".
pub fn amz_stamp(when: SystemTime) -> String {
    let t = when.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let (y, mo, d, h, mi, s, _) = utc(t);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// An HTTP date (RFC 1123, RFC 850, asctime, also with numeric zones) as a
/// Unix timestamp; None if it can't be read.
pub fn parse_http_date(text: &str) -> Option<f64> {
    let tokens: Vec<&str> = text.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()).collect();
    let (mut day, mut month, mut year, mut clock, mut offset) = (None, None, None, None, 0i64);
    for token in tokens {
        if token.contains(':') {
            clock = Some(token);
        } else if let Some(m) = month_of(token) {
            month = Some(m);
        } else if token.contains('-') && !token.starts_with(['+', '-']) {
            // RFC 850: 06-Nov-94
            let mut parts = token.split('-');
            day = parts.next().and_then(|p| p.parse::<u32>().ok());
            month = parts.next().and_then(month_of);
            year = parts.next().and_then(|p| p.parse::<i64>().ok());
        } else if let Some(zone) = token.strip_prefix(['+', '-']) {
            let n: i64 = zone.parse().ok()?;
            let minutes = n / 100 * 60 + n % 100;
            offset = if token.starts_with('-') { -minutes * 60 } else { minutes * 60 };
        } else if let Ok(n) = token.parse::<i64>() {
            if day.is_none() && token.len() <= 2 {
                day = Some(n as u32);
            } else {
                year = Some(n);
            }
        }
        // Weekday names, "GMT"/"UTC"/"Z": nothing to do.
    }
    let mut year = year?;
    if year < 100 {
        year += if year < 69 { 2000 } else { 1900 };
    }
    let mut hms = clock?.split(':').map(|p| p.parse::<u32>().ok());
    let (h, mi) = (hms.next()??, hms.next()??);
    let s = hms.next().flatten().unwrap_or(0);
    Some(timestamp(year, month?, day?, h, mi, s)? - offset as f64)
}

fn month_of(token: &str) -> Option<u32> {
    let lower = token.get(..3)?.to_ascii_lowercase();
    if token.len() > 3 && !token.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    MONTHS.iter().position(|m| *m == lower).map(|i| i as u32 + 1)
}

/// ISO 8601 as S3 sends it ("2025-01-02T03:04:05.000Z", or with an offset);
/// without a zone it's taken as UTC. None if it can't be read.
pub fn parse_iso8601(text: &str) -> Option<f64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' ']).unwrap_or((text, ""));
    let mut ymd = date.split('-');
    let (y, mo, d) = (ymd.next()?.parse::<i64>().ok()?, ymd.next()?.parse::<u32>().ok()?, ymd.next()?.parse::<u32>().ok()?);
    let (clock, offset) = match rest.find(['Z', 'z', '+', '-']) {
        Some(i) => {
            let zone = &rest[i..];
            let offset = if zone.eq_ignore_ascii_case("z") {
                0
            } else {
                let sign = if zone.starts_with('-') { -1 } else { 1 };
                let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
                if digits.len() != 4 {
                    return None;
                }
                let n: i64 = digits.parse().ok()?;
                sign * (n / 100 * 3600 + n % 100 * 60)
            };
            (&rest[..i], offset)
        }
        None => (rest, 0),
    };
    let (mut h, mut mi, mut s, mut frac) = (0, 0, 0, 0.0);
    if !clock.is_empty() {
        let mut parts = clock.split(':');
        h = parts.next()?.parse().ok()?;
        mi = parts.next()?.parse().ok()?;
        if let Some(sec) = parts.next() {
            let (whole, fraction) = sec.split_once('.').unwrap_or((sec, ""));
            s = whole.parse().ok()?;
            if !fraction.is_empty() {
                frac = format!("0.{fraction}").parse().ok()?;
            }
        }
    }
    Some(timestamp(y, mo, d, h, mi, s)? + frac - offset as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn http_dates() {
        let t = 784_111_777.0; // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(t));
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), Some(t));
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(t));
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 09:49:37 +0100"), Some(t));
        assert_eq!(parse_http_date("6 Nov 1994 08:49:37 UTC"), Some(t));
        assert_eq!(parse_http_date(""), None);
        assert_eq!(parse_http_date("garbage"), None);
        assert_eq!(format_http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(format_http_date(1_600_000_000.0), "Sun, 13 Sep 2020 12:26:40 GMT");
        assert_eq!(parse_http_date(&format_http_date(1_600_000_000.0)), Some(1_600_000_000.0));
    }

    #[test]
    fn iso_dates() {
        assert_eq!(parse_iso8601("2020-09-13T12:26:40.000Z"), Some(1_600_000_000.0));
        assert_eq!(parse_iso8601("2020-09-13T14:26:40+02:00"), Some(1_600_000_000.0));
        assert_eq!(parse_iso8601("2020-09-13T12:26:40.5Z"), Some(1_600_000_000.5));
        assert_eq!(parse_iso8601("nonsense"), None);
    }

    #[test]
    fn stamps_and_calendar() {
        assert_eq!(amz_stamp(UNIX_EPOCH + Duration::from_secs(1_369_353_600)), "20130524T000000Z");
        for days in [-1000, 0, 59, 60, 365, 11_016, 20_000, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
