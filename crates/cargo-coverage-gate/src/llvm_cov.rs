// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `cargo-llvm-cov` JSON v2 schema deserialization.
//!
//! Only the subset of fields the gate actually consumes is modeled:
//! file paths and per-file line counters. The top-level `totals` block
//! and the `functions` / `regions` summaries are accepted by the
//! deserializer but discarded — the tool computes its own per-crate
//! aggregates so that gating stays consistent with whichever subset of
//! files is in scope.
//!
//! ## Version tolerance
//!
//! The top-level `version` field is treated as advisory. A missing or
//! unparseable version emits a warning via `tracing` and continues; a
//! version whose major component is not `2` also warns but continues.
//! Only structural parse failures are hard errors. This keeps the tool
//! usable across cargo-llvm-cov / llvm-tools upgrades that nudge the
//! string without changing the structure we depend on.

use std::path::PathBuf;

use serde::Deserialize;
use tracing::warn;

use crate::error::CoverageGateError;

/// The supported major version of the LLVM coverage JSON export schema.
const SUPPORTED_MAJOR: &str = "2";

/// Classification of the report's `version` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionStatus {
    /// `version` is present and its major component matches
    /// [`SUPPORTED_MAJOR`].
    Supported,
    /// `version` is missing entirely.
    Missing,
    /// `version` is present but its major component is not
    /// [`SUPPORTED_MAJOR`].
    Unsupported,
}

/// A parsed `cargo-llvm-cov` JSON report.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CoverageReport {
    /// Optional schema version string from the top of the document.
    #[serde(default)]
    pub(crate) version: Option<String>,
    /// One or more export blobs. cargo-llvm-cov emits a single element
    /// in practice but the schema is a list.
    pub(crate) data: Vec<Export>,
}

/// One export blob inside the report.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Export {
    /// Per-file coverage entries.
    pub(crate) files: Vec<FileEntry>,
}

/// Coverage data for a single source file.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileEntry {
    /// Absolute path to the source file as recorded by cargo-llvm-cov.
    pub(crate) filename: PathBuf,
    /// The per-summary counter block.
    pub(crate) summary: SummaryBlock,
}

/// The `summary` block on a file entry.
///
/// `cargo-llvm-cov` also emits `functions` and `regions` siblings; both
/// are accepted at the JSON level but ignored at the type level so we
/// don't fail when their shape shifts between toolchain releases.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SummaryBlock {
    /// Line counters — the only block we actually consume.
    pub(crate) lines: LineCounters,
}

/// Line-level coverage counters.
#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct LineCounters {
    /// Total number of executable lines.
    pub(crate) count: u64,
    /// Number of executable lines that ran at least once.
    pub(crate) covered: u64,
}

impl CoverageReport {
    /// Parse a `cargo-llvm-cov` JSON v2 report from a string.
    ///
    /// Emits warnings via `tracing` for missing or unsupported `version`
    /// values but returns `Ok` as long as the structural shape parses.
    pub(crate) fn from_str(json: &str) -> Result<Self, CoverageGateError> {
        let report: Self = serde_json::from_str(json)
            .map_err(|source| CoverageGateError::caused_by("failed to parse coverage JSON".to_owned(), source))?;
        match report.version_status() {
            VersionStatus::Supported => {}
            VersionStatus::Missing => {
                warn!("coverage JSON has no `version` field; assuming v2 schema");
            }
            VersionStatus::Unsupported => {
                let v = report.version.as_deref().unwrap_or("?");
                warn!(
                    "coverage JSON has unsupported version `{v}`; \
                     continuing on the assumption that v2 structure still applies"
                );
            }
        }
        Ok(report)
    }

    /// Classify the report's top-level `version` field.
    pub(crate) fn version_status(&self) -> VersionStatus {
        match self.version.as_deref() {
            None => VersionStatus::Missing,
            Some(v) => {
                let major = v.split('.').next().unwrap_or("");
                if major == SUPPORTED_MAJOR {
                    VersionStatus::Supported
                } else {
                    VersionStatus::Unsupported
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const EMPTY: &str = include_str!("../tests/fixtures/llvm_cov/empty.json");
    const SINGLE_FILE: &str = include_str!("../tests/fixtures/llvm_cov/single_file.json");
    const MULTI_FILE: &str = include_str!("../tests/fixtures/llvm_cov/multi_file.json");
    const NO_VERSION: &str = include_str!("../tests/fixtures/llvm_cov/no_version.json");
    const UNKNOWN_VERSION: &str = include_str!("../tests/fixtures/llvm_cov/unknown_version.json");
    const MALFORMED: &str = include_str!("../tests/fixtures/llvm_cov/malformed.json");
    const MISSING_REQUIRED: &str = include_str!("../tests/fixtures/llvm_cov/missing_required.json");

    #[test]
    fn parses_empty_data() {
        let report = CoverageReport::from_str(EMPTY).expect("empty fixture is well-formed");
        assert_eq!(report.version.as_deref(), Some("2.0.1"));
        assert!(report.data.is_empty());
    }

    #[test]
    fn parses_single_file() {
        let report = CoverageReport::from_str(SINGLE_FILE).expect("single_file is well-formed");
        let files = &report.data[0].files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].summary.lines.count, 100);
        assert_eq!(files[0].summary.lines.covered, 82);
        assert!(files[0].filename.to_string_lossy().ends_with("crates/alpha/src/lib.rs"));
    }

    #[test]
    fn parses_multi_file() {
        let report = CoverageReport::from_str(MULTI_FILE).expect("multi_file is well-formed");
        let files = &report.data[0].files;
        assert_eq!(files.len(), 4);
        // Sum lines across files.
        let total_lines: u64 = files.iter().map(|f| f.summary.lines.count).sum();
        let total_covered: u64 = files.iter().map(|f| f.summary.lines.covered).sum();
        assert_eq!(total_lines, 100 + 40 + 50 + 80);
        assert_eq!(total_covered, 95 + 30 + 25 + 80);
    }

    #[test]
    fn missing_version_is_tolerated() {
        let report = CoverageReport::from_str(NO_VERSION).expect("no_version is structurally well-formed");
        assert!(report.version.is_none());
        assert_eq!(report.version_status(), VersionStatus::Missing);
        assert_eq!(report.data[0].files.len(), 1);
    }

    #[test]
    fn unknown_version_is_tolerated() {
        let report = CoverageReport::from_str(UNKNOWN_VERSION).expect("unknown_version is structurally well-formed");
        assert_eq!(report.version.as_deref(), Some("3.0.0"));
        assert_eq!(report.version_status(), VersionStatus::Unsupported);
        assert_eq!(report.data[0].files.len(), 1);
    }

    #[test]
    fn supported_version_is_recognised() {
        let report = CoverageReport::from_str(SINGLE_FILE).expect("single_file is well-formed");
        assert_eq!(report.version_status(), VersionStatus::Supported);
    }

    #[test]
    fn malformed_json_is_rejected() {
        let err = CoverageReport::from_str(MALFORMED).expect_err("garbage should fail to parse");
        assert!(err.to_string().contains("coverage JSON"));
    }

    #[test]
    fn structurally_invalid_json_is_rejected() {
        let err = CoverageReport::from_str(MISSING_REQUIRED).expect_err("entry without filename / summary should fail to parse");
        assert!(err.to_string().contains("coverage JSON"));
    }
}
