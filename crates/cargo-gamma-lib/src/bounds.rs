// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Range checks for the numbers a user can supply.
//!
//! Every one of these values reaches a `Duration` eventually, and `Duration`'s float constructors
//! *panic* on a negative, `NaN` or over-large argument. A mistyped flag or a bad line in
//! `gamma.toml` should be a diagnostic, not a crash report, so the checks live here and are
//! applied at both entry points rather than at the point of use.

use core::time::Duration;

/// The largest multiplier worth accepting.
///
/// The bound exists so that scaling a baseline by it cannot overflow a `Duration`; the exact value
/// is arbitrary, since nothing sane is anywhere near it.
///
/// `pub` within this private module so the crate's proc-macro agreement test can reach it through
/// the `internals` facade and pin the proc-macro's hand-copied `MOST_FACTOR` against it.
pub const MOST_FACTOR: f64 = 1e6;

/// The largest timeout accepted from configuration: one year.
pub(crate) const MOST_SECONDS: u64 = 365 * 24 * 60 * 60;
const MOST_SECONDS_F64: f64 = 31_536_000.0;

/// Checks a number of seconds that a `Duration` has to be able to represent.
///
/// # Errors
///
/// Returns a message naming the offending text if it is not a positive, finite, representable
/// number of seconds.
pub fn seconds(text: &str, value: f64) -> Result<f64, String> {
    let value = positive(text, value)?;

    if value > MOST_SECONDS_F64 {
        return Err(format!("`{text}` is unreasonably large; the most is {MOST_SECONDS} seconds"));
    }

    Duration::try_from_secs_f64(value)
        .map(|_duration| value)
        .map_err(|_cause| format!("`{text}` is too large to be a duration"))
}

/// Checks a multiplier, bounded so that scaling a baseline by it cannot overflow a `Duration`.
///
/// # Errors
///
/// Returns a message naming the offending text if it is not a positive, finite, reasonable factor.
pub fn factor(text: &str, value: f64) -> Result<f64, String> {
    let value = positive(text, value)?;

    if value > MOST_FACTOR {
        return Err(format!("`{text}` is unreasonably large; the most is {MOST_FACTOR}"));
    }

    Ok(value)
}

/// Parses a memory size, with or without a binary unit suffix.
///
/// Sizes are the one quantity here people habitually write with a unit, and a flag that silently
/// read `512M` as five hundred and twelve bytes would install a ceiling no test could ever fit
/// under and report every mutant as caught. A suffix is therefore understood rather than rejected,
/// and an unrecognized one is an error rather than a number quietly taken from the digits in front
/// of it.
///
/// The units are binary — `K` is 1024 — because that is what every other memory figure in this tool
/// and in the kernel interfaces behind it means.
///
/// # Errors
///
/// Returns a message naming the offending text if it is not a positive, finite, representable
/// number of at least one byte, or if its suffix is not a unit.
pub fn size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let digits = trimmed.trim_end_matches(|character: char| character.is_ascii_alphabetic());
    let suffix = trimmed.strip_prefix(digits).unwrap_or_default();

    let scale: u64 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024 * 1024 * 1024 * 1024,
        _other => return Err(format!("`{text}` does not end in a size unit such as `KiB`, `MiB` or `GiB`")),
    };

    let count: f64 = digits
        .trim()
        .parse()
        .map_err(|_cause| format!("`{text}` is not a number of bytes"))?;

    let _positive = positive(text, count)?;

    #[expect(clippy::cast_precision_loss, reason = "the comparison only needs to be right near the boundary")]
    let bytes = count * scale as f64;

    #[expect(clippy::cast_precision_loss, reason = "as above")]
    let most = u64::MAX as f64;

    if bytes >= most {
        return Err(format!("`{text}` is too large to be a number of bytes"));
    }

    // Positivity was checked on the unscaled count, so a positive size that scales to less than one
    // byte reaches this point and would truncate to a zero ceiling. A zero ceiling is not a small
    // ceiling: every mutant exceeds it, so the whole population reports as detected and the run
    // claims a perfect score off a typo.
    if bytes < 1.0 {
        return Err(format!("`{text}` is less than one byte"));
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is finite, positive and below `u64::MAX` by the checks above"
    )]
    let bytes = bytes as u64;

    Ok(bytes)
}

/// Checks a percentage between zero and a hundred.
///
/// # Errors
///
/// Returns a message naming the offending text if it is outside that range. `NaN` is rejected
/// rather than passed through: it compares false against every score, so a gate set to it would
/// silently never fire.
pub fn percentage(text: &str, value: f64) -> Result<f64, String> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(format!("`{text}` is not a percentage between 0 and 100"));
    }

    Ok(value)
}

/// Checks that a number is finite and greater than zero.
fn positive(text: &str, value: f64) -> Result<f64, String> {
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("`{text}` must be a number greater than zero"));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_that_would_panic_is_rejected_instead() {
        // Each of these reaches `Duration::from_secs_f64`, which panics rather than returning.
        _ = seconds("-5", -5.0).expect_err("out of range");
        _ = seconds("nan", f64::NAN).expect_err("out of range");
        _ = seconds("1e300", 1e300).expect_err("out of range");
        _ = seconds("0", 0.0).expect_err("out of range");
        _ = seconds("1.5", 1.5).expect("in range");
        assert_eq!(seconds(&MOST_SECONDS.to_string(), MOST_SECONDS_F64), Ok(MOST_SECONDS_F64));
        _ = seconds(&(MOST_SECONDS + 1).to_string(), MOST_SECONDS_F64 + 1.0).expect_err("past the usable deadline range");
    }

    #[test]
    fn a_factor_is_positive_and_not_absurd() {
        _ = factor("-1", -1.0).expect_err("out of range");
        _ = factor("nan", f64::NAN).expect_err("out of range");
        _ = factor("1e9", 1e9).expect_err("out of range");
        _ = factor("1.2", 1.2).expect("in range");
        assert_eq!(factor("1000000", MOST_FACTOR), Ok(MOST_FACTOR));
    }

    #[test]
    fn a_size_understands_the_units_people_write() {
        assert_eq!(size("1024"), Ok(1024));
        assert_eq!(size("1k"), Ok(1024));
        assert_eq!(size("512MiB"), Ok(512 * 1024 * 1024));
        assert_eq!(size(" 2GB "), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(size("1.5G"), Ok(1024 * 1024 * 1024 + 512 * 1024 * 1024));
        assert_eq!(size("1TiB"), Ok(1024_u64.pow(4)));
    }

    #[test]
    fn a_size_that_is_not_one_is_refused_rather_than_guessed_at() {
        // Reading `512Mb/s` as five hundred and twelve would install a ceiling no suite could fit
        // under, and every mutant would then be reported as caught by tests that never ran.
        _ = size("512Mb/s").expect_err("not a size");
        _ = size("many").expect_err("not a size");
        _ = size("0").expect_err("not a size");
        _ = size("-1M").expect_err("not a size");
        _ = size("1e30G").expect_err("not a size");
        _ = size("18446744073709551616").expect_err("exactly one past u64::MAX");
    }

    #[test]
    fn a_size_below_one_byte_is_refused_rather_than_truncated_to_a_zero_ceiling() {
        // Positivity is checked on the unscaled count, so these pass it and then truncate. A zero
        // ceiling is the worst possible failure: every mutant exceeds it, so the run reports a
        // perfect score without a single test having judged anything.
        assert_eq!(size("0.5"), Err("`0.5` is less than one byte".to_owned()));
        assert_eq!(size("0.5B"), Err("`0.5B` is less than one byte".to_owned()));
        assert_eq!(size("0.0001k"), Err("`0.0001k` is less than one byte".to_owned()));

        // The boundary itself is a legitimate size and must survive the new check.
        assert_eq!(size("1"), Ok(1));
        assert_eq!(size("1B"), Ok(1));
    }

    #[test]
    fn a_percentage_stays_within_its_range() {
        // `NaN` is the interesting one: it compares false against every score, so a gate set to it
        // would pass silently forever.
        _ = percentage("nan", f64::NAN).expect_err("out of range");
        _ = percentage("150", 150.0).expect_err("out of range");
        _ = percentage("-1", -1.0).expect_err("out of range");
        _ = percentage("0", 0.0).expect("in range");
        _ = percentage("100", 100.0).expect("in range");
    }
}
