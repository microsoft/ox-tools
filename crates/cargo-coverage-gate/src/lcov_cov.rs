// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! lcov tracefile parser.
//!
//! Reads an [lcov tracefile][lcov] (the format `cargo llvm-cov --lcov`
//! emits) and produces a per-file view of line coverage that the rest
//! of the gate consumes.
//!
//! ## Why lcov, not the JSON
//!
//! `cargo-llvm-cov` exports the same instrumentation run in several
//! formats. We pick lcov because it matches what every other coverage
//! UI consuming this data sees:
//!
//! - Codecov.io ingests lcov uploads directly.
//! - Azure DevOps ingests cobertura XML, which cargo-llvm-cov derives
//!   from lcov internally — same line set.
//! - `cargo llvm-cov report --codecov` emits Codecov's custom JSON,
//!   also derived from lcov.
//!
//! Using the JSON export instead gives stricter "every region on a
//! line must be hit" line semantics — defensible for gating but
//! systematically reports 0.5–2pp lower than the other UIs, which
//! makes calibrating thresholds against codecov / ADO numbers
//! confusing for adopters.
//!
//! [lcov]: https://github.com/linux-test-project/lcov

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use crate::error::CoverageGateError;

/// Per-file line-coverage view of a parsed lcov tracefile.
#[derive(Debug, Clone)]
pub(crate) struct CoverageReport {
    /// One entry per source file in the tracefile.
    pub(crate) files: Vec<FileEntry>,
}

/// Coverage counters for a single source file.
#[derive(Debug, Clone)]
pub(crate) struct FileEntry {
    /// Absolute path to the source file as recorded by lcov.
    pub(crate) filename: PathBuf,
    /// Number of distinct executable lines instrumented in the file.
    pub(crate) lines_total: u32,
    /// Number of those lines hit at least once across the run.
    pub(crate) lines_covered: u32,
}

impl CoverageReport {
    /// Parse an lcov tracefile from a string.
    ///
    /// # Errors
    ///
    /// Returns [`CoverageGateError`] if the input is not a well-formed
    /// lcov tracefile.
    pub(crate) fn from_str(input: &str) -> Result<Self, CoverageGateError> {
        let reader = lcov::Reader::new(input.as_bytes());
        let report =
            lcov::Report::from_reader(reader).map_err(|e| CoverageGateError::caused_by("failed to parse lcov tracefile".to_owned(), e))?;
        Ok(Self::from_lcov_report(report))
    }

    /// Parse an lcov tracefile from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`CoverageGateError`] if the file cannot be read or is
    /// not a well-formed lcov tracefile.
    #[cfg(test)]
    pub(crate) fn from_path(path: &Path) -> Result<Self, CoverageGateError> {
        let report = lcov::Report::from_file(path)
            .map_err(|e| CoverageGateError::caused_by(format!("failed to read lcov tracefile `{}`", path.display()), e))?;
        Ok(Self::from_lcov_report(report))
    }

    fn from_lcov_report(report: lcov::Report) -> Self {
        let mut files = Vec::with_capacity(report.sections.len());
        for (key, section) in report.sections {
            let mut total: u32 = 0;
            let mut covered: u32 = 0;
            for data in section.lines.values() {
                total = total.saturating_add(1);
                if data.count > 0 {
                    covered = covered.saturating_add(1);
                }
            }
            files.push(FileEntry {
                filename: key.source_file,
                lines_total: total,
                lines_covered: covered,
            });
        }
        Self { files }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    const SINGLE_FILE: &str = include_str!("../tests/fixtures/lcov/single_file.info");
    const MULTI_FILE: &str = include_str!("../tests/fixtures/lcov/multi_file.info");
    const EMPTY: &str = include_str!("../tests/fixtures/lcov/empty.info");
    const MALFORMED: &str = include_str!("../tests/fixtures/lcov/malformed.info");

    #[test]
    fn parses_empty_tracefile() {
        let report = CoverageReport::from_str(EMPTY).expect("empty fixture parses");
        assert!(report.files.is_empty());
    }

    #[test]
    fn parses_single_file() {
        let report = CoverageReport::from_str(SINGLE_FILE).expect("single file parses");
        assert_eq!(report.files.len(), 1);
        let f = &report.files[0];
        assert!(f.filename.to_string_lossy().ends_with("crates/alpha/src/lib.rs"));
        // Fixture has 4 instrumented lines, 3 hit.
        assert_eq!(f.lines_total, 4);
        assert_eq!(f.lines_covered, 3);
    }

    #[test]
    fn parses_multi_file() {
        let report = CoverageReport::from_str(MULTI_FILE).expect("multi-file parses");
        assert_eq!(report.files.len(), 4);
        let total_lines: u32 = report.files.iter().map(|f| f.lines_total).sum();
        let total_covered: u32 = report.files.iter().map(|f| f.lines_covered).sum();
        // Sum of fixture: 100 + 40 + 50 + 80 = 270, hit 95 + 30 + 25 + 80 = 230.
        assert_eq!(total_lines, 100 + 40 + 50 + 80);
        assert_eq!(total_covered, 95 + 30 + 25 + 80);
    }

    #[test]
    fn lenient_line_semantics_match_lcov() {
        // A line with two regions, one hit and one missed, is "covered"
        // in lcov / our parser. This is the lenient semantics that codecov
        // and ADO use.
        let input = "\
TN:
SF:/repo/crates/alpha/src/lib.rs
DA:1,1
DA:2,0
DA:3,5
end_of_record
";
        let report = CoverageReport::from_str(input).expect("parses");
        assert_eq!(report.files[0].lines_total, 3);
        assert_eq!(report.files[0].lines_covered, 2);
    }

    #[test]
    fn malformed_is_rejected() {
        let err = CoverageReport::from_str(MALFORMED).expect_err("garbage should fail to parse");
        assert!(err.to_string().contains("lcov tracefile"));
    }

    #[test]
    fn from_path_reads_from_disk() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), SINGLE_FILE).expect("write fixture");
        let report = CoverageReport::from_path(tmp.path()).expect("parses from disk");
        assert_eq!(report.files.len(), 1);
    }
}
