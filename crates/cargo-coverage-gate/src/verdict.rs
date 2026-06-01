// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! End-to-end verdict computation.
//!
//! Ties together [`attribute`], [`aggregate`], and [`threshold`] to
//! produce a [`Report`] — one [`CrateOutcome`] per gated package, each
//! classified as [`Status::Ok`], [`Status::Fail`], or
//! [`Status::NoData`] — and a derived [`Verdict`] usable as a process
//! exit code.
//!
//! [`attribute`]: crate::attribute
//! [`aggregate`]: crate::aggregate
//! [`threshold`]: crate::threshold

use crate::Verdict;
use crate::aggregate::{LineTotals, aggregate};
use crate::attribute::{AttributionOutcome, attribute};
use crate::error::CoverageGateError;
use crate::lcov_cov::CoverageReport;
use crate::threshold::Threshold;
use crate::workspace::{Member, Workspace};

/// Tolerance used when comparing measured percentages to thresholds.
///
/// A tight epsilon avoids spurious failures when the stored JSON
/// percentage and the recomputed-from-counters percentage differ at
/// the last representable `f64` digit.
const COMPARE_EPSILON: f64 = 1e-6;

/// Status of a single crate against its threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// Measured percentage met or exceeded the threshold.
    Ok,
    /// Measured percentage fell below the threshold.
    Fail,
    /// No coverage data was attributed to the crate. This is treated as
    /// a configuration error: a package that we asked to gate must have
    /// some test binary contributing data.
    NoData,
}

/// One row of the verdict report.
#[derive(Debug, Clone)]
pub(crate) struct CrateOutcome {
    /// Cargo package name.
    pub(crate) name: String,
    /// Resolved threshold and the layer it came from.
    pub(crate) threshold: Threshold,
    /// Aggregated line counters; may be all-zero when status is `NoData`.
    pub(crate) totals: LineTotals,
    /// Outcome of the comparison.
    pub(crate) status: Status,
}

impl CrateOutcome {
    /// Measured percentage, or `None` when no data was attributed.
    pub(crate) fn percent(&self) -> Option<f64> {
        self.totals.percent()
    }
}

/// Full verdict report — one row per gated package.
#[derive(Debug, Clone)]
pub(crate) struct Report {
    /// One outcome per gated package, in alphabetical order by name.
    pub(crate) outcomes: Vec<CrateOutcome>,
    /// Number of coverage entries that matched no workspace member.
    /// Surfaced as a single aggregated warning rather than per-file.
    pub(crate) unattributed: usize,
}

impl Report {
    /// Roll the per-package outcomes up into an overall [`Verdict`].
    ///
    /// `NoData` dominates `Fail` dominates `Ok`: any `NoData` produces
    /// [`Verdict::ConfigError`]; otherwise any `Fail` produces
    /// [`Verdict::Fail`]; otherwise [`Verdict::Pass`].
    pub(crate) fn verdict(&self) -> Verdict {
        let mut has_fail = false;
        for o in &self.outcomes {
            match o.status {
                Status::NoData => return Verdict::ConfigError,
                Status::Fail => has_fail = true,
                Status::Ok => {}
            }
        }
        if has_fail { Verdict::Fail } else { Verdict::Pass }
    }
}

/// Evaluate a parsed coverage report against the resolved workspace.
///
/// `gated_packages` is the result of applying `--package` to the
/// workspace's member list: when empty, every member is gated. Each
/// entry is a cargo-style package selector — a literal name or a Unix
/// glob pattern (`*` and `?`). A literal that matches no workspace
/// member, or a glob that matches none, produces a [`CoverageGateError`].
pub(crate) fn evaluate(report: &CoverageReport, workspace: &Workspace, gated_packages: &[String]) -> Result<Report, CoverageGateError> {
    let gated = resolve_gated(workspace, gated_packages)?;

    // Flatten every file entry across all data[] elements.
    let files: Vec<_> = report.files.clone();

    let AttributionOutcome { by_member, unattributed } = attribute(&files, &workspace.members);

    if !unattributed.is_empty() {
        eprintln!(
            "warning: {} coverage entries did not match any workspace member; ignoring",
            unattributed.len()
        );
    }

    let mut outcomes: Vec<CrateOutcome> = gated
        .iter()
        .map(|m| {
            let attrib: Vec<&_> = by_member.get(m.name.as_str()).cloned().unwrap_or_default();
            let totals = aggregate(&attrib);
            let threshold = Threshold::resolve(m, workspace);
            let status = classify(totals, threshold);
            CrateOutcome {
                name: m.name.clone(),
                threshold,
                totals,
                status,
            }
        })
        .collect();
    outcomes.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Report {
        outcomes,
        unattributed: unattributed.len(),
    })
}

/// Resolve `packages` (each a cargo-style selector) against the
/// workspace.
///
/// An empty `packages` list selects every member. Otherwise each
/// selector is matched against member names: bare identifiers require
/// exact match, while selectors containing `*` or `?` are matched as
/// Unix shell globs (mirroring `cargo build -p 'tokio-*'`). A selector
/// that matches no member is a configuration error. Members matched by
/// multiple selectors appear only once.
fn resolve_gated<'w>(workspace: &'w Workspace, packages: &[String]) -> Result<Vec<&'w Member>, CoverageGateError> {
    if packages.is_empty() {
        return Ok(workspace.members.iter().collect());
    }
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(packages.len());
    for spec in packages {
        let matches: Vec<&Member> = if is_glob(spec) {
            workspace.members.iter().filter(|m| glob_matches(spec, &m.name)).collect()
        } else {
            workspace.members.iter().filter(|m| m.name == *spec).collect()
        };
        if matches.is_empty() {
            return Err(CoverageGateError::new(format!(
                "`--package` selector `{spec}` did not match any workspace member"
            )));
        }
        for m in matches {
            if seen.insert(m.name.as_str()) {
                out.push(m);
            }
        }
    }
    Ok(out)
}

/// True when `spec` contains any cargo-style glob metacharacter.
fn is_glob(spec: &str) -> bool {
    spec.contains('*') || spec.contains('?')
}

/// Tiny Unix-style glob matcher: `*` matches any run of characters
/// (including empty), `?` matches exactly one character. Everything
/// else matches literally. No character classes, no escapes — package
/// names are simple identifiers, so this is sufficient.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob_inner(&p, 0, &n, 0)
}

fn glob_inner(p: &[char], mut pi: usize, n: &[char], mut ni: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' => {
                // Collapse runs of `*` and try every possible match
                // length for the next literal segment.
                while pi < p.len() && p[pi] == '*' {
                    pi += 1;
                }
                if pi == p.len() {
                    return true;
                }
                for k in ni..=n.len() {
                    if glob_inner(p, pi, n, k) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ni >= n.len() {
                    return false;
                }
                pi += 1;
                ni += 1;
            }
            c => {
                if ni >= n.len() || n[ni] != c {
                    return false;
                }
                pi += 1;
                ni += 1;
            }
        }
    }
    ni == n.len()
}

/// Compare `totals` against `threshold` and classify the outcome.
fn classify(totals: LineTotals, threshold: Threshold) -> Status {
    // A zero threshold is the explicit opt-out documented in the
    // design: the crate passes regardless of how much (or whether
    // any) coverage data was attributed to it. Check this before the
    // no-data path so that opting a package out doesn't turn its
    // missing data into a configuration error.
    if threshold.min_lines_percent <= COMPARE_EPSILON {
        return Status::Ok;
    }
    let Some(pct) = totals.percent() else {
        return Status::NoData;
    };
    if pct + COMPARE_EPSILON >= threshold.min_lines_percent {
        Status::Ok
    } else {
        Status::Fail
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::lcov_cov::FileEntry;
    use crate::threshold::ThresholdSource;

    fn make_file(path: &str, count: u32, covered: u32) -> FileEntry {
        FileEntry {
            filename: PathBuf::from(path),
            lines_total: count,
            lines_covered: covered,
        }
    }

    fn make_member(name: &str, manifest_dir: &str, min_lines_percent: Option<f64>) -> Member {
        Member {
            name: name.to_owned(),
            manifest_dir: PathBuf::from(manifest_dir),
            min_lines_percent,
        }
    }

    fn make_report(files: Vec<FileEntry>) -> CoverageReport {
        CoverageReport { files }
    }

    fn make_workspace(members: Vec<Member>, default: Option<f64>) -> Workspace {
        Workspace {
            members,
            default_min_lines_percent: default,
        }
    }

    #[test]
    fn all_pass() {
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 100, 95),
            make_file("/repo/crates/beta/src/lib.rs", 50, 50),
        ]);
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", Some(90.0)),
                make_member("beta", "/repo/crates/beta", Some(80.0)),
            ],
            None,
        );
        let r = evaluate(&report, &ws, &[]).expect("evaluate");
        assert_eq!(r.verdict(), Verdict::Pass);
        assert!(r.outcomes.iter().all(|o| o.status == Status::Ok));
    }

    #[test]
    fn one_failure_produces_fail_verdict() {
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 100, 95),
            make_file("/repo/crates/beta/src/lib.rs", 100, 60),
        ]);
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", Some(90.0)),
                make_member("beta", "/repo/crates/beta", Some(80.0)),
            ],
            None,
        );
        let r = evaluate(&report, &ws, &[]).expect("evaluate");
        assert_eq!(r.verdict(), Verdict::Fail);
        let beta = r.outcomes.iter().find(|o| o.name == "beta").unwrap();
        assert_eq!(beta.status, Status::Fail);
        assert!((beta.percent().unwrap() - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn no_data_dominates_fail() {
        let report = make_report(vec![make_file("/repo/crates/alpha/src/lib.rs", 100, 60)]);
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", Some(80.0)),
                // `beta` is gated but has no data attributed.
                make_member("beta", "/repo/crates/beta", Some(80.0)),
            ],
            None,
        );
        let r = evaluate(&report, &ws, &[]).expect("evaluate");
        assert_eq!(r.verdict(), Verdict::ConfigError);
        let beta = r.outcomes.iter().find(|o| o.name == "beta").unwrap();
        assert_eq!(beta.status, Status::NoData);
    }

    #[test]
    fn crates_flag_restricts_scope() {
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 100, 95),
            make_file("/repo/crates/beta/src/lib.rs", 100, 50),
        ]);
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", Some(90.0)),
                make_member("beta", "/repo/crates/beta", Some(80.0)),
            ],
            None,
        );
        // Only gate alpha; beta would have failed but is out of scope.
        let r = evaluate(&report, &ws, &["alpha".to_owned()]).expect("evaluate");
        assert_eq!(r.verdict(), Verdict::Pass);
        assert_eq!(r.outcomes.len(), 1);
        assert_eq!(r.outcomes[0].name, "alpha");
    }

    #[test]
    fn crates_flag_with_unknown_name_errors() {
        let ws = make_workspace(vec![make_member("alpha", "/repo/crates/alpha", None)], None);
        let report = make_report(Vec::new());
        let err = evaluate(&report, &ws, &["typo".to_owned()]).expect_err("unknown package must error");
        let rendered = err.to_string();
        assert!(rendered.contains("typo"));
        assert!(rendered.contains("--package"));
    }

    #[test]
    fn glob_selector_matches_multiple_members() {
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", None),
                make_member("alpha_macros", "/repo/crates/alpha_macros", None),
                make_member("beta", "/repo/crates/beta", None),
            ],
            None,
        );
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 10, 10),
            make_file("/repo/crates/alpha_macros/src/lib.rs", 10, 10),
        ]);
        let r = evaluate(&report, &ws, &["alpha*".to_owned()]).expect("evaluate");
        let names: Vec<_> = r.outcomes.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "alpha_macros"]);
    }

    #[test]
    fn glob_matching_no_member_errors() {
        let ws = make_workspace(vec![make_member("alpha", "/repo/crates/alpha", None)], None);
        let report = make_report(Vec::new());
        let err = evaluate(&report, &ws, &["beta*".to_owned()]).expect_err("no match must error");
        assert!(err.to_string().contains("beta*"));
    }

    #[test]
    fn overlapping_selectors_dedupe_members() {
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", None),
                make_member("alpha_macros", "/repo/crates/alpha_macros", None),
            ],
            None,
        );
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 10, 10),
            make_file("/repo/crates/alpha_macros/src/lib.rs", 10, 10),
        ]);
        let r = evaluate(
            &report,
            &ws,
            &["alpha".to_owned(), "alpha*".to_owned()],
        )
        .expect("evaluate");
        let names: Vec<_> = r.outcomes.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "alpha_macros"]);
    }

    #[test]
    fn glob_matcher_handles_wildcards() {
        assert!(super::glob_matches("alpha*", "alpha"));
        assert!(super::glob_matches("alpha*", "alpha_macros"));
        assert!(super::glob_matches("*macros", "alpha_macros"));
        assert!(super::glob_matches("*alpha*", "my_alpha_lib"));
        assert!(super::glob_matches("a?pha", "alpha"));
        assert!(!super::glob_matches("alpha", "alphax"));
        assert!(!super::glob_matches("alpha*", "beta"));
        assert!(!super::glob_matches("a?pha", "axxpha"));
        // Multiple consecutive `*` collapse.
        assert!(super::glob_matches("a**b", "ab"));
        assert!(super::glob_matches("a**b", "axyzb"));
    }

    #[test]
    fn unattributed_files_are_counted_and_dropped() {
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 100, 80),
            make_file("/elsewhere/build-script.rs", 50, 0),
        ]);
        let ws = make_workspace(vec![make_member("alpha", "/repo/crates/alpha", Some(70.0))], None);
        let r = evaluate(&report, &ws, &[]).expect("evaluate");
        assert_eq!(r.unattributed, 1);
        let alpha = &r.outcomes[0];
        assert_eq!(alpha.totals.count, 100);
        assert_eq!(alpha.status, Status::Ok);
    }

    #[test]
    fn threshold_source_propagated_through_outcome() {
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", Some(50.0)),
                make_member("beta", "/repo/crates/beta", None),
                make_member("gamma", "/repo/crates/gamma", None),
            ],
            Some(80.0),
        );
        let report = make_report(vec![
            make_file("/repo/crates/alpha/src/lib.rs", 10, 10),
            make_file("/repo/crates/beta/src/lib.rs", 10, 10),
            make_file("/repo/crates/gamma/src/lib.rs", 10, 10),
        ]);
        let r = evaluate(&report, &ws, &[]).expect("evaluate");
        let by_name: std::collections::HashMap<_, _> = r.outcomes.iter().map(|o| (o.name.as_str(), o)).collect();
        assert_eq!(by_name["alpha"].threshold.source, ThresholdSource::Package);
        assert_eq!(by_name["beta"].threshold.source, ThresholdSource::Workspace);
        // gamma also inherits from workspace, not default, because the
        // workspace default is set.
        assert_eq!(by_name["gamma"].threshold.source, ThresholdSource::Workspace);
    }

    #[test]
    fn epsilon_protects_against_floating_point_equality() {
        // 82.0 = 82.0 must not fail due to f64 representation jitter.
        let totals = LineTotals { count: 100, covered: 82 };
        let threshold = Threshold {
            min_lines_percent: 82.0,
            source: ThresholdSource::Default,
        };
        assert_eq!(classify(totals, threshold), Status::Ok);
    }

    #[test]
    fn zero_threshold_opts_out_even_with_no_data() {
        // `min-lines-percent = 0.0` is the documented opt-out; a package with no
        // attributed coverage data must still pass rather than be
        // flagged as a configuration error.
        let totals = LineTotals { count: 0, covered: 0 };
        let threshold = Threshold {
            min_lines_percent: 0.0,
            source: ThresholdSource::Package,
        };
        assert_eq!(classify(totals, threshold), Status::Ok);
    }

    #[test]
    fn zero_threshold_opts_out_even_when_well_covered() {
        let totals = LineTotals { count: 100, covered: 100 };
        let threshold = Threshold {
            min_lines_percent: 0.0,
            source: ThresholdSource::Package,
        };
        assert_eq!(classify(totals, threshold), Status::Ok);
    }

    #[test]
    fn opt_out_crate_does_not_force_config_error_verdict() {
        // End-to-end: an opt-out crate with no JSON data must not push
        // the overall verdict to ConfigError.
        let report = make_report(vec![make_file("/repo/crates/alpha/src/lib.rs", 100, 95)]);
        let ws = make_workspace(
            vec![
                make_member("alpha", "/repo/crates/alpha", Some(80.0)),
                // `beta` is opted out and has no attributed data.
                make_member("beta", "/repo/crates/beta", Some(0.0)),
            ],
            None,
        );
        let r = evaluate(&report, &ws, &[]).expect("evaluate");
        assert_eq!(r.verdict(), Verdict::Pass);
        let beta = r.outcomes.iter().find(|o| o.name == "beta").unwrap();
        assert_eq!(beta.status, Status::Ok);
    }
}
