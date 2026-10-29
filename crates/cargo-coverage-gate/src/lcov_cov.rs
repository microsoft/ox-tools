// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! lcov tracefile parser.
//!
//! Reads an [lcov tracefile][lcov] (derived from the format
//! `cargo llvm-cov --lcov` emits) and produces a per-file view of line coverage
//! that the rest of the gate consumes. On the released-surface path,
//! [`crate::source_filter`] filters raw reports first. The
//! `evaluate_unfiltered*` entry points parse caller-supplied reports as-is, so
//! this parser assumes nothing about filtering.
//!
//! ## Why lcov, not the JSON
//!
//! `cargo-llvm-cov` exports the same instrumentation run in several
//! formats. We pick lcov because it matches what every other coverage
//! UI consuming this data sees:
//!
//! - Codecov ingests lcov uploads directly.
//! - Azure DevOps ingests lcov directly.
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

use std::collections::BTreeMap;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
use crate::error::ReadLcovError;
use crate::error::{CoverageGateError, MergedSourceChecksumError, ParseLcovError};

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
    /// Instrumented source line numbers in ascending order.
    pub(crate) coverable_lines: Vec<u32>,
    /// Instrumented source line numbers with no hits, in ascending order.
    pub(crate) uncovered_lines: Vec<u32>,
}

#[derive(Debug)]
struct MergedLineCoverage {
    covered: bool,
    checksum: Option<String>,
}

type LineCoverage = BTreeMap<PathBuf, BTreeMap<u32, MergedLineCoverage>>;

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
    /// Each input is parsed independently and merged at the line level. The
    /// line set is the union across inputs, and a line is covered if it was hit
    /// in any input.
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
        let mut merged = LineCoverage::new();
        for input in inputs {
            let reader = lcov::Reader::new(input.as_bytes());
            let report = lcov::Report::from_reader(reader).map_err(ParseLcovError::from)?;
            merge_line_coverage(&mut merged, report)?;
        }
        Ok(Self::from_line_coverage(merged))
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
        let mut merged = LineCoverage::new();
        merge_line_coverage(&mut merged, report)?;
        Ok(Self::from_line_coverage(merged))
    }

    fn from_line_coverage(merged: LineCoverage) -> Self {
        let mut files = Vec::with_capacity(merged.len());
        for (filename, lines) in merged {
            let mut total: u32 = 0;
            let mut covered: u32 = 0;
            let mut coverable_lines = Vec::with_capacity(lines.len());
            let mut uncovered_lines = Vec::new();
            for (line, data) in lines {
                total = total.saturating_add(1);
                coverable_lines.push(line);
                if data.covered {
                    covered = covered.saturating_add(1);
                } else {
                    uncovered_lines.push(line);
                }
            }
            files.push(FileReport {
                filename,
                lines_total: total,
                lines_covered: covered,
                coverable_lines,
                uncovered_lines,
            });
        }
        Self { files }
    }
}

fn merge_line_coverage(merged: &mut LineCoverage, report: lcov::Report) -> Result<(), CoverageGateError> {
    for (section_key, section) in report.sections {
        let source_file = section_key.source_file;
        let lines = merged.entry(source_file.clone()).or_default();
        for (line_key, data) in section.lines {
            let is_covered = data.count > 0;
            match lines.entry(line_key.line) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let _ = entry.insert(MergedLineCoverage {
                        covered: is_covered,
                        checksum: data.checksum,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let existing = entry.get_mut();
                    if existing
                        .checksum
                        .as_ref()
                        .zip(data.checksum.as_ref())
                        .is_some_and(|(left, right)| left != right)
                    {
                        return Err(MergedSourceChecksumError::new(
                            crate::package_glob::diagnostic(&source_file.display().to_string()),
                            line_key.line,
                        )
                        .into());
                    }
                    if data.checksum.is_some() {
                        existing.checksum = data.checksum;
                    }
                    existing.covered |= is_covered;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn survivor_merging_an_absent_checksum_cannot_erase_an_earlier_checksum() {
        let mut merged = LineCoverage::new();
        for suffix in [",retained", ""] {
            let report = lcov::Report::from_reader(lcov::Reader::new(
                format!("TN:\nSF:source.rs\nDA:1,1{suffix}\nend_of_record\n").as_bytes(),
            ))
            .unwrap();
            merge_line_coverage(&mut merged, report).unwrap();
        }
        let source = &merged[Path::new("source.rs")];
        assert_eq!(source[&1].checksum.as_deref(), Some("retained"));
        assert!(source[&1].covered);
    }
    const SINGLE_FILE: &str = include_str!("../tests/fixtures/lcov/single_file.info");
    const MULTI_FILE: &str = include_str!("../tests/fixtures/lcov/multi_file.info");
    const EMPTY: &str = include_str!("../tests/fixtures/lcov/empty.info");
    const MALFORMED: &str = include_str!("../tests/fixtures/lcov/malformed.info");

    fn merged_checksum_error(error: &CoverageGateError) -> &MergedSourceChecksumError {
        std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |source| source.source())
            .find_map(|source| source.downcast_ref::<MergedSourceChecksumError>())
            .expect("merged checksum error cause")
    }

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
        assert_eq!(f.coverable_lines, vec![1, 2, 3, 4]);
        assert_eq!(f.uncovered_lines, vec![3]);
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
    fn from_strs_unions_line_coverage() {
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
        assert_eq!(f.coverable_lines, vec![1, 2, 3, 4]);
        assert_eq!(f.uncovered_lines, vec![4]);
    }

    #[test]
    fn named_test_sections_merge_by_source_path_and_line() {
        let input = "\
TN:first
SF:/repo/crates/alpha/src/lib.rs
DA:1,1
DA:2,0
end_of_record
TN:second
SF:/repo/crates/alpha/src/lib.rs
DA:1,0
DA:2,1
end_of_record
";
        let report = CoverageReport::from_str(input).expect("named sections parse");
        assert_eq!(report.files.len(), 1, "test names must not create duplicate source files");
        let file = &report.files[0];
        assert_eq!(file.lines_total, 2);
        assert_eq!(file.lines_covered, 2, "a hit in either named section covers the source line");
        assert!(file.uncovered_lines.is_empty());
    }

    #[test]
    fn named_test_sections_reject_conflicting_checksums() {
        let input = "\
TN:first
SF:/repo/crates/alpha/src/lib.rs
DA:1,1,first
end_of_record
TN:second
SF:/repo/crates/alpha/src/lib.rs
DA:1,0,second
end_of_record
";
        let error = CoverageReport::from_str(input).expect_err("named sections from different source text must not merge");
        let checksum = merged_checksum_error(&error);
        assert_eq!(checksum.path, "/repo/crates/alpha/src/lib.rs");
        assert_eq!(checksum.line, 1);
        assert!(error.to_string().contains("conflicting checksums for line 1"), "{error}");
    }

    #[test]
    fn separate_reports_reject_conflicting_checksums() {
        let first = "TN:\nSF:/repo/crates/alpha/src/lib.rs\nDA:1,1,first\nend_of_record\n";
        let second = "TN:\nSF:/repo/crates/alpha/src/lib.rs\nDA:1,0,second\nend_of_record\n";
        let error = CoverageReport::from_strs(&[first, second]).expect_err("reports from different source text must not merge");
        let checksum = merged_checksum_error(&error);
        assert_eq!(checksum.path, "/repo/crates/alpha/src/lib.rs");
        assert_eq!(checksum.line, 1);
        assert!(error.to_string().contains("conflicting checksums for line 1"), "{error}");
    }

    #[test]
    fn checksum_added_by_a_later_report_is_retained_for_conflict_detection() {
        let without_checksum = "TN:\nSF:/repo/crates/alpha/src/lib.rs\nDA:1,0\nend_of_record\n";
        let first_checksum = "TN:\nSF:/repo/crates/alpha/src/lib.rs\nDA:1,1,first\nend_of_record\n";
        let conflicting = "TN:\nSF:/repo/crates/alpha/src/lib.rs\nDA:1,0,second\nend_of_record\n";
        let error = CoverageReport::from_strs(&[without_checksum, first_checksum, conflicting])
            .expect_err("a later checksum must participate in subsequent conflict checks");
        let checksum = merged_checksum_error(&error);
        assert_eq!(checksum.path, "/repo/crates/alpha/src/lib.rs");
        assert_eq!(checksum.line, 1);
        assert!(error.to_string().contains("conflicting checksums for line 1"), "{error}");
    }

    #[test]
    fn from_strs_does_not_overflow_execution_counts() {
        let config_a = "\
TN:
SF:/repo/crates/alpha/src/lib.rs
DA:1,18446744073709551615
DA:2,0
end_of_record
";
        let config_b = "\
TN:
SF:/repo/crates/alpha/src/lib.rs
DA:1,1
DA:2,0
end_of_record
";
        let report = CoverageReport::from_strs(&[config_a, config_b]).expect("merge parses");
        let f = &report.files[0];
        assert_eq!(f.lines_total, 2);
        assert_eq!(f.lines_covered, 1, "positive counts remain covered without arithmetic");
        assert_eq!(f.uncovered_lines, vec![2]);
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
        assert_eq!(
            report
                .files
                .iter()
                .map(|file| file.filename.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["/repo/crates/x/src/lib.rs", "/repo/crates/y/src/lib.rs"]
        );
    }

    #[test]
    fn files_are_sorted_by_path_not_test_name() {
        let input = "\
TN:z-test
SF:/repo/crates/a/src/lib.rs
DA:9,0
DA:2,1
end_of_record
TN:a-test
SF:/repo/crates/z/src/lib.rs
DA:8,0
DA:1,1
end_of_record
";
        let report = CoverageReport::from_str(input).expect("parses");
        assert_eq!(report.files[0].filename, PathBuf::from("/repo/crates/a/src/lib.rs"));
        assert_eq!(report.files[0].coverable_lines, [2, 9]);
        assert_eq!(report.files[0].uncovered_lines, [9]);
        assert_eq!(report.files[1].filename, PathBuf::from("/repo/crates/z/src/lib.rs"));
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
