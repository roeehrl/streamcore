//! RFC 3339 date-times for `Profile.updatedAt` (last writer wins), without a date
//! library: parsing to an instant and formatting the current time.

use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since 1970-01-01T00:00:00Z.
pub type Instant = i128;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

fn digits(bytes: &[u8], start: usize, count: usize) -> Option<i64> {
    let slice = bytes.get(start..start + count)?;
    slice.iter().try_fold(0i64, |acc, b| {
        b.is_ascii_digit().then(|| acc * 10 + i64::from(b - b'0'))
    })
}

/// Parses an RFC 3339 date-time ("2026-10-10T10:00:00Z",
/// "2026-10-10T12:00:00.123+02:00") to an instant. `None` when it is not one.
pub fn parse_rfc3339(text: &str) -> Option<Instant> {
    let b = text.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let year = digits(b, 0, 4)?;
    let month = digits(b, 5, 2)?;
    let day = digits(b, 8, 2)?;
    let hour = digits(b, 11, 2)?;
    let minute = digits(b, 14, 2)?;
    let second = digits(b, 17, 2)?;
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    let mut nanos: i128 = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            if i - start < 9 {
                nanos = nanos * 10 + i128::from(b[i] - b'0');
            }
            i += 1;
        }
        let count = i - start;
        if count == 0 {
            return None;
        }
        for _ in count..9 {
            nanos *= 10;
        }
    }
    let offset_seconds: i64 = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if i + 6 == b.len() && b[i + 3] == b':' => {
            let oh = digits(b, i + 1, 2)?;
            let om = digits(b, i + 4, 2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let magnitude = oh * 3600 + om * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    Some(i128::from(seconds) * NANOS_PER_SECOND + nanos)
}

/// Formats an instant as RFC 3339 in UTC with milliseconds.
pub fn format_rfc3339(instant: Instant) -> String {
    let seconds = instant.div_euclid(NANOS_PER_SECOND);
    let millis = instant.rem_euclid(NANOS_PER_SECOND) / 1_000_000;
    let days = i64::try_from(seconds.div_euclid(86_400)).unwrap_or(0);
    let rem = i64::try_from(seconds.rem_euclid(86_400)).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Whether `text` is an RFC 3339 date-time.
#[uniffi::export]
pub fn is_rfc3339(text: String) -> bool {
    parse_rfc3339(&text).is_some()
}

/// Compares two RFC 3339 date-times: -1, 0 or 1. An unreadable value sorts
/// before every readable one (it never wins last-writer-wins).
#[uniffi::export]
pub fn compare_timestamps(a: String, b: String) -> i32 {
    match (parse_rfc3339(&a), parse_rfc3339(&b)) {
        (Some(a), Some(b)) => a.cmp(&b) as i32,
        (Some(_), None) => 1,
        (None, Some(_)) => -1,
        (None, None) => 0,
    }
}

/// The current time as an RFC 3339 `updatedAt` (UTC, milliseconds).
#[uniffi::export]
pub fn profile_timestamp_now() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    format_rfc3339(nanos)
}

/// Stamp an edit later than the version it replaces, even when the clock has not advanced.
/// `now` is optional so tests can use an exact clock; app callers normally omit it.
#[uniffi::export]
pub fn profile_timestamp_after(
    previous: Option<String>,
    now: Option<String>,
) -> crate::Result<String> {
    let now = now.unwrap_or_else(profile_timestamp_now);
    let current = parse_rfc3339(&now)
        .ok_or_else(|| crate::StreamInputError::message("now must be an RFC 3339 timestamp"))?;
    let millis = 1_000_000;
    let previous = previous.as_deref().and_then(parse_rfc3339);
    let instant = previous.map_or(current.div_euclid(millis) * millis, |prior| {
        (current.div_euclid(millis) * millis).max((prior.div_euclid(millis) + 1) * millis)
    });
    let text = format_rfc3339(instant);
    if parse_rfc3339(&text).is_none() {
        return Err(crate::StreamInputError::message(
            "profile timestamp exceeds the RFC 3339 year range",
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_advance_past_ties_clock_skew_and_sub_millisecond_values() {
        for (prior, now, expected) in [
            (
                "2026-10-11T10:00:00.123Z",
                "2026-10-11T10:00:00.123Z",
                "2026-10-11T10:00:00.124Z",
            ),
            (
                "2026-10-11T12:00:00.123+02:00",
                "2026-10-11T09:00:00Z",
                "2026-10-11T10:00:00.124Z",
            ),
            (
                "2026-10-11T10:00:00.123999999Z",
                "2026-10-11T10:00:00.123Z",
                "2026-10-11T10:00:00.124Z",
            ),
            (
                "1969-12-31T23:59:59.999Z",
                "1969-12-31T23:59:59.999Z",
                "1970-01-01T00:00:00.000Z",
            ),
        ] {
            assert_eq!(
                profile_timestamp_after(Some(prior.into()), Some(now.into())).unwrap(),
                expected
            );
        }
        assert!(profile_timestamp_after(None, Some("invalid".into())).is_err());
        assert!(
            profile_timestamp_after(
                Some("9999-12-31T23:59:59.999Z".into()),
                Some("2000-01-01T00:00:00Z".into())
            )
            .is_err()
        );
    }

    #[test]
    fn parses_offsets_and_fractions() {
        let utc = parse_rfc3339("2026-10-10T10:00:00Z").unwrap();
        assert_eq!(parse_rfc3339("2026-10-10T12:00:00+02:00"), Some(utc));
        assert_eq!(parse_rfc3339("2026-10-10T05:30:00-04:30"), Some(utc));
        assert_eq!(parse_rfc3339("2026-10-10t10:00:00z"), Some(utc));
        assert_eq!(
            parse_rfc3339("2026-10-10T10:00:00.123Z"),
            Some(utc + 123_000_000)
        );
        assert_eq!(
            parse_rfc3339("2026-10-10T10:00:00.1234567891Z"),
            Some(utc + 123_456_789)
        );
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2000-01-01T00:00:00Z"),
            Some(946_684_800 * NANOS_PER_SECOND)
        );
    }

    #[test]
    fn rejects_non_dates() {
        for bad in [
            "yesterday",
            "2026-10-10",
            "2026-10-10T10:00:00",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-10-10T24:00:00Z",
            "2026-10-10T10:00:00.Z",
            "2026-10-10T10:00:00+0200",
            "2026-10-10T10:00:00Zjunk",
            "２026-10-10T10:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
        assert!(parse_rfc3339("2024-02-29T00:00:00Z").is_some());
    }

    #[test]
    fn formats_and_compares() {
        let t = parse_rfc3339("2026-10-10T10:00:00.250Z").unwrap();
        assert_eq!(format_rfc3339(t), "2026-10-10T10:00:00.250Z");
        assert_eq!(format_rfc3339(-1_000_000), "1969-12-31T23:59:59.999Z");
        assert!(is_rfc3339(profile_timestamp_now()));
        assert_eq!(
            compare_timestamps(
                "2026-10-10T10:00:01Z".into(),
                "2026-10-10T12:00:00+02:00".into()
            ),
            1
        );
        assert_eq!(
            compare_timestamps("bad".into(), "2000-01-01T00:00:00Z".into()),
            -1
        );
    }
}
