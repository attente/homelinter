//! Conservative filename date parsing.
//!
//! Recognized forms are implementation-defined and illustrative, not
//! exhaustive: `IMG_20240315_153045.jpg`, `PXL_20240315_153045123.jpg`,
//! `2024-03-15.jpg`, `20240315.jpg`, and similar. Parsing is conservative:
//! an invalid calendar date, or a date outside a plausible range, is never
//! accepted, and a filename-derived date never overrides a higher-priority
//! source (that ordering is enforced by the suggestion builder).

use std::time::SystemTime;

/// Parse a full calendar date from a tag-style string (ID3 `TDRC`,
/// Vorbis `DATE`, bulk XML `dc:date`). Only strings that name a month and
/// a day are accepted: a year-only value carries no month/day and is left
/// to the caller's fallback rather than emitting a placeholding `01`/`01`.
/// Recognized forms are ISO-8601-ish: `2024-03-15T12:34:56`,
/// `2024-03-15 12:34:56`, `2024-03-15`, `2024/03/15`, and the
/// EXIF colon form `2024:03:15 12:34:56`.
pub(crate) fn parse_tag_date_parts(s: &str) -> Option<DateParts> {
    use chrono::{Datelike, Timelike};
    let s = s.trim();
    if let Some(dp) = parse_exif_datetime(s) {
        return Some(dp);
    }
    const FORMATS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d",
        "%Y/%m/%d",
    ];
    for fmt in FORMATS {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            let p = DateParts {
                year: ndt.year(),
                month: ndt.month(),
                day: ndt.day(),
                hour: ndt.hour(),
                minute: ndt.minute(),
                second: ndt.second(),
            };
            return DateParts::valid_year(p.year).then_some(p);
        }
        if let Ok(nd) = chrono::NaiveDate::parse_from_str(s, fmt) {
            let p = DateParts {
                year: nd.year(),
                month: nd.month(),
                day: nd.day(),
                hour: 0,
                minute: 0,
                second: 0,
            };
            return DateParts::valid_year(p.year).then_some(p);
        }
    }
    None
}

/// Calendar parts used by `%Y %y %m %d %H %M %S` placeholders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateParts {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl DateParts {
    /// Convert to a `SystemTime` for fallback comparisons.
    pub fn to_system_time(&self) -> Option<SystemTime> {
        use chrono::TimeZone;
        let local = chrono::Local;
        local
            .with_ymd_and_hms(
                self.year,
                self.month,
                self.day,
                self.hour,
                self.minute,
                self.second,
            )
            .single()
            .map(|dt| dt.into())
    }

    pub fn valid_year(y: i32) -> bool {
        (1970..=2100).contains(&y)
    }

    fn valid(&self) -> bool {
        Self::valid_year(self.year)
            && (1..=12).contains(&self.month)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
            && self.calendar_day_ok()
    }

    /// True when the month/day combination exists in the calendar
    /// (Feb 31, Apr 31, and leap-day rules included).
    fn calendar_day_ok(&self) -> bool {
        use chrono::NaiveDate;
        NaiveDate::from_ymd_opt(self.year, self.month, self.day).is_some()
    }
}

fn digits(s: &[u8]) -> Option<u32> {
    s.iter().try_fold(0u32, |acc, b| {
        if b.is_ascii_digit() {
            Some(acc * 10 + u32::from(b - b'0'))
        } else {
            None
        }
    })
}

/// Parse a date from a file name (stem plus extension tolerated).
pub fn parse_filename_date(name: &str) -> Option<DateParts> {
    let name = name.trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '-');
    // Whole-name digit runs are enough; scan for the first plausible
    // pattern and stop at the first hit (conservative: no cross-field
    // guessing).
    let bytes = name.as_bytes();

    // 1) YYYYMMDD_HHMMSS or YYYYMMDD-HHMMSS or YYYYMMDDHHMMSS (>= 14 digits
    //    in a row after an optional prefix such as IMG_/PXL_).
    // 2) YYYY-MM-DD[_HH-MM-SS].
    // 3) YYYYMMDD (exactly 8 digits in a row).

    // Form 2 with separators.
    if let Some(d) = parse_separated(bytes) {
        return Some(d);
    }
    // Forms 1 and 3: collect contiguous digit runs, then try the most
    // plausible interpretation, stopping at the first hit.
    let mut runs: Vec<&[u8]> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        runs.push(&bytes[start..i]);
    }
    for (idx, run) in runs.iter().enumerate() {
        if run.len() < 8 {
            continue;
        }
        let d = DateParts {
            year: digits(&run[0..4])? as i32,
            month: digits(&run[4..6])?,
            day: digits(&run[6..8])?,
            hour: 0,
            minute: 0,
            second: 0,
        };
        if !d.valid() {
            continue;
        }
        // Time in the same run (YYYYMMDDHHMMSS or longer with ms) or in a
        // following run separated by `_`/`-` (HHMMSS or HHMMSSmmm).
        if run.len() >= 14 {
            if let Some(t) = time_from(&run[8..14]) {
                return Some(DateParts {
                    hour: t.0,
                    minute: t.1,
                    second: t.2,
                    ..d
                });
            }
        }
        if let Some(next) = runs.get(idx + 1) {
            if next.len() >= 6 {
                if let Some(t) = time_from(&next[0..6]) {
                    return Some(DateParts {
                        hour: t.0,
                        minute: t.1,
                        second: t.2,
                        ..d
                    });
                }
            }
        }
        return Some(d);
    }
    None
}

/// Parse `HHMMSS` from six bytes.
fn time_from(b: &[u8]) -> Option<(u32, u32, u32)> {
    if b.len() < 6 {
        return None;
    }
    let hour = digits(&b[0..2])?;
    let minute = digits(&b[2..4])?;
    let second = digits(&b[4..6])?;
    if hour < 24 && minute < 60 && second < 60 {
        Some((hour, minute, second))
    } else {
        None
    }
}

/// `YYYY-MM-DD` optionally followed by `_HH-MM-SS` or `-HH:MM:SS`.
fn parse_separated(bytes: &[u8]) -> Option<DateParts> {
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let d = DateParts {
        year: digits(&bytes[0..4])? as i32,
        month: digits(&bytes[5..7])?,
        day: digits(&bytes[8..10])?,
        hour: 0,
        minute: 0,
        second: 0,
    };
    if !d.valid() {
        return None;
    }
    // Optional time following `_` or ` ` then HHMMSS / HH:MM:SS / HH-MM-SS.
    let rest = &bytes[10..];
    if let Some(pos) = rest
        .iter()
        .position(|b| *b == b'_' || *b == b' ' || *b == b'T')
    {
        let t = &rest[pos + 1..];
        if t.len() == 6 {
            let hour = digits(&t[0..2])?;
            let minute = digits(&t[2..4])?;
            let second = digits(&t[4..6])?;
            if hour < 24 && minute < 60 && second < 60 {
                return Some(DateParts {
                    hour,
                    minute,
                    second,
                    ..d
                });
            }
        }
    }
    Some(d)
}

/// Parse an EXIF-style date-time string into `DateParts`.
///
/// EXIF stores `YYYY:MM:DD HH:MM:SS` (the separator is `:`, the date and
/// time are space-separated); date-only values, `-`/`/` separators, and a
/// `T` date/time separator are tolerated. The calendar is validated with
/// the same conservative rules as filename dates.
pub fn parse_exif_datetime(s: &str) -> Option<DateParts> {
    let b = s.as_bytes();
    if b.len() < 10 || !b[0..4].iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut parts = DateParts {
        year: digits(&b[0..4])? as i32,
        month: digits(&b[5..7])?,
        day: digits(&b[8..10])?,
        hour: 0,
        minute: 0,
        second: 0,
    };
    if !parts.valid() {
        return None;
    }
    // Optional time following a space or `T`: HH, HH:MM, or HH:MM:SS.
    if b.len() >= 13 && (b[10] == b' ' || b[10] == b'T') {
        if b.len() >= 16 && b[13] == b':' {
            parts.hour = digits(&b[11..13])?;
            parts.minute = digits(&b[14..16])?;
            parts.second = if b.len() >= 19 && b[16] == b':' {
                digits(&b[17..19])?
            } else {
                0
            };
        } else if b.len() >= 15 && b[13] == b':' {
            parts.hour = digits(&b[11..13])?;
            parts.minute = digits(&b[14..16])?;
        } else {
            parts.hour = digits(&b[11..13])?;
        }
        if parts.hour >= 24 || parts.minute >= 60 || parts.second >= 60 {
            return None;
        }
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exif_datetimes() {
        assert_eq!(
            parse_exif_datetime("2024:03:15 15:30:45").unwrap(),
            DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 15,
                minute: 30,
                second: 45
            }
        );
        assert_eq!(parse_exif_datetime("2024-03-15T15:30:45").unwrap().hour, 15);
        assert_eq!(parse_exif_datetime("2024/03/15").unwrap().day, 15);
        assert!(parse_exif_datetime("2024:13:01 00:00:00").is_none());
        assert!(parse_exif_datetime("2024:02:31 00:00:00").is_none());
        assert!(parse_exif_datetime("junk").is_none());
    }

    #[test]
    fn parses_common_forms() {
        assert_eq!(
            parse_filename_date("IMG_20240315_153045.jpg").unwrap(),
            DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 15,
                minute: 30,
                second: 45
            }
        );
        assert_eq!(
            parse_filename_date("PXL_20240315_153045123.jpg").unwrap(),
            DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 15,
                minute: 30,
                second: 45
            }
        );
        assert_eq!(
            parse_filename_date("2024-03-15.jpg").unwrap(),
            DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
        assert_eq!(
            parse_filename_date("20240315.jpg").unwrap(),
            DateParts {
                year: 2024,
                month: 3,
                day: 15,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
    }

    #[test]
    fn rejects_implausible_dates_and_noise() {
        assert!(parse_filename_date("readme.txt").is_none());
        assert!(parse_filename_date("invoice_12345678.pdf").is_none()); // year 1234 out of range
        assert!(parse_filename_date("IMG_21010315.jpg").is_none()); // year 2101 out of range
        assert!(parse_filename_date("IMG_20241301.jpg").is_none()); // month 13
        assert!(parse_filename_date("IMG_20240231.jpg").is_none()); // Feb 31
    }
}
