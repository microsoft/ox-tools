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
//! - Codecov ingests lcov uploads directly.
//! - Azure DevOps ingests cobertura XML, which cargo-llvm-cov derives
//!   from lcov internally — same line set.
//! - `cargo llvm-cov report --codecov` emits Codecov's custom JSON,
//!   also derived from lcov.
//!
//! Using the JSON export instead gives stricter "every region on a
//! line must be hit" line semantics — defensible for gating but
//! systematically reports 0.5 to 2 percentage points lower than the
//! other coverage UIs, which makes calibrating thresholds against
//! Codecov / ADO numbers confusing for adopters.
//!
//! [lcov]: https://github.com/linux-test-project/lcov

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
use crate::error::ReadLcovError;
use crate::error::{CoverageGateError, ParseLcovError};

/// Per-file line-coverage view of a parsed lcov tracefile.
#[derive(Debug, Clone)]
pub(crate) struct CoverageReport {
    /// One entry per source file in the tracefile.
    pub(crate) files: Vec<FileReport>,
}

/// Coverage counters for a single source file.
#[derive(Debug, Clone)]
pub(crate) struct FileReport {
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
    #[ohno::enrich_err("failed to parse lcov tracefile")]
    pub(crate) fn from_str(input: &str) -> Result<Self, CoverageGateError> {
        Self::from_strs(std::slice::from_ref(&input))
    }

    /// Parse and merge one or more lcov tracefiles.
    ///
    /// Each input is parsed independently and merged at the line level
    /// (per-line execution counts are summed, so a line is covered if it
    /// was hit in *any* input and the line set is the union across inputs).
    /// This matches `cargo-llvm-cov`'s own profdata merge: feeding the
    /// `--all-features` and `--no-default-features` lcovs here yields the
    /// same per-file line coverage as a single merged report, without
    /// needing a platform-specific lcov merger (`lcov -a` is Linux-only).
    ///
    /// An empty slice yields an empty report (no files); callers that
    /// require at least one input enforce that at the CLI layer.
    ///
    /// # Errors
    ///
    /// Returns [`CoverageGateError`] if any input is not a well-formed
    /// lcov tracefile.
    #[ohno::enrich_err("failed to parse lcov tracefile")]
    pub(crate) fn from_strs(inputs: &[&str]) -> Result<Self, CoverageGateError> {
        let mut merged: Option<lcov::Report> = None;
        for input in inputs {
            let reader = lcov::Reader::new(input.as_bytes());
            let report = lcov::Report::from_reader(reader).map_err(ParseLcovError::from)?;
            match &mut merged {
                None => merged = Some(report),
                // `merge_lossy` sums per-line counts and unions the line
                // sets, ignoring checksum conflicts. The inputs are
                // different feature configs of the *same* sources, so a
                // strict `merge` would only differ by erroring on a
                // checksum mismatch that cannot meaningfully occur here;
                // the lossy variant is the robust choice.
                Some(acc) => acc.merge_lossy(report),
            }
        }
        Ok(Self::from_lcov_report(merged.unwrap_or_default()))
    }

    /// Parse an lcov tracefile from a file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`CoverageGateError`] if the file cannot be read or is
    /// not a well-formed lcov tracefile.
    #[cfg(test)]
    #[ohno::enrich_err("failed to read lcov tracefile `{}`", path.display())]
    pub(crate) fn from_path(path: &Path) -> Result<Self, CoverageGateError> {
        let report = lcov::Report::from_file(path).map_err(|e| ReadLcovError::caused_by(path.display().to_string(), e))?;
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
            files.push(FileReport {
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
    fn from_strs_empty_slice_yields_empty_report() {
        let report = CoverageReport::from_strs(&[]).expect("empty slice parses");
        assert!(report.files.is_empty());
    }

    #[test]
    fn from_strs_merges_line_counts_and_unions_lines() {
        // Two configs of the SAME file: config A covers lines 1,2 (line 3
        // missed); config B covers line 3 (and instruments line 4, which it
        // misses). Merged: union of lines {1,2,3,4}, covered where hit in
        // EITHER input => {1,2,3} covered, 4 missed.
        let config_a = "\
TN:
SF:/repo/crates/alpha/src/lib.rs
DA:1,1
DA:2,3
DA:3,0
end_of_record
";
        let config_b = "\
TN:
SF:/repo/crates/alpha/src/lib.rs
DA:1,0
DA:2,0
DA:3,2
DA:4,0
end_of_record
";
        let report = CoverageReport::from_strs(&[config_a, config_b]).expect("merge parses");
        assert_eq!(report.files.len(), 1, "same file must merge into one entry");
        let f = &report.files[0];
        assert_eq!(f.lines_total, 4, "line set is the union across configs");
        assert_eq!(f.lines_covered, 3, "covered if hit in either config");
    }

    #[test]
    fn from_strs_unions_distinct_files() {
        let file_x = "\
TN:
SF:/repo/crates/x/src/lib.rs
DA:1,1
end_of_record
";
        let file_y = "\
TN:
SF:/repo/crates/y/src/lib.rs
DA:1,1
DA:2,0
end_of_record
";
        let report = CoverageReport::from_strs(&[file_x, file_y]).expect("merge parses");
        // Two inputs naming distinct files merge into two entries.
        assert_eq!(report.files.len(), 2);
    }

    #[test]
    fn from_strs_propagates_parse_error_from_any_input() {
        let err = CoverageReport::from_strs(&[SINGLE_FILE, MALFORMED]).expect_err("a malformed input must fail the merge");
        assert!(err.to_string().contains("lcov tracefile"));
    }

    #[cfg_attr(miri, ignore = "uses real filesystem and open(); miri isolation forbids both")]
    #[test]
    fn from_path_reads_from_disk() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), SINGLE_FILE).expect("write fixture");
        let report = CoverageReport::from_path(tmp.path()).expect("parses from disk");
        assert_eq!(report.files.len(), 1);
    }
}
