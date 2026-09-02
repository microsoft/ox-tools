// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};

use super::cli::MergeArgs;
use super::dispatch::{EXIT_GATE_FAILED, EXIT_OK};
use super::host::Host;
use crate::elements::Report;
use crate::error::error;
use crate::report::{Styler, encode_controls, quantity};

/// The most independently produced reports one merge retains.
///
/// A rotation is normally in the tens or hundreds. This leaves room for a large history while
/// making a directory full of individually valid reports an actionable error rather than an OOM.
const MAX_REPORTS: usize = 4_096;

/// The input bytes whose decoded reports one merge retains.
///
/// String data in a decoded report cannot exceed its JSON representation, so this bounds the
/// untrusted text retained by the report vector in addition to the per-report read cap.
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Implements `merge`.
pub(super) fn merge<H: Host>(host: &mut H, args: &MergeArgs, styler: Styler) -> crate::Result<i32> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|since| since.as_secs());

    merge_at(host, args, styler, now)
}

fn merge_at<H: Host>(host: &mut H, args: &MergeArgs, styler: Styler, now: Option<u64>) -> crate::Result<i32> {
    validate_output_paths(args)?;
    let inputs = collect_reports(&args.inputs)?;

    if inputs.is_empty() {
        return Err(error!("no reports were found in the given paths").usage());
    }

    let window = (args.window > 0).then(|| args.window.saturating_mul(86_400));
    let mut merged = crate::merge::merge(&inputs, now.unwrap_or(0), window);

    if now.is_none() {
        merged.fresh = 0;
        merged.stale = 0;
        merged.freshness_unavailable = true;
    }

    report_merge(host, args, &merged, styler)?;

    if let Some(report) = merged.report.as_ref() {
        if let Some(path) = args.json_report.as_ref() {
            crate::elements::write_json(report, path)?;
            writeln!(host.error(), "{} {}", styler.verb("Wrote"), encode_controls(path.as_str()))?;
        }

        if let Some(path) = args.html_report.as_ref() {
            crate::html::write_page(report, path)?;
            writeln!(host.error(), "{} {}", styler.verb("Wrote"), encode_controls(path.as_str()))?;
        }
    }

    if let Some(minimum) = args.min_score {
        if merged.never_tested > 0 {
            writeln!(
                host.error(),
                "{} {} {} still pending, so the `--min-score` gate cannot evaluate the complete merged population",
                styler.error("error:"),
                merged.never_tested,
                if merged.never_tested == 1 { "mutant is" } else { "mutants are" }
            )?;

            return Ok(EXIT_GATE_FAILED);
        }

        let Some(score) = merged.scored() else {
            // Every mutant was withdrawn, never tested, or otherwise ungradeable, so the merged
            // score is a ratio with nothing in its denominator. That prints as 100%, which is the
            // right answer to "how much of what ran was caught" and the wrong one to hand a
            // threshold, so the gate refuses rather than passing a merge that scored nothing.
            writeln!(
                host.error(),
                "{} no mutant counted toward the merged score, so the `--min-score` gate was never evaluated; \
                 check that the inputs cover a population that was actually run",
                styler.error("error:")
            )?;

            return Ok(EXIT_GATE_FAILED);
        };

        if score < minimum {
            let (shown_score, shown_minimum) = super::run::distinguish(score, minimum);

            writeln!(
                host.error(),
                "{} merged mutation score {shown_score}% is below the required {shown_minimum}%",
                styler.error("error:")
            )?;

            return Ok(EXIT_GATE_FAILED);
        }
    }

    Ok(EXIT_OK)
}

/// Refuses a merged HTML and JSON report that would publish over one another.
fn validate_output_paths(args: &MergeArgs) -> crate::Result<()> {
    let mut outputs = Vec::new();

    if let Some(path) = args.json_report.as_deref() {
        outputs.push(("JSON report", path));
    }

    if let Some(path) = args.html_report.as_deref() {
        outputs.push(("HTML report", path));
    }

    crate::paths::reject_collisions(&outputs)
}

/// Reads every report named, expanding directories to the JSON files they contain.
///
/// Directories are accepted because the natural place to keep a rotation's history is a directory,
/// and requiring a glob would mean the command behaves differently under shells that do not expand
/// one.
fn collect_reports(inputs: &[Utf8PathBuf]) -> crate::Result<Vec<(String, Report)>> {
    collect_reports_limited(inputs, MAX_REPORTS, MAX_TOTAL_BYTES)
}

/// Collects inputs under explicit budgets, so the production limits have cheap boundary tests.
fn collect_reports_limited(inputs: &[Utf8PathBuf], max_reports: usize, max_bytes: u64) -> crate::Result<Vec<(String, Report)>> {
    let mut out = Vec::new();
    let mut retained_bytes = 0;

    for input in inputs {
        if input.is_dir() {
            let entries = fs::read_dir(input).map_err(|cause| error!("could not read `{input}`").caused_by(cause))?;
            let mut paths: Vec<Utf8PathBuf> = Vec::new();

            for entry in entries {
                let entry = entry.map_err(|cause| error!("could not read `{input}`").caused_by(cause))?;
                let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| error!("{} is not a UTF-8 path", path.display()))?;

                if path.extension() == Some("json") {
                    if paths.len() >= max_reports.saturating_sub(out.len()) {
                        return Err(error!(
                            "`{input}` contains more than the {max_reports} reports a merge will retain; split the directory into smaller merges"
                        )
                        .usage());
                    }

                    paths.push(path);
                }
            }

            // Directory order is not defined, and the merge must not depend on it.
            paths.sort();

            for path in paths {
                read_report(&path, max_reports, max_bytes, &mut retained_bytes, &mut out)?;
            }
        } else {
            read_report(input, max_reports, max_bytes, &mut retained_bytes, &mut out)?;
        }
    }

    Ok(out)
}

/// Reads and retains one report after applying the aggregate bounds.
fn read_report(
    path: &Utf8Path,
    max_reports: usize,
    max_bytes: u64,
    retained_bytes: &mut u64,
    out: &mut Vec<(String, Report)>,
) -> crate::Result<()> {
    if out.len() >= max_reports {
        return Err(error!("merge has more than the {max_reports} reports it will retain; split the inputs into smaller merges").usage());
    }

    let remaining = max_bytes.saturating_sub(*retained_bytes);
    let input = crate::merge::read_limited(path, remaining)?;

    *retained_bytes = retained_bytes
        .checked_add(input.bytes)
        .expect("a report read under the remaining budget cannot overflow the aggregate");
    out.push((path.to_string(), input.report));

    Ok(())
}

/// Prints what the merge concluded.
fn report_merge<H: Host>(host: &mut H, args: &MergeArgs, merged: &crate::merge::Merged, styler: Styler) -> crate::Result<()> {
    let mut stream = host.error();

    // "detected" and "not detected" rather than "killed" and "survived", because these are the two
    // halves of the score's fraction and neither is the outcome it would otherwise be named after.
    // Only a failing assertion detects a mutant. The remainder also holds timeouts, memory
    // exhaustion and `NoCoverage`, so naming it "survived" would hide several distinct remedies.
    writeln!(
        stream,
        "{} {} detected, {} not detected, score {}%",
        styler.verb("Merged"),
        merged.detected,
        merged.valid.saturating_sub(merged.detected),
        crate::report::score(merged.score(), merged.detected, merged.valid)
    )?;

    if merged.freshness_unavailable {
        writeln!(
            stream,
            "{} unavailable because the system clock is before the Unix epoch; {} never tested",
            styler.verb("Freshness"),
            merged.never_tested
        )?;
    } else if args.window == 0 {
        writeln!(
            stream,
            "{} window disabled, {} verdicts current, {} never tested",
            styler.verb("Freshness"),
            merged.fresh,
            merged.never_tested
        )?;
    } else {
        writeln!(
            stream,
            "{} {} fresh, {} older than {} days, {} never tested",
            styler.verb("Freshness"),
            merged.fresh,
            merged.stale,
            args.window,
            merged.never_tested
        )?;
    }

    if merged.withdrawn > 0 {
        writeln!(
            stream,
            "{} {} dropped, tested against code that has since changed",
            styler.verb("Withdrawn"),
            merged.withdrawn
        )?;
    }

    if merged.incompatible > 0 {
        writeln!(
            stream,
            "{} {} excluded because their source presentation is incompatible with the selected source",
            styler.verb("Incompatible"),
            quantity(merged.incompatible, "verdict")
        )?;
    }

    if merged.unchecked > 0 {
        writeln!(
            stream,
            "{} withdrawals unchecked for {}: no input supplied a complete population",
            styler.verb("Note"),
            quantity(merged.unchecked, "file")
        )?;
    }

    if let Some(count) = merged.shard_count {
        writeln!(
            stream,
            "{} {} of {count} shards seen, {:.0}% of the rotation",
            styler.verb("Rotation"),
            merged.shards_seen.len(),
            merged.coverage()
        )?;

        let missing = merged.missing_shards();

        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(u32::to_string).collect();

            writeln!(stream, "{} shards never run: {}", styler.verb("Note"), names.join(", "))?;
        }
    }

    // Two runs at different shard counts partitioned the population differently, so the coverage
    // number above is not the claim it appears to be.
    for input in &merged.inconsistent {
        writeln!(
            stream,
            "{} {input} used a different shard count; rotation coverage is not meaningful across it",
            styler.verb("Warning")
        )?;
    }

    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::elements::{FileResult, RunInfo, ShardInfo};
    use crate::fixtures;
    use crate::fixtures::mutant_result_at as mutant;
    use crate::testing::{Sink, fails_at_every_line, workdir};

    fn report(index: u32, count: u32, status: &str) -> Report {
        let mut files = BTreeMap::new();
        let _ = files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "pub fn f() {}\n".to_owned(),
                language: "rust".to_owned(),
                mutants: vec![fixtures::mutant_result_at(
                    &format!("m{index}"),
                    usize::try_from(index + 1).unwrap(),
                    status,
                )],
            },
        );

        Report {
            files,
            config: Some(RunInfo {
                started_at: 100 + u64::from(index),
                merged: false,
                shard: Some(ShardInfo { index, count }),
                tests: None,
                not_built: None,
                dropped_test_packages: Vec::new(),
                merge_provenance: None,
            }),
            ..fixtures::report()
        }
    }

    /// Builds a report with no shard identity at all, the shape a full (non-sharded) run writes.
    /// `populations` only trusts an unsharded report to say what currently exists at a path, so a
    /// withdrawn mutant can only be produced from a pair of these.
    fn unsharded_report(started_at: u64, id: &str, line: usize, status: &str) -> Report {
        let mut files = BTreeMap::new();
        let _ = files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "pub fn f() {}\n".to_owned(),
                language: "rust".to_owned(),
                mutants: vec![mutant(id, line, status)],
            },
        );

        Report {
            files,
            config: Some(RunInfo {
                started_at,
                merged: false,
                shard: None,
                tests: None,
                not_built: None,
                dropped_test_packages: Vec::new(),
                merge_provenance: None,
            }),
            ..fixtures::report()
        }
    }

    fn write_report(path: &Utf8Path, report: &Report) {
        crate::elements::write(&path.to_path_buf(), &crate::elements::to_json(report).expect("json")).expect("write");
    }

    #[test]
    fn directories_are_scanned_and_outputs_are_written() {
        let dir = workdir("merge-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");
        write_report(&input_dir.join("a.json"), &report(0, 3, "Killed"));
        write_report(&input_dir.join("b.json"), &report(1, 4, "Survived"));
        fs::write(input_dir.join("ignored.txt"), "not a report").expect("ignore");

        let args = MergeArgs {
            inputs: vec![input_dir],
            json_report: Some(root.join("out/report.json")),
            html_report: Some(root.join("out/report.html")),
            window: 30,
            min_score: Some(75.0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(root.join("out/report.json").exists());
        assert!(root.join("out/report.html").exists());
        assert!(err.contains("shards never run"), "{err}");
        assert!(err.contains("different shard count"), "{err}");
        assert!(err.contains("below the required"), "{err}");
        assert!(host.out.is_empty());
    }

    #[test]
    fn a_directory_with_too_many_reports_is_refused_before_they_are_retained() {
        let dir = workdir("merge-report-count-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");
        write_report(&input_dir.join("a.json"), &report(0, 2, "Killed"));
        write_report(&input_dir.join("b.json"), &report(1, 2, "Survived"));

        let error = collect_reports_limited(&[input_dir], 1, u64::MAX)
            .expect_err("the second report exceeds the count budget")
            .to_string();

        assert!(error.contains("1 reports"), "{error}");
        assert!(error.contains("split the directory"), "{error}");
    }

    #[test]
    fn aggregate_report_bytes_are_bounded_before_a_report_is_retained() {
        let dir = workdir("merge-total-bytes-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("report.json");
        let report = report(0, 1, "Killed");
        let text = crate::elements::to_json(&report).expect("json");
        let bytes = u64::try_from(text.len()).expect("report text length fits u64");
        fs::write(&input, text).expect("report");

        let error = collect_reports_limited(&[input], 2, bytes - 1)
            .expect_err("the aggregate byte budget is too small")
            .to_string();

        assert!(error.contains(&bytes.to_string()), "{error}");
        assert!(error.contains(&(bytes - 1).to_string()), "{error}");
        assert!(error.contains("merge will retain"), "{error}");
    }

    #[test]
    fn empty_inputs_are_a_usage_error() {
        let dir = workdir("merge-empty-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = MergeArgs {
            inputs: vec![root],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let err = merge(&mut host, &args, Styler::new(false)).unwrap_err();

        assert!(err.is_usage());
    }

    /// A closed diagnostic stream has to surface from every line the merge prints.
    #[test]
    fn a_closed_diagnostic_stream_is_reported_from_any_line() {
        let dir = workdir("merge-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");
        write_report(&input_dir.join("a.json"), &report(0, 3, "Killed"));
        write_report(&input_dir.join("b.json"), &report(1, 4, "Survived"));

        let args = MergeArgs {
            inputs: vec![input_dir],
            json_report: Some(root.join("out/report.json")),
            html_report: Some(root.join("out/report.html")),
            window: 30,
            min_score: Some(75.0),
        };

        fails_at_every_line(8, |host| merge(host, &args, Styler::new(false)).map(|_| ()));
    }

    /// Merging without a gate or any report to write still succeeds and says what it saw.
    #[test]
    fn a_merge_with_no_gate_and_no_outputs_succeeds() {
        let dir = workdir("merge-plain-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &report(0, 1, "Killed"));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: Some(10.0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("Merged"), "{}", host.err());
        assert!(!host.err().contains("shards never run"), "{}", host.err());
    }

    #[test]
    fn zero_window_and_clock_failure_are_reported_truthfully() {
        let dir = workdir("merge-clock-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &report(0, 1, "Killed"));
        let mut args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 0,
            min_score: None,
        };
        let mut host = Sink::default();

        let _code = merge_at(&mut host, &args, Styler::new(false), Some(200)).expect("merge");
        assert!(host.err().contains("window disabled"), "{}", host.err());
        assert!(!host.err().contains("older than 0 days"), "{}", host.err());

        args.window = 30;
        let mut host = Sink::default();
        let _code = merge_at(&mut host, &args, Styler::new(false), None).expect("merge");
        assert!(
            host.err().contains("unavailable because the system clock is before the Unix epoch"),
            "{}",
            host.err()
        );
        assert!(!host.err().contains("1 fresh"), "{}", host.err());
    }

    #[test]
    fn inexact_merged_boundary_scores_do_not_render_as_exact_boundaries() {
        let args = MergeArgs {
            inputs: vec![Utf8PathBuf::from("unused")],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };
        for (detected, expected, forbidden) in [(9_999, "score 99.99%", "score 100.0%"), (1, "score 0.01%", "score 0.0%")] {
            let merged = crate::merge::Merged {
                detected,
                valid: 10_000,
                incompatible: 1,
                ..crate::merge::Merged::default()
            };
            let mut host = Sink::default();

            report_merge(&mut host, &args, &merged, Styler::new(false)).expect("summary");
            assert!(host.err().contains(expected), "{}", host.err());
            assert!(!host.err().contains(forbidden), "{}", host.err());
            assert!(host.err().contains("1 verdict excluded"), "{}", host.err());
        }
    }

    /// The merge headline names the two halves of the fraction, not two of the ten outcomes.
    ///
    /// The remainder of the denominator is every mutant the run judged and nothing noticed, which
    /// includes the uncovered ones — mutants no test ever reached. Calling that figure "survived"
    /// sends the reader looking for a test with a weak assertion when the truth is that no test ran
    /// the line at all, and it does so on the summary line that is read most often and checked
    /// least.
    #[test]
    fn the_merge_headline_does_not_call_uncovered_mutants_survivors() {
        let dir = workdir("merge-headline-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &report(0, 1, "NoCoverage"));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let _code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert!(host.err().contains("0 detected, 1 not detected"), "{}", host.err());
        assert!(
            !host.err().contains("survived"),
            "an uncovered mutant was called a survivor: {}",
            host.err()
        );
    }

    /// A directory that cannot be read names itself rather than failing anonymously.
    #[test]
    fn an_unreadable_input_directory_names_itself() {
        let dir = workdir("merge-unreadable-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let missing = root.join("gone");

        let args = MergeArgs {
            inputs: vec![missing.clone()],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let error = merge(&mut host, &args, Styler::new(false)).expect_err("missing input");

        assert!(error.to_string().contains(missing.as_str()), "{error}");
    }

    /// A mutant whose site was edited between two full runs is dropped from the merge, and the
    /// summary calls out how many were withdrawn so the count is never confused with a mutant that
    /// simply failed to run; without the note, a shrinking denominator would look identical to a
    /// suite that stopped testing something.
    #[test]
    fn a_withdrawn_mutant_is_called_out_by_name() {
        let dir = workdir("merge-withdrawn-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");

        // The old survivor's site no longer exists in the newer run, so it is withdrawn rather than
        // counted as a gap in the current code.
        write_report(&input_dir.join("old.json"), &unsharded_report(100, "aaa", 1, "Survived"));
        write_report(&input_dir.join("new.json"), &unsharded_report(200, "bbb", 1, "Killed"));

        let args = MergeArgs {
            inputs: vec![input_dir],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_OK);
        assert!(
            host.err().contains("1 dropped, tested against code that has since changed"),
            "{}",
            host.err()
        );
    }

    #[test]
    fn a_merged_only_input_says_no_complete_population_was_supplied() {
        let dir = workdir("merge-incomplete-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("merged.json");
        let mut prior = unsharded_report(100, "aaa", 1, "Killed");

        prior.config.as_mut().expect("config").merged = true;
        write_report(&input, &prior);
        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };
        let mut host = Sink::default();

        let _code = merge_at(&mut host, &args, Styler::new(false), Some(200)).expect("merge");
        assert!(host.err().contains("no input supplied a complete population"), "{}", host.err());
    }

    /// A closed stream has to surface from the withdrawn note too, just like every other line the
    /// merge summary writes.
    #[test]
    fn a_closed_stream_is_reported_by_the_withdrawn_note() {
        let dir = workdir("merge-withdrawn-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input_dir = root.join("reports");
        fs::create_dir_all(&input_dir).expect("reports");

        write_report(&input_dir.join("old.json"), &unsharded_report(100, "aaa", 1, "Survived"));
        write_report(&input_dir.join("new.json"), &unsharded_report(200, "bbb", 1, "Killed"));

        let args = MergeArgs {
            inputs: vec![input_dir],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
        };

        // "Merged", "Freshness" and then "Withdrawn" is the third line the summary writes, so
        // closing the stream there is what exercises it.
        fails_at_every_line(3, |host| merge(host, &args, Styler::new(false)).map(|_| ()));
    }

    /// Builds an unsharded report whose single file holds one mutant per status given.
    ///
    /// The near-miss and empty-population gate tests need a population with a chosen detected/valid
    /// split, which the per-shard `report` helper cannot express; an unsharded report states a whole
    /// file's population, so the merge takes the statuses exactly as written.
    fn population(statuses: &[&str]) -> Report {
        let mutants = statuses
            .iter()
            .enumerate()
            .map(|(index, status)| mutant(&format!("m{index}"), index + 1, status))
            .collect();

        let mut files = BTreeMap::new();
        let _ = files.insert(
            "src/lib.rs".to_owned(),
            FileResult {
                source: "pub fn f() {}\n".to_owned(),
                language: "rust".to_owned(),
                mutants,
            },
        );

        Report {
            files,
            config: Some(RunInfo {
                started_at: 100,
                merged: false,
                shard: None,
                tests: None,
                not_built: None,
                dropped_test_packages: Vec::new(),
                merge_provenance: None,
            }),
            ..fixtures::report()
        }
    }

    /// A merge whose population never scored must fail `--min-score`, not pass it.
    ///
    /// Every mutant here was ignored, so the denominator is empty and the printed score is 100% —
    /// the right thing to display for "how much of what ran was caught" and a catastrophe to hand a
    /// threshold, because `--min-score 100` against a merge that scored nothing is a gate that never
    /// ran, not one that passed. The gate routes through `scored`, so it refuses structurally rather
    /// than relying on the placeholder being unflattering, and says why it could not grade.
    #[test]
    fn a_merge_that_scored_nothing_fails_the_min_score_gate() {
        let dir = workdir("merge-ungraded-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &population(&["Ignored", "Ignored"]));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: Some(100.0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
        assert!(host.err().contains("no mutant counted toward the merged score"), "{}", host.err());
    }

    #[test]
    fn a_merge_with_pending_mutants_fails_the_min_score_gate() {
        let dir = workdir("merge-pending-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &population(&["Killed", "Pending"]));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: Some(50.0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
        assert!(host.err().contains("1 mutant is still pending"), "{}", host.err());
    }

    /// The gate message must never print the required score as already met.
    ///
    /// Two killed against one survived is 66.666…%, which fails a 66.7% bar on the full-precision
    /// comparison — the correct verdict — but at one decimal both the score and the threshold read
    /// "66.7%", so the old message said a score of 66.7% was below the required 66.7%. The score now
    /// keeps a second decimal until it is visibly under the bar.
    #[test]
    fn the_merge_gate_message_does_not_print_the_required_score_as_met() {
        let dir = workdir("merge-near-miss-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &population(&["Killed", "Killed", "Survived"]));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: Some(66.7),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");
        let err = host.err();

        assert_eq!(code, EXIT_GATE_FAILED, "{err}");
        assert!(err.contains("66.67% is below the required 66.70%"), "{err}");
        assert!(
            !err.contains("66.7% is below the required 66.7%"),
            "the message denies itself: {err}"
        );
    }

    #[test]
    fn merge_outputs_with_identical_names_are_refused_before_any_report_is_written() {
        let directory = workdir("merge-output-collision-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        let output = root.join("merged");
        let args = MergeArgs {
            inputs: vec![root.join("missing.json")],
            json_report: Some(output.clone()),
            html_report: Some(output),
            window: 30,
            min_score: None,
        };

        let error = merge(&mut Sink::default(), &args, Styler::new(false)).expect_err("colliding outputs are rejected first");

        assert!(error.is_usage(), "{error}");
        assert!(!root.join("merged").exists());
    }

    #[cfg(unix)]
    #[test]
    fn merge_outputs_with_symlink_aliased_names_are_refused_before_any_report_is_written() {
        let directory = workdir("merge-output-symlink-collision-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 path");
        let target = root.join("merged");
        let alias = root.join("alias");

        std::os::unix::fs::symlink(target.as_std_path(), alias.as_std_path()).expect("symlink");
        let args = MergeArgs {
            inputs: vec![root.join("missing.json")],
            json_report: Some(target),
            html_report: Some(alias),
            window: 30,
            min_score: None,
        };

        let _error = merge(&mut Sink::default(), &args, Styler::new(false)).expect_err("symlink-aliased outputs must be refused");
    }
}
