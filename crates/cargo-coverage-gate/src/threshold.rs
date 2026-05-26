// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Three-layer threshold resolution.
//!
//! Each workspace member's effective `min-lines` value is the first match
//! among:
//!
//! 1. [`Member::min_lines`] — per-crate `[package.metadata.coverage-gate]`.
//! 2. [`Workspace::default_min_lines`] — workspace-level
//!    `[workspace.metadata.coverage-gate]`.
//! 3. [`DEFAULT_MIN_LINES`] — the built-in default of `100.0` (full
//!    coverage required).
//!
//! The resolved [`Threshold`] carries both the value and a
//! [`ThresholdSource`] tag so the verdict table can report which layer
//! supplied the number.

use crate::workspace::{Member, Workspace};

/// The built-in `min-lines` value used when neither per-crate nor
/// workspace metadata supplies a value.
pub(crate) const DEFAULT_MIN_LINES: f64 = 100.0;

/// Which layer of the resolution stack a threshold came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThresholdSource {
    /// `[package.metadata.coverage-gate]` in the crate's `Cargo.toml`.
    Crate,
    /// `[workspace.metadata.coverage-gate]` in the workspace root.
    Workspace,
    /// The built-in default of `100.0`.
    Default,
}

impl ThresholdSource {
    /// Short label used in the verdict table's `Source` column.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "phase 6 renderer will display this")
    )]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Crate => "crate",
            Self::Workspace => "workspace",
            Self::Default => "default",
        }
    }
}

/// A resolved per-crate threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Threshold {
    /// The minimum line-coverage percentage required, in `[0.0, 100.0]`.
    pub(crate) min_lines: f64,
    /// Which configuration layer supplied [`Threshold::min_lines`].
    pub(crate) source: ThresholdSource,
}

impl Threshold {
    /// Resolve the effective threshold for `member` given `workspace`'s
    /// default.
    pub(crate) fn resolve(member: &Member, workspace: &Workspace) -> Self {
        if let Some(v) = member.min_lines {
            Self {
                min_lines: v,
                source: ThresholdSource::Crate,
            }
        } else if let Some(v) = workspace.default_min_lines {
            Self {
                min_lines: v,
                source: ThresholdSource::Workspace,
            }
        } else {
            Self {
                min_lines: DEFAULT_MIN_LINES,
                source: ThresholdSource::Default,
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn member(name: &str, min_lines: Option<f64>) -> Member {
        Member {
            name: name.to_owned(),
            manifest_dir: PathBuf::from(format!("/repo/crates/{name}")),
            min_lines,
        }
    }

    fn workspace(default: Option<f64>) -> Workspace {
        Workspace {
            root: PathBuf::from("/repo"),
            members: Vec::new(),
            default_min_lines: default,
        }
    }

    #[test]
    fn per_crate_wins() {
        let m = member("alpha", Some(82.0));
        let ws = workspace(Some(50.0));
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines - 82.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Crate);
    }

    #[test]
    fn workspace_used_when_no_per_crate() {
        let m = member("alpha", None);
        let ws = workspace(Some(50.0));
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines - 50.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Workspace);
    }

    #[test]
    fn default_used_when_nothing_set() {
        let m = member("alpha", None);
        let ws = workspace(None);
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines - 100.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Default);
        assert!((DEFAULT_MIN_LINES - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn per_crate_zero_is_an_opt_out_not_a_skip() {
        let m = member("alpha", Some(0.0));
        let ws = workspace(Some(50.0));
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines - 0.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Crate);
    }

    #[test]
    fn source_labels() {
        assert_eq!(ThresholdSource::Crate.label(), "crate");
        assert_eq!(ThresholdSource::Workspace.label(), "workspace");
        assert_eq!(ThresholdSource::Default.label(), "default");
    }
}
