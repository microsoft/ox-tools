// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Semantic-version arithmetic and change-type ranking.
//!
//! Strict `SemVer` 2.0 parsing and ordering, next-version computation under
//! Cargo's `0.x` compatibility rules, the change type implied by a numeric
//! transition, and the breaking-change test.
//!
//! # Vocabulary
//!
//! * **change type** — the semantic intent of a release: breaking /
//!   non-breaking / patch. This is what a user thinks about.
//! * **version component** — a position in the `major.minor.patch` triple.
//!   These names are *positional*, not semantic.
//!
//! The mapping from change type to the incremented component depends on the
//! current version:
//!
//! * `x.y.z` (`x >= 1`): breaking → `(x+1).0.0`, non-breaking → `x.(y+1).0`,
//!   patch → `x.y.(z+1)`.
//! * `0.x.y` (`x >= 1`): breaking → `0.(x+1).0` (the *minor* moves), and both
//!   non-breaking and patch → `0.x.(y+1)`.
//! * `0.0.x`: every change → `0.0.(x+1)` (every change is breaking).

use std::cmp::Ordering;
use std::sync::OnceLock;

use ohno::{AppError, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// A release change type, ordered by strength.
///
/// [`ChangeType::None`] means "no constraint" (for example, when
/// `cargo semver-checks` found nothing to compare against) and ranks below
/// every real change type. Ordering follows
/// `none < patch < non-breaking < breaking` — the variant declaration order —
/// so [`Ord`] gives the "stronger of two change types" for free via
/// [`Ord::max`].
///
/// The serialized form is the one-word lowercase spelling (`nonbreaking`, not
/// `non-breaking`), matching the release plan's wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeType {
    /// No constraint; ranks below every real change type.
    None,
    /// A backward-compatible bug fix.
    Patch,
    /// A backward-compatible feature addition.
    NonBreaking,
    /// A backward-incompatible change.
    Breaking,
}

impl ChangeType {
    /// The canonical internal spelling (`non-breaking`, not `nonbreaking`).
    #[must_use]
    pub fn internal_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Patch => "patch",
            Self::NonBreaking => "non-breaking",
            Self::Breaking => "breaking",
        }
    }

    /// The macro-contract verdict spelling for this change type
    /// (`compatible` / `nonbreaking` / `breaking`).
    #[must_use]
    pub fn macro_verdict_name(self) -> &'static str {
        match self {
            Self::Breaking => "breaking",
            Self::NonBreaking => "nonbreaking",
            // A patch or "none" macro contract is compatible.
            Self::None | Self::Patch => "compatible",
        }
    }

    /// Parses an accepted change-type token.
    ///
    /// Accepts `breaking`, `nonbreaking`, `non-breaking`, and `patch`. The
    /// `none` token is accepted only when `allow_none` is set.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is empty, `none` without `allow_none`,
    /// or otherwise unrecognized.
    pub fn parse(value: &str, allow_none: bool) -> Result<Self, AppError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            if allow_none {
                return Ok(Self::None);
            }
            bail!("A change type is required.");
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "breaking" => Ok(Self::Breaking),
            "nonbreaking" | "non-breaking" => Ok(Self::NonBreaking),
            "patch" => Ok(Self::Patch),
            "none" => {
                if allow_none {
                    Ok(Self::None)
                } else {
                    bail!("Change type 'none' is not valid here.")
                }
            }
            other => bail!("Unknown change type '{other}'."),
        }
    }

    /// Parses a macro-contract verdict into the change type it implies.
    ///
    /// `compatible` → patch, `nonbreaking` → non-breaking, `breaking` →
    /// breaking.
    ///
    /// # Errors
    ///
    /// Returns an error for an unrecognized verdict.
    pub fn parse_macro_verdict(value: &str) -> Result<Self, AppError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "compatible" => Ok(Self::Patch),
            "nonbreaking" | "non-breaking" => Ok(Self::NonBreaking),
            "breaking" => Ok(Self::Breaking),
            other => bail!("Unknown macro-contract verdict '{other}'."),
        }
    }
}

/// A strictly parsed semantic version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
    /// Pre-release identifier chain (empty when absent).
    pub pre_release: String,
    /// Build metadata (empty when absent).
    pub build: String,
}

fn semver_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The canonical SemVer 2.0 pattern: three numeric components with no
        // leading zeros, an optional `-prerelease`, and optional `+build`.
        Regex::new(
            r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$",
        )
        .expect("the embedded SemVer 2.0 regex is a compile-time constant and always parses")
    })
}

impl SemVer {
    /// Strictly parses a `SemVer` string.
    ///
    /// Rejects 1- or 2-component forms, leading zeros, and other non-canonical
    /// inputs that a lenient parser might accept.
    ///
    /// # Errors
    ///
    /// Returns an error when `version` is not a canonical `SemVer` 2.0 string.
    pub fn parse(version: &str) -> Result<Self, AppError> {
        let captures = semver_regex().captures(version).ok_or_else(|| {
            ohno::app_err!(
                "Invalid SemVer version '{version}'. Expected the form \
                 <major>.<minor>.<patch>[-<prerelease>][+<build>] with exactly three numeric \
                 components (no leading zeros)."
            )
        })?;
        // Each numeric group is guaranteed present and digit-only by the regex,
        // but could overflow u64 for absurd inputs; treat overflow as invalid.
        let component = |index: usize| -> Result<u64, AppError> {
            captures
                .get(index)
                .map_or("", |m| m.as_str())
                .parse::<u64>()
                .map_err(|source| ohno::app_err!("SemVer component in '{version}' is out of range: {source}"))
        };
        Ok(Self {
            major: component(1)?,
            minor: component(2)?,
            patch: component(3)?,
            pre_release: captures.get(4).map_or("", |m| m.as_str()).to_string(),
            build: captures.get(5).map_or("", |m| m.as_str()).to_string(),
        })
    }
}

/// Strictly parses a `SemVer` string, discarding the parsed structure.
///
/// # Errors
///
/// Returns an error when `version` is not a canonical `SemVer` 2.0 string.
pub fn validate_version(version: &str) -> Result<(), AppError> {
    SemVer::parse(version).map(|_| ())
}

fn compare_pre_release(left: &str, right: &str) -> Ordering {
    // A version with a pre-release has lower precedence than the same version
    // without one (SemVer 2.0 §11.3).
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    let mut left_ids = left.split('.');
    let mut right_ids = right.split('.');
    loop {
        match (left_ids.next(), right_ids.next()) {
            (None, None) => return Ordering::Equal,
            // A larger set of pre-release fields has higher precedence when all
            // preceding identifiers are equal (§11.4.4).
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) => {
                let ordering = compare_pre_release_identifier(a, b);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

fn compare_pre_release_identifier(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        // Numeric identifiers compare numerically (§11.4.1).
        (Ok(a), Ok(b)) => a.cmp(&b),
        // Numeric identifiers always have lower precedence than alphanumeric
        // ones (§11.4.3).
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        // Alphanumeric identifiers compare lexically in ASCII order (§11.4.2).
        (Err(_), Err(_)) => left.cmp(right),
    }
}

/// Compares two `SemVer` strings, returning `SemVer` 2.0 ordering.
///
/// Build metadata is ignored per spec. Both inputs are validated strictly and
/// will error on invalid input.
///
/// # Errors
///
/// Returns an error when either input is not a canonical `SemVer` 2.0 string.
pub fn compare_versions(version1: &str, version2: &str) -> Result<Ordering, AppError> {
    let a = SemVer::parse(version1)?;
    let b = SemVer::parse(version2)?;
    Ok(a.major
        .cmp(&b.major)
        .then_with(|| a.minor.cmp(&b.minor))
        .then_with(|| a.patch.cmp(&b.patch))
        .then_with(|| compare_pre_release(&a.pre_release, &b.pre_release)))
}

/// Computes the next version for the given change type, honoring Cargo's
/// `0.x.y` `SemVer` rules.
///
/// Any pre-release or build suffix on the input is dropped; a release is always
/// a clean `SemVer` triple.
///
/// # Errors
///
/// Returns an error when `current_version` is not a canonical `SemVer` 2.0
/// string, or when `change_type` is [`ChangeType::None`] (which carries no
/// increment).
pub fn next_version(current_version: &str, change_type: ChangeType) -> Result<String, AppError> {
    let SemVer { major, minor, patch, .. } = SemVer::parse(current_version)?;
    let next = if major >= 1 {
        match change_type {
            ChangeType::Breaking => format!("{}.0.0", major + 1),
            ChangeType::NonBreaking => format!("{major}.{}.0", minor + 1),
            ChangeType::Patch => format!("{major}.{minor}.{}", patch + 1),
            ChangeType::None => bail!("Change type 'none' has no version increment."),
        }
    } else if minor >= 1 {
        match change_type {
            ChangeType::Breaking => format!("0.{}.0", minor + 1),
            ChangeType::NonBreaking | ChangeType::Patch => format!("0.{minor}.{}", patch + 1),
            ChangeType::None => bail!("Change type 'none' has no version increment."),
        }
    } else {
        format!("0.0.{}", patch + 1)
    };
    Ok(next)
}

/// Recovers the change type implied by an `old → new` transition.
///
/// Returns the conservative lower bound: on a `0.x.y` package a `0.4.1 → 0.4.2`
/// transition could be either non-breaking or patch, and this returns patch —
/// the tightest claim available from numbers alone.
///
/// # Errors
///
/// Returns an error when either input is not a canonical `SemVer` 2.0 string.
pub fn change_type_from_versions(old_version: &str, new_version: &str) -> Result<ChangeType, AppError> {
    let old = SemVer::parse(old_version)?;
    let new = SemVer::parse(new_version)?;
    if old.major >= 1 {
        if new.major != old.major {
            return Ok(ChangeType::Breaking);
        }
        if new.minor != old.minor {
            return Ok(ChangeType::NonBreaking);
        }
        return Ok(ChangeType::Patch);
    }
    if old.minor >= 1 {
        if new.minor != old.minor {
            return Ok(ChangeType::Breaking);
        }
        return Ok(ChangeType::Patch);
    }
    Ok(ChangeType::Breaking)
}

/// Whether a change of the given type on `old_version` moves the Cargo
/// compatibility line (i.e. is breaking to consumers).
///
/// # Errors
///
/// Returns an error when `old_version` is not a canonical `SemVer` 2.0 string.
pub fn is_breaking_change(old_version: &str, change_type: ChangeType) -> Result<bool, AppError> {
    let parts = SemVer::parse(old_version)?;
    if parts.major >= 1 || parts.minor >= 1 {
        return Ok(change_type == ChangeType::Breaking);
    }
    // On `0.0.x` every change is breaking.
    Ok(true)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn change_type_ordering_is_by_rank() {
        assert!(ChangeType::Breaking > ChangeType::NonBreaking);
        assert!(ChangeType::NonBreaking > ChangeType::Patch);
        assert!(ChangeType::Patch > ChangeType::None);
        assert_eq!(ChangeType::Patch.max(ChangeType::Breaking), ChangeType::Breaking);
    }

    #[test]
    fn next_version_stable_line() {
        assert_eq!(next_version("1.4.2", ChangeType::Breaking).unwrap(), "2.0.0");
        assert_eq!(next_version("1.4.2", ChangeType::NonBreaking).unwrap(), "1.5.0");
        assert_eq!(next_version("1.4.2", ChangeType::Patch).unwrap(), "1.4.3");
    }

    #[test]
    fn next_version_zero_x_line() {
        assert_eq!(next_version("0.4.0", ChangeType::Breaking).unwrap(), "0.5.0");
        assert_eq!(next_version("0.4.0", ChangeType::NonBreaking).unwrap(), "0.4.1");
        assert_eq!(next_version("0.4.0", ChangeType::Patch).unwrap(), "0.4.1");
    }

    #[test]
    fn next_version_zero_zero_line() {
        assert_eq!(next_version("0.0.3", ChangeType::Patch).unwrap(), "0.0.4");
        assert_eq!(next_version("0.0.3", ChangeType::Breaking).unwrap(), "0.0.4");
    }

    #[test]
    fn compare_versions_orders_pre_release_below_release() {
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("0.5.0", "0.4.9").unwrap(), Ordering::Greater);
    }

    #[test]
    fn parse_rejects_non_canonical() {
        SemVer::parse("01.2.3").unwrap_err();
        SemVer::parse("1.2").unwrap_err();
        SemVer::parse("1.2.3.4").unwrap_err();
        SemVer::parse("").unwrap_err();
    }

    #[test]
    fn change_type_from_versions_is_conservative_on_zero_x() {
        assert_eq!(change_type_from_versions("0.4.1", "0.4.2").unwrap(), ChangeType::Patch);
        assert_eq!(change_type_from_versions("0.4.1", "0.5.0").unwrap(), ChangeType::Breaking);
        assert_eq!(change_type_from_versions("1.4.1", "1.5.0").unwrap(), ChangeType::NonBreaking);
    }

    #[test]
    fn is_breaking_change_follows_cargo_zero_rules() {
        assert!(is_breaking_change("0.0.3", ChangeType::Patch).unwrap());
        assert!(!is_breaking_change("0.4.0", ChangeType::Patch).unwrap());
        assert!(is_breaking_change("0.4.0", ChangeType::Breaking).unwrap());
        assert!(!is_breaking_change("1.4.0", ChangeType::NonBreaking).unwrap());
    }
}
