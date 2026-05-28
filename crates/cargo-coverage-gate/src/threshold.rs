// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Three-layer threshold resolution.
//!
//! Each workspace member's effective `min-lines` value is the first match
//! among:
//!
//! 1. [`Member::min_lines`] — per-package `[package.metadata.coverage-gate]`.
//! 2. [`Workspace::DEFAULT_MIN_LINES_PERCENT`] — workspace-level
//!    `[workspace.metadata.coverage-gate]`.
//! 3. [`DEFAULT_MIN_LINES_PERCENT`] — the built-in default of `100.0` (full
//!    coverage required).
//!
//! The resolved [`Threshold`] carries both the value and a
//! [`ThresholdSource`] tag so the verdict table can report which layer
//! supplied the number.

use crate::workspace::{Member, Workspace};

/// The built-in `min-lines-percent` value used when neither per-package
/// nor workspace metadata supplies a value.
pub(crate) const DEFAULT_MIN_LINES_PERCENT: f64 = 100.0;

/// Which layer of the resolution stack a threshold came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThresholdSource {
    /// `[package.metadata.coverage-gate]` in the package's `Cargo.toml`.
    Package,
    /// `[workspace.metadata.coverage-gate]` in the workspace root.
    Workspace,
    /// The built-in default of `100.0`.
    Default,
}

impl ThresholdSource {
    /// Short label used in the verdict table's `Source` column.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Workspace => "workspace",
            Self::Default => "default",
        }
    }
}

/// A resolved per-package threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Threshold {
    /// The minimum line-coverage percentage required, in `[0.0, 100.0]`.
    pub(crate) min_lines_percent: f64,
    /// Which configuration layer supplied [`Threshold::min_lines`].
    pub(crate) source: ThresholdSource,
}

impl Threshold {
    /// Resolve the effective threshold for `member` given `workspace`'s
    /// default.
    pub(crate) fn resolve(member: &Member, workspace: &Workspace) -> Self {
        if let Some(v) = member.min_lines_percent {
            Self {
                min_lines_percent: v,
                source: ThresholdSource::Package,
            }
        } else if let Some(v) = workspace.default_min_lines_percent {
            Self {
                min_lines_percent: v,
                source: ThresholdSource::Workspace,
            }
        } else {
            Self {
                min_lines_percent: DEFAULT_MIN_LINES_PERCENT,
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

    fn member(name: &str, min_lines_percent: Option<f64>) -> Member {
        Member {
            name: name.to_owned(),
            manifest_dir: PathBuf::from(format!("/repo/crates/{name}")),
            min_lines_percent,
        }
    }

    fn workspace(default: Option<f64>) -> Workspace {
        Workspace {
            members: Vec::new(),
            default_min_lines_percent: default,
        }
    }

    #[test]
    fn per_crate_wins() {
        let m = member("alpha", Some(82.0));
        let ws = workspace(Some(50.0));
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines_percent - 82.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Package);
    }

    #[test]
    fn workspace_used_when_no_per_crate() {
        let m = member("alpha", None);
        let ws = workspace(Some(50.0));
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines_percent - 50.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Workspace);
    }

    #[test]
    fn default_used_when_nothing_set() {
        let m = member("alpha", None);
        let ws = workspace(None);
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines_percent - 100.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Default);
        assert!((DEFAULT_MIN_LINES_PERCENT - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn per_crate_zero_is_an_opt_out_not_a_skip() {
        let m = member("alpha", Some(0.0));
        let ws = workspace(Some(50.0));
        let t = Threshold::resolve(&m, &ws);
        assert!((t.min_lines_percent - 0.0).abs() < f64::EPSILON);
        assert_eq!(t.source, ThresholdSource::Package);
    }

    #[test]
    fn source_labels() {
        assert_eq!(ThresholdSource::Package.label(), "package");
        assert_eq!(ThresholdSource::Workspace.label(), "workspace");
        assert_eq!(ThresholdSource::Default.label(), "default");
    }
}
