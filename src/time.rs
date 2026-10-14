//! The one clock the whole program reads, and how a time is written down.
//!
//! Transcript lines and log lines carry a timestamp in RFC 3339 UTC, so a run can be
//! read back long after it happened without a timezone in the way. Nothing here
//! depends on a date crate: the conversion from "seconds since the Unix epoch" to a
//! calendar date is the standard civil-from-days computation, and the tests pin it
//! to known instants.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, or `0` if the clock is before it.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Write milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn rfc3339(unix_ms: u64) -> String {
    let seconds = (unix_ms / 1000) as i64;
    let millis = unix_ms % 1000;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// The calendar date for a count of days since 1970-01-01.
///
/// This is Howard Hinnant's `civil_from_days`, which is exact for every date the
/// proleptic Gregorian calendar defines in the range the epoch arithmetic can reach.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Read the `rfc3339` form back into milliseconds since the Unix epoch.
///
/// [`rfc3339`] is the only writer, so this reads exactly that shape; anything else
/// is `None`, never a guess. It exists so a run in progress can report how long it
/// has been going from the start time its own record carries.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let (date, rest) = text.split_once('T')?;
    let (time, fraction) = match rest.split_once('.') {
        Some((time, fraction)) => (time, fraction.strip_suffix('Z')?),
        None => (rest.strip_suffix('Z')?, "0"),
    };
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() {
        return None;
    }
    // Milliseconds are exactly three digits, as `rfc3339` writes them.
    let millis: i64 = if fraction.is_empty() {
        0
    } else {
        let padded = format!("{fraction:0<3}");
        padded.get(0..3)?.parse().ok()?
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    if seconds < 0 {
        return None;
    }
    Some((seconds as u64) * 1000 + millis as u64)
}

/// The count of days since 1970-01-01 for a calendar date.
///
/// The inverse of [`civil_from_days`], and the same algorithm's other half.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_written_as_1970() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_known_instant_is_written_exactly() {
        // 2021-01-01T00:00:00Z is 1_609_459_200 seconds after the epoch.
        assert_eq!(rfc3339(1_609_459_200_000), "2021-01-01T00:00:00.000Z");
    }

    #[test]
    fn a_leap_day_and_a_millisecond_are_written() {
        // 2024-02-29T12:34:56.789Z
        assert_eq!(rfc3339(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
    }

    #[test]
    fn a_time_in_the_far_future_still_has_a_calendar_date() {
        // 2100-03-01T00:00:00Z — a century year that is not a leap year.
        assert_eq!(rfc3339(4_107_542_400_000), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn the_written_form_reads_back_to_the_same_instant() {
        for instant in [
            0,
            1_609_459_200_000,
            1_709_210_096_789,
            4_107_542_400_000,
        ] {
            let text = rfc3339(instant);
            assert_eq!(parse_rfc3339(&text), Some(instant), "{text}");
        }
    }

    #[test]
    fn anything_that_is_not_the_written_form_is_not_guessed() {
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-10-14T09:50:30.123"), None);
        assert_eq!(parse_rfc3339("2026-13-14T09:50:30.123Z"), None);
    }
}
