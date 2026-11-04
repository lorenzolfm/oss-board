//! GitHub's timestamps, parsed once at the boundary: a malformed one fails the
//! snapshot, so the rules compare instants and have nothing left to fail on.

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, de};

const DAY_MICROS: i64 = 86_400_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    pub fn parse(text: &str) -> Option<Self> {
        let parsed = DateTime::parse_from_rfc3339(text).ok()?;
        Some(Self(parsed.with_timezone(&Utc)))
    }

    fn micros_ago(self, now: DateTime<Utc>) -> i64 {
        (now - self.0).num_microseconds().unwrap_or(i64::MAX)
    }

    /// Phone-sized age: whole days, or whole hours (at least 1) under a day.
    pub fn ago(self, now: DateTime<Utc>) -> String {
        let micros = self.micros_ago(now);
        let days = micros.div_euclid(DAY_MICROS);
        if days >= 1 {
            return format!("{days}d");
        }
        let hours = micros.rem_euclid(DAY_MICROS) / 3_600_000_000;
        format!("{}h", hours.max(1))
    }

    pub fn days_ago(self, now: DateTime<Utc>) -> f64 {
        self.micros_ago(now) as f64 / DAY_MICROS as f64
    }
}

/// The form GitHub sends, so a fingerprint written to a card or to `state/`
/// compares equal to the one a later run computes.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text)
            .ok_or_else(|| de::Error::custom(format!("not an RFC 3339 timestamp: {text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
    }

    #[test]
    fn age_reads_in_days_then_hours_with_a_floor_of_one() {
        for (at, want) in [
            ("2026-10-12T11:00:00Z", "2d"),
            ("2026-10-13T12:00:00Z", "1d"),
            ("2026-10-13T12:00:01Z", "23h"),
            ("2026-10-14T11:59:30Z", "1h"),
        ] {
            assert_eq!(Timestamp::parse(at).unwrap().ago(now()), want, "{at}");
        }
    }

    #[test]
    fn a_fingerprint_round_trips_in_the_form_github_sends() {
        let text = "2026-10-08T17:14:49Z";
        assert_eq!(Timestamp::parse(text).unwrap().to_string(), text);
    }
}
