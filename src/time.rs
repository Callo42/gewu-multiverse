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
}
