//! Timestamps and ISO 8601 durations. Every time in this crate is UTC.

use chrono::{DateTime, Months, NaiveDate, Utc};
use thiserror::Error;

pub type Timestamp = DateTime<Utc>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TimeError {
    #[error("unparseable timestamp: {0}")]
    Timestamp(String),
    #[error("unparseable ISO 8601 duration: {0}")]
    Duration(String),
}

/// Parse an RFC 3339 timestamp or a bare `YYYY-MM-DD` date (midnight UTC).
pub fn parse_time(text: &str) -> Result<Timestamp, TimeError> {
    let text = text.trim();
    if let Ok(time) = DateTime::parse_from_rfc3339(text) {
        return Ok(time.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc())
        .ok_or_else(|| TimeError::Timestamp(text.to_string()))
}

/// RFC 3339 with second precision and a `Z` suffix, as written into frontmatter.
pub fn format_time(time: Timestamp) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// An ISO 8601 duration `PnYnMnWnDTnHnMnS` with integer components.
/// Years and months use calendar arithmetic; the rest are fixed lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoDuration {
    pub months: u32,
    pub seconds: i64,
}

impl IsoDuration {
    pub fn parse(text: &str) -> Result<Self, TimeError> {
        let error = || TimeError::Duration(text.to_string());
        let rest = text.trim().strip_prefix('P').ok_or_else(error)?;
        if rest.is_empty() {
            return Err(error());
        }
        let mut duration = Self::default();
        let mut in_time = false;
        let mut number = String::new();
        let mut seen_component = false;
        let mut seen_time_component = false;
        // Components appear at most once, in `Y M W D T H M S` order.
        let mut last_rank = 0;
        for ch in rest.chars() {
            if ch.is_ascii_digit() {
                number.push(ch);
                continue;
            }
            if ch == 'T' {
                if in_time || !number.is_empty() {
                    return Err(error());
                }
                in_time = true;
                continue;
            }
            let rank = component_rank(in_time, ch).ok_or_else(error)?;
            if rank <= last_rank {
                return Err(error());
            }
            last_rank = rank;
            let n: i64 = number.parse().map_err(|_| error())?;
            number.clear();
            seen_component = true;
            seen_time_component |= in_time;
            match (in_time, ch) {
                (false, 'Y') => {
                    duration.months = add_months(duration.months, n.checked_mul(12))?;
                }
                (false, 'M') => duration.months = add_months(duration.months, Some(n))?,
                (false, 'W') => duration.seconds = add_seconds(duration.seconds, n, 7 * 86_400)?,
                (false, 'D') => duration.seconds = add_seconds(duration.seconds, n, 86_400)?,
                (true, 'H') => duration.seconds = add_seconds(duration.seconds, n, 3_600)?,
                (true, 'M') => duration.seconds = add_seconds(duration.seconds, n, 60)?,
                (true, 'S') => duration.seconds = add_seconds(duration.seconds, n, 1)?,
                _ => return Err(error()),
            }
        }
        if !number.is_empty() || !seen_component || (in_time && !seen_time_component) {
            return Err(error());
        }
        Ok(duration)
    }

    /// `start + self`, or `None` on overflow.
    pub fn after(&self, start: Timestamp) -> Option<Timestamp> {
        start
            .checked_add_months(Months::new(self.months))?
            .checked_add_signed(chrono::TimeDelta::try_seconds(self.seconds)?)
    }

    pub fn days(days: i64) -> Self {
        Self {
            months: 0,
            seconds: days.saturating_mul(86_400),
        }
    }
}

/// Position of a duration designator in the ISO 8601 order.
fn component_rank(in_time: bool, designator: char) -> Option<u8> {
    match (in_time, designator) {
        (false, 'Y') => Some(1),
        (false, 'M') => Some(2),
        (false, 'W') => Some(3),
        (false, 'D') => Some(4),
        (true, 'H') => Some(5),
        (true, 'M') => Some(6),
        (true, 'S') => Some(7),
        _ => None,
    }
}

fn add_months(current: u32, add: Option<i64>) -> Result<u32, TimeError> {
    add.and_then(|n| u32::try_from(n).ok())
        .and_then(|n| current.checked_add(n))
        .ok_or_else(|| TimeError::Duration("month component overflow".into()))
}

fn add_seconds(current: i64, n: i64, unit: i64) -> Result<i64, TimeError> {
    n.checked_mul(unit)
        .and_then(|s| current.checked_add(s))
        .ok_or_else(|| TimeError::Duration("duration overflow".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_and_refuse() {
        assert_eq!(IsoDuration::parse("P7D").unwrap(), IsoDuration::days(7));
        assert_eq!(IsoDuration::parse("PT36H").unwrap().seconds, 36 * 3600);
        assert_eq!(IsoDuration::parse("P1Y2M").unwrap().months, 14);
        assert_eq!(IsoDuration::parse("P1W").unwrap(), IsoDuration::days(7));
        assert_eq!(IsoDuration::parse("P1Y2M3DT4H5M6S").unwrap().months, 14);
        for bad in [
            "",
            "P",
            "7D",
            "PT",
            "P1H",
            "P1DT",
            "PxD",
            "P1.5D",
            "P1D1D",
            "PT1S1H",
            "P1M1Y",
            "PT1H1H",
            "P1D1W",
            "PTT1H",
            "P1DT1HT1M",
        ] {
            assert!(IsoDuration::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn timestamps_accept_rfc3339_and_dates() {
        let t = parse_time("2026-11-05T09:00:00+09:00").unwrap();
        assert_eq!(format_time(t), "2026-11-05T00:00:00Z");
        assert_eq!(
            format_time(parse_time("2026-11-05").unwrap()),
            "2026-11-05T00:00:00Z"
        );
        assert!(parse_time("yesterday").is_err());
    }
}
