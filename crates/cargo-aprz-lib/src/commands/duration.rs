// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serde support for durations written the way a person writes them.
//!
//! Cache lifetimes are strings on disk (`crates_cache_ttl = "1 week"`) and [`Duration`] values in
//! memory. The grammar is a run of `<amount><unit>` pairs with optional spaces around them, so
//! `"1 week"`, `"7days"`, `"1h 30m"` and `"250ms"` are all accepted. It is the grammar existing
//! configuration files already use, so this module is a drop-in for what the `humantime-serde`
//! dependency used to do.
//!
//! A month is 30.44 days and a year is 365.25 days, matching that established grammar. Both are
//! averages, so prefer days or weeks when the exact span matters.

use core::fmt::{self, Write as _};
use core::time::Duration;

use serde::de::Visitor;
use serde::{Deserializer, Serializer};

/// Nanoseconds in one second.
const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Seconds in one day.
const SECS_PER_DAY: u64 = 86_400;

/// Seconds in an average month of 30.44 days.
const SECS_PER_MONTH: u64 = 2_630_016;

/// Seconds in an average year of 365.25 days.
const SECS_PER_YEAR: u64 = 31_557_600;

/// Why a duration string could not be read.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ParseError {
    /// The string held no `<amount><unit>` pair at all.
    Empty,

    /// A unit appeared where an amount was expected.
    ExpectedAmount,

    /// An amount was not followed by a unit.
    MissingUnit,

    /// The unit is not one this grammar knows.
    UnknownUnit(String),

    /// The total does not fit in a [`Duration`].
    Overflow,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("expected a duration such as \"1 week\", \"12h\" or \"250ms\", found nothing"),
            Self::ExpectedAmount => f.write_str("expected a number before the unit"),
            Self::MissingUnit => f.write_str("expected a unit after the number, such as \"s\", \"h\" or \"days\""),
            Self::UnknownUnit(unit) => write!(f, "unknown time unit \"{unit}\""),
            Self::Overflow => f.write_str("duration is too large to represent"),
        }
    }
}

impl core::error::Error for ParseError {}

/// Maps a unit to the number of nanoseconds one of it lasts.
///
/// Matching is case-sensitive on purpose: `m` is minutes and `M` is months.
fn unit_nanos(unit: &str) -> Option<u128> {
    let nanos = match unit {
        "ns" | "nsec" | "nsecs" | "nanos" | "nanosecond" | "nanoseconds" => 1,
        "us" | "\u{b5}s" | "usec" | "usecs" | "micros" | "microsecond" | "microseconds" => 1_000,
        "ms" | "msec" | "msecs" | "millis" | "millisecond" | "milliseconds" => 1_000_000,
        "s" | "sec" | "secs" | "second" | "seconds" => NANOS_PER_SEC,
        "m" | "min" | "mins" | "minute" | "minutes" => 60 * NANOS_PER_SEC,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600 * NANOS_PER_SEC,
        "d" | "day" | "days" => u128::from(SECS_PER_DAY) * NANOS_PER_SEC,
        "w" | "week" | "weeks" => 7 * u128::from(SECS_PER_DAY) * NANOS_PER_SEC,
        "M" | "month" | "months" => u128::from(SECS_PER_MONTH) * NANOS_PER_SEC,
        "y" | "year" | "years" => u128::from(SECS_PER_YEAR) * NANOS_PER_SEC,
        _ => return None,
    };

    Some(nanos)
}

/// Reads a duration such as `"1 week"` or `"1h 30m"`.
pub(super) fn parse(text: &str) -> Result<Duration, ParseError> {
    let mut rest = text;
    let mut total: u128 = 0;
    let mut saw_pair = false;

    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }

        let amount_len = rest.find(|character: char| !character.is_ascii_digit()).unwrap_or(rest.len());
        let (amount, tail) = rest.split_at(amount_len);
        if amount.is_empty() {
            return Err(ParseError::ExpectedAmount);
        }
        let amount: u128 = amount.parse().ok().ok_or(ParseError::Overflow)?;

        let tail = tail.trim_start();
        let unit_len = tail
            .find(|character: char| character.is_ascii_digit() || character.is_whitespace())
            .unwrap_or(tail.len());
        let (unit, remainder) = tail.split_at(unit_len);
        if unit.is_empty() {
            return Err(ParseError::MissingUnit);
        }
        let nanos = unit_nanos(unit).ok_or_else(|| ParseError::UnknownUnit(unit.to_owned()))?;

        total = amount
            .checked_mul(nanos)
            .and_then(|scaled| total.checked_add(scaled))
            .ok_or(ParseError::Overflow)?;
        saw_pair = true;
        rest = remainder;
    }

    if !saw_pair {
        return Err(ParseError::Empty);
    }

    let secs = u64::try_from(total / NANOS_PER_SEC).ok().ok_or(ParseError::Overflow)?;
    let nanos = u32::try_from(total % NANOS_PER_SEC).expect("a remainder modulo one billion always fits in a u32");

    Ok(Duration::new(secs, nanos))
}

/// Appends one `<amount><unit>` pair, separating it from anything already written.
fn write_pair(out: &mut String, amount: u64, unit: &str, plural: bool) {
    if !out.is_empty() {
        out.push(' ');
    }

    let _ = write!(out, "{amount}{unit}");
    if plural && amount > 1 {
        out.push('s');
    }
}

/// Writes a duration in the grammar [`parse`] reads, largest unit first.
pub(super) fn format(value: Duration) -> String {
    let mut secs = value.as_secs();
    let nanos = value.subsec_nanos();
    if secs == 0 && nanos == 0 {
        return "0s".to_owned();
    }

    let mut out = String::new();
    for (unit_secs, unit, plural) in [
        (SECS_PER_YEAR, "year", true),
        (SECS_PER_MONTH, "month", true),
        (SECS_PER_DAY, "day", true),
        (3_600, "h", false),
        (60, "m", false),
        (1, "s", false),
    ] {
        let amount = secs / unit_secs;
        if amount > 0 {
            secs %= unit_secs;
            write_pair(&mut out, amount, unit, plural);
        }
    }

    for (divisor, unit) in [(1_000_000, "ms"), (1_000, "us"), (1, "ns")] {
        let amount = (nanos / divisor) % 1_000;
        if amount > 0 {
            write_pair(&mut out, u64::from(amount), unit, false);
        }
    }

    out
}

/// Serializes a [`Duration`] as a human-readable string.
///
/// # Errors
///
/// Returns any error the serializer reports while writing the string.
pub(super) fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format(*value))
}

/// Deserializes a [`Duration`] from a human-readable string.
///
/// # Errors
///
/// Returns an error when the value is not a string, or is a string this grammar cannot read.
pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_str(DurationVisitor)
}

/// Reads the string form of a duration.
#[derive(Debug)]
struct DurationVisitor;

impl Visitor<'_> for DurationVisitor {
    type Value = Duration;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a duration such as \"1 week\", \"12h\" or \"250ms\"")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        parse(v).map_err(|error| E::custom(format!("invalid duration \"{v}\": {error}")))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct Holder {
        #[serde(with = "super")]
        ttl: Duration,
    }

    #[test]
    fn units_are_read_in_every_spelling() {
        for (text, expected) in [
            ("1ns", Duration::from_nanos(1)),
            ("2nanoseconds", Duration::from_nanos(2)),
            ("1us", Duration::from_micros(1)),
            ("1\u{b5}s", Duration::from_micros(1)),
            ("3microseconds", Duration::from_micros(3)),
            ("250ms", Duration::from_millis(250)),
            ("4millis", Duration::from_millis(4)),
            ("30s", Duration::from_secs(30)),
            ("30seconds", Duration::from_secs(30)),
            ("5m", Duration::from_mins(5)),
            ("5mins", Duration::from_mins(5)),
            ("2h", Duration::from_hours(2)),
            ("2hours", Duration::from_hours(2)),
            ("3d", Duration::from_secs(3 * SECS_PER_DAY)),
            ("1 week", Duration::from_secs(7 * SECS_PER_DAY)),
            ("2w", Duration::from_secs(14 * SECS_PER_DAY)),
            ("1M", Duration::from_secs(SECS_PER_MONTH)),
            ("2months", Duration::from_secs(2 * SECS_PER_MONTH)),
            ("1y", Duration::from_secs(SECS_PER_YEAR)),
            ("2years", Duration::from_secs(2 * SECS_PER_YEAR)),
        ] {
            assert_eq!(parse(text), Ok(expected), "parsing {text}");
        }
    }

    #[test]
    fn pairs_accumulate_with_or_without_spaces() {
        assert_eq!(parse("1h30m"), Ok(Duration::from_mins(90)));
        assert_eq!(parse("  1h 30m  "), Ok(Duration::from_mins(90)));
        assert_eq!(parse("1 h 30 m"), Ok(Duration::from_mins(90)));
        assert_eq!(parse("1s 500ms"), Ok(Duration::from_millis(1_500)));
    }

    #[test]
    fn minutes_and_months_are_told_apart_by_case() {
        assert_eq!(parse("1m"), Ok(Duration::from_mins(1)));
        assert_eq!(parse("1M"), Ok(Duration::from_secs(SECS_PER_MONTH)));
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
        assert_eq!(parse("week"), Err(ParseError::ExpectedAmount));
        assert_eq!(parse("12"), Err(ParseError::MissingUnit));
        assert_eq!(parse("1h 30"), Err(ParseError::MissingUnit));
        assert_eq!(parse("5 fortnights"), Err(ParseError::UnknownUnit("fortnights".to_owned())));
        assert_eq!(parse("5 W"), Err(ParseError::UnknownUnit("W".to_owned())));
    }

    #[test]
    fn totals_beyond_a_duration_are_rejected() {
        assert_eq!(parse("99999999999999999999999999999999 years"), Err(ParseError::Overflow));
        assert_eq!(parse("18446744073709551616s"), Err(ParseError::Overflow));
        assert_eq!(parse("18446744073709551615s 1s"), Err(ParseError::Overflow));
    }

    #[test]
    fn every_parse_error_has_a_message() {
        for error in [
            ParseError::Empty,
            ParseError::ExpectedAmount,
            ParseError::MissingUnit,
            ParseError::UnknownUnit("blink".to_owned()),
            ParseError::Overflow,
        ] {
            assert!(!error.to_string().is_empty(), "{error:?} has no message");
        }
        assert!(ParseError::UnknownUnit("blink".to_owned()).to_string().contains("blink"));
    }

    #[test]
    fn durations_are_written_largest_unit_first() {
        assert_eq!(format(Duration::ZERO), "0s");
        assert_eq!(format(Duration::from_secs(7 * SECS_PER_DAY)), "7days");
        assert_eq!(format(Duration::from_secs(SECS_PER_DAY)), "1day");
        assert_eq!(format(Duration::from_mins(90)), "1h 30m");
        assert_eq!(format(Duration::from_millis(1_500)), "1s 500ms");
        assert_eq!(format(Duration::from_nanos(1_001_001)), "1ms 1us 1ns");
        assert_eq!(format(Duration::from_secs(SECS_PER_YEAR + SECS_PER_MONTH)), "1year 1month");
        assert_eq!(format(Duration::from_secs(2 * SECS_PER_YEAR)), "2years");
    }

    #[test]
    fn formatting_round_trips_through_parsing() {
        for original in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_nanos(1_001_001),
            Duration::from_millis(1_500),
            Duration::from_mins(90),
            Duration::from_secs(7 * SECS_PER_DAY),
            Duration::from_secs(2 * SECS_PER_YEAR + 3 * SECS_PER_MONTH + 4),
            Duration::new(u64::MAX / SECS_PER_YEAR * SECS_PER_YEAR, 999_999_999),
        ] {
            let text = format(original);
            assert_eq!(parse(&text), Ok(original), "round-tripping {text}");
        }
    }

    #[test]
    fn serde_round_trips_through_a_string_field() {
        let holder = Holder {
            ttl: Duration::from_secs(7 * SECS_PER_DAY),
        };

        let json = serde_json::to_string(&holder).expect("a duration always serializes");
        assert_eq!(json, r#"{"ttl":"7days"}"#);
        assert_eq!(
            serde_json::from_str::<Holder>(&json).expect("the value just written parses"),
            holder
        );
    }

    #[test]
    fn deserializing_reports_the_reason_a_value_was_rejected() {
        let error = serde_json::from_str::<Holder>(r#"{"ttl":"5 fortnights"}"#).expect_err("the unit is unknown");
        assert!(error.to_string().contains("fortnights"), "unhelpful message: {error}");

        let error = serde_json::from_str::<Holder>(r#"{"ttl":7}"#).expect_err("a number is not a duration");
        assert!(error.to_string().contains("duration"), "unhelpful message: {error}");
    }
}
