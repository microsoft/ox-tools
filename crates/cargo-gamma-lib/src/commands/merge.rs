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

#[cfg_attr(coverage_nightly, coverage(off))]
fn merge_at<H: Host>(host: &mut H, args: &MergeArgs, styler: Styler, now: Option<u64>) -> crate::Result<i32> {
    validate_output_paths(args)?;
    let inputs = collect_reports(&args.inputs)?;
    merge_reports_at(host, args, styler, now, &inputs)
}

fn merge_reports_at<H: Host>(
    host: &mut H,
    args: &MergeArgs,
    styler: Styler,
    now: Option<u64>,
    inputs: &[(String, Report)],
) -> crate::Result<i32> {
    if inputs.is_empty() {
        return Err(error!("no reports were found in the given paths").usage());
    }

    let window = (args.window > 0).then(|| args.window.saturating_mul(86_400));
    let mut merged = crate::merge::merge(
        inputs,
        now.unwrap_or(
            // #[gamma::skip(literal.int_increment, reason = "now.unwrap_or0 is changed only for now=None. merge::union::merge uses now solely to increment fresh versus stale counters; merge_reports_at immediately resets both counters and sets freshness_unavailable for that exact None case. No report population/verdict or other merge field depends on now.", tag = "equivalent")]
            0,
        ),
        window,
    );

    if merged.unpresentable_filtered > 0 {
        return Err(error!(
            "{} name-filtered {} cannot be rendered in the selected source; include an unfiltered listing from the current source before merging so the verdicts are preserved",
            merged.unpresentable_filtered,
            if merged.unpresentable_filtered == 1 { "mutant" } else { "mutants" }
        ));
    }

    if now.is_none() {
        // #[gamma::skip(stmt.delete_assign, reason = "These counter assignments occur only inside now.is_none(), which always sets freshness_unavailable=true. report_merge then takes its unavailable branch and never reads fresh or stale; later gates read score/completeness/flaky fields, and the serialized Report was already built independently of these counters. Deleting these resets or assigning1 cannot affect command bytes, published report, exit code or retained state.", tag = "equivalent")]
        merged.fresh =
        // #[gamma::skip(literal.int_increment, reason = "These counter assignments occur only inside now.is_none(), which always sets freshness_unavailable=true. report_merge then takes its unavailable branch and never reads fresh or stale; later gates read score/completeness/flaky fields, and the serialized Report was already built independently of these counters. Deleting these resets or assigning1 cannot affect command bytes, published report, exit code or retained state.", tag = "equivalent")]
        0;

        // #[gamma::skip(stmt.delete_assign, reason = "These counter assignments occur only inside now.is_none(), which always sets freshness_unavailable=true. report_merge then takes its unavailable branch and never reads fresh or stale; later gates read score/completeness/flaky fields, and the serialized Report was already built independently of these counters. Deleting these resets or assigning1 cannot affect command bytes, published report, exit code or retained state.", tag = "equivalent")]
        merged.stale =
        // #[gamma::skip(literal.int_increment, reason = "These counter assignments occur only inside now.is_none(), which always sets freshness_unavailable=true. report_merge then takes its unavailable branch and never reads fresh or stale; later gates read score/completeness/flaky fields, and the serialized Report was already built independently of these counters. Deleting these resets or assigning1 cannot affect command bytes, published report, exit code or retained state.", tag = "equivalent")]
        0;
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

    let completeness_gate = if args.min_score.is_some() && args.max_flaky.is_some() {
        Some("`--min-score` and `--max-flaky` gates")
    } else if args.min_score.is_some() {
        Some("`--min-score` gate")
    } else if args.max_flaky.is_some() {
        Some("`--max-flaky` gate")
    } else {
        None
    };
    if let Some(gate) = completeness_gate
        && report_incomplete_gate(host, &merged, gate, styler)?
    {
        return Ok(EXIT_GATE_FAILED);
    }

    if let Some(minimum) = args.min_score {
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

    if args.max_flaky.is_some() && merged.scored().is_none() && merged.flaky.is_empty() {
        writeln!(
            host.error(),
            "{} no mutant counted toward the `--max-flaky` gate, so it was never evaluated; \
             check that the inputs cover a population that was actually run",
            styler.error("error:")
        )?;

        return Ok(EXIT_GATE_FAILED);
    }

    if let Some(maximum) = args.max_flaky
        && merged.flaky.len() > maximum
    {
        for finding in &merged.flaky {
            writeln!(
                host.error(),
                "{} {}",
                styler.error("error:"),
                crate::report::encode_controls(finding)
            )?;
        }
        writeln!(
            host.error(),
            "{} {} unresolved flaky {} exceed the configured maximum of {maximum}",
            styler.error("error:"),
            merged.flaky.len(),
            if merged.flaky.len() == 1 { "outcome" } else { "outcomes" }
        )?;

        return Ok(EXIT_GATE_FAILED);
    }

    Ok(EXIT_OK)
}

fn report_incomplete_gate<H: Host>(host: &mut H, merged: &crate::merge::Merged, gate: &str, styler: Styler) -> crate::Result<bool> {
    if !merged.identity_incompatible.is_empty() {
        writeln!(
            host.error(),
            "{} {} {} excluded because of a different mutant-ID scheme, so the {gate} cannot evaluate \
             the complete requested population",
            styler.error("error:"),
            merged.identity_incompatible.len(),
            if merged.identity_incompatible.len() == 1 {
                "report was"
            } else {
                "reports were"
            }
        )?;
        return Ok(true);
    }

    if !merged.inconsistent.is_empty() {
        writeln!(
            host.error(),
            "{} {} {} used a different shard count, so the {gate} cannot evaluate a complete shard rotation",
            styler.error("error:"),
            merged.inconsistent.len(),
            if merged.inconsistent.len() == 1 { "report" } else { "reports" }
        )?;
        return Ok(true);
    }

    let missing = merged.missing_shards();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(u32::to_string).collect();
        writeln!(
            host.error(),
            "{} shards {} were never run, so the {gate} cannot evaluate the complete shard rotation",
            styler.error("error:"),
            names.join(", ")
        )?;
        return Ok(true);
    }

    if merged.never_tested > 0 {
        writeln!(
            host.error(),
            "{} {} {} still pending, so the {gate} cannot evaluate the complete merged population",
            styler.error("error:"),
            merged.never_tested,
            if merged.never_tested == 1 { "mutant is" } else { "mutants are" }
        )?;
        return Ok(true);
    }

    Ok(false)
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
    collect_reports_using(inputs, collect_reports_limited)
}

fn collect_reports_using(
    inputs: &[Utf8PathBuf],
    collect: impl FnOnce(&[Utf8PathBuf], usize, u64) -> crate::Result<Vec<(String, Report)>>,
) -> crate::Result<Vec<(String, Report)>> {
    collect(inputs, MAX_REPORTS, MAX_TOTAL_BYTES)
}

/// Collects inputs under explicit budgets, so the production limits have cheap boundary tests.
fn collect_reports_limited(inputs: &[Utf8PathBuf], max_reports: usize, max_bytes: u64) -> crate::Result<Vec<(String, Report)>> {
    collect_reports_reading(inputs, max_reports, max_bytes, Utf8Path::is_dir, |input| {
        fs::read_dir(input).map(|entries| entries.map(|entry| entry.map(|entry| entry.path())))
    })
}

/// Supplies directory discovery while retaining the native bounded report reader.
fn collect_reports_reading<I: IntoIterator<Item = std::io::Result<std::path::PathBuf>>>(
    inputs: &[Utf8PathBuf],
    max_reports: usize,
    max_bytes: u64,
    is_dir: impl FnMut(&Utf8Path) -> bool,
    read_dir: impl FnMut(&Utf8Path) -> std::io::Result<I>,
) -> crate::Result<Vec<(String, Report)>> {
    collect_reports_with_reader(inputs, max_reports, max_bytes, is_dir, read_dir, read_report)
}

fn collect_reports_with_reader<I: IntoIterator<Item = std::io::Result<std::path::PathBuf>>>(
    inputs: &[Utf8PathBuf],
    max_reports: usize,
    max_bytes: u64,
    mut is_dir: impl FnMut(&Utf8Path) -> bool,
    mut read_dir: impl FnMut(&Utf8Path) -> std::io::Result<I>,
    mut read_report: impl FnMut(&Utf8Path, usize, u64, &mut u64, &mut Vec<(String, Report)>) -> crate::Result<()>,
) -> crate::Result<Vec<(String, Report)>> {
    let mut out = Vec::new();
    let mut retained_bytes = 0;

    for input in inputs {
        if is_dir(input) {
            let entries = read_dir(input).map_err(|cause| error!("could not read `{input}`").caused_by(cause))?;
            let mut paths: Vec<Utf8PathBuf> = Vec::new();

            for entry in entries {
                let entry = entry.map_err(|cause| error!("could not read `{input}`").caused_by(cause))?;
                let path = Utf8PathBuf::from_path_buf(entry).map_err(|path| error!("{} is not a UTF-8 path", path.display()))?;

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
                read_report(
                    &path,
                    // #[gamma::skip(expr.increment, reason = "The changed max_reports argument belongs only to the sorted directory loop. Before any report read, directory enumeration rejects when paths.len() reaches max_reports-out.len(), so the retained prefix plus this directory's entire path count is at most max_reports. Every read in that loop consequently has out.len()<max_reports already; increasing its redundant read_report count cap by1 can neither admit another path nor change an error. Direct-file input count checks are unchanged.", tag = "equivalent")]
                    max_reports,
                    max_bytes,
                    &mut retained_bytes,
                    &mut out,
                )?;
            }
        } else {
            read_report(input, max_reports, max_bytes, &mut retained_bytes, &mut out)?;
        }
    }

    Ok(out)
}

/// Reads and retains one report after applying the aggregate bounds.
#[cfg_attr(coverage_nightly, coverage(off))]
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
#[cfg_attr(coverage_nightly, coverage(off))]
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
        let input = encode_controls(input);

        writeln!(
            stream,
            "{} {input} used a different shard count; rotation coverage is not meaningful across it",
            styler.verb("Warning")
        )?;
    }

    for input in &merged.identity_incompatible {
        let input = encode_controls(input);

        writeln!(
            stream,
            "{} {input} used a different mutant-ID scheme and was excluded from this merge",
            styler.verb("Warning")
        )?;
    }

    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt as _;

    use super::*;
    use crate::elements::{FileResult, RunInfo, ShardInfo};
    use crate::fixtures;
    use crate::fixtures::mutant_result_at as mutant;
    use crate::testing::{Sink, fails_at_every_line, workdir};

    #[test]
    fn oct29_merge_refuses_one_or_many_unpresentable_filtered_verdicts_before_publication() {
        // The unfiltered source wins but cannot present the filtered verdicts.
        // Invalid destinations distinguish that semantic refusal from publication errors.
        for count in [1, 2] {
            let mut filtered = fixtures::report_with(
                None,
                100,
                (0..count)
                    .map(|index| mutant(&format!("filtered-{index}"), index + 1, "Killed"))
                    .collect(),
            );
            filtered.config.as_mut().unwrap().name_filtered = true;
            let mut current = fixtures::report_with(Some((0, 2)), 200, vec![mutant("current", 1, "Survived")]);
            current.files.get_mut("src/lib.rs").unwrap().source = "different source\n".into();
            let inputs = vec![("filtered".into(), filtered), ("current".into(), current)];
            let mut args = survivor_args("unused-in-memory-input".into());
            args.json_report = Some("invalid\0must-not-publish.json".into());
            args.html_report = Some("invalid\0must-not-publish.html".into());
            let mut host = Sink::default();
            let failure = merge_reports_at(&mut host, &args, Styler::new(false), Some(300), &inputs).unwrap_err();
            let noun = if count == 1 { "mutant" } else { "mutants" };
            assert_eq!(
                failure.message(),
                format!(
                    "{count} name-filtered {noun} cannot be rendered in the selected source; include an unfiltered listing from the current source before merging so the verdicts are preserved"
                )
            );
            assert_eq!(host.out(), "");
            assert_eq!(host.err(), "");
        }
    }

    #[test]
    fn recovered_native_merge_returns_the_html_destination_error_before_announcing_a_written_report() {
        let path = Utf8PathBuf::from("invalid\0merge-report.html");
        let expected = crate::paths::physical(&path).unwrap_err().message().to_owned();
        let mut args = survivor_args("not-read-in-memory-input".into());
        args.html_report = Some(path);
        let inputs = vec![("in-memory-report".into(), unsharded_report(100, "only", 1, "Survived"))];
        let mut host = Sink::default();
        assert_eq!(
            merge_reports_at(&mut host, &args, Styler::new(false), Some(101), &inputs)
                .unwrap_err()
                .message(),
            expected
        );
        assert!(!String::from_utf8(host.err).unwrap().contains("Wrote"));
    }

    #[test]
    fn recovered_directory_merge_sorts_reverse_enumeration_before_reporting_the_first_input_error() {
        let first = Utf8PathBuf::from("first.json");
        let later = Utf8PathBuf::from("later.json");
        let mut reads = Vec::new();
        let result = collect_reports_with_reader(
            &[Utf8PathBuf::from("memory-report-directory")],
            2,
            0,
            |path| {
                assert_eq!(path, Utf8Path::new("memory-report-directory"));
                true
            },
            |_path| Ok(vec![Ok(later.clone().into_std_path_buf()), Ok(first.clone().into_std_path_buf())]),
            |path, count, bytes, retained, reports| {
                reads.push(path.to_owned());
                assert_eq!((count, bytes, *retained, reports.len()), (2, 0, 0, 0));
                Err(error!("named first-report refusal").usage())
            },
        );
        let failure = result.unwrap_err();
        assert!(failure.is_usage());
        assert_eq!(failure.message(), "named first-report refusal");
        assert_eq!(reads, [first]);
    }

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
                mutant_id_version: Some(crate::model::MUTANT_ID_VERSION),
                merged: false,
                population_complete: false,
                name_filtered: false,
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
                mutant_id_version: Some(crate::model::MUTANT_ID_VERSION),
                merged: false,
                population_complete: true,
                name_filtered: false,
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
    #[cfg(feature = "system-tests")]
    fn survivor_report_budgets_are_inclusive_cumulative_and_paths_are_sorted() {
        let directory = workdir("merge-survivor-budgets-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let a = root.join("a.json");
        let z = root.join("z.json");
        write_report(&z, &report(1, 2, "Survived"));
        write_report(&a, &report(0, 2, "Killed"));
        fs::write(root.join("ignored.txt"), "not JSON").unwrap();
        let total = fs::metadata(&a).unwrap().len() + fs::metadata(&z).unwrap().len();
        let retained = collect_reports_limited(&[root.to_owned()], 2, total).unwrap();
        assert_eq!(
            retained.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(),
            [a.as_str(), z.as_str()]
        );
        collect_reports_limited(&[root.to_owned()], 2, total - 1).expect_err("cumulative byte budget must reject one extra byte");
        collect_reports_limited(&[a.clone(), z.clone()], 1, total).expect_err("report-count budget must reject a second report");
        collect_reports_limited(&[a, z], 2, total).expect("both inclusive budgets admit exactly two reports");
        let mut bytes = 9;
        let mut out = Vec::new();
        let cause = read_report(&root.join("absent.json"), 0, total, &mut bytes, &mut out).unwrap_err();
        assert!(cause.message().contains("more than the 0 reports"));
        assert_eq!(bytes, 9);
        assert!(out.is_empty());
    }

    #[test]
    fn survivor_merge_window_day_boundaries_and_disabled_clock_have_exact_freshness() {
        let input = Utf8PathBuf::from("synthetic/input.json");
        let inputs = [(input.to_string(), unsharded_report(10, "id", 1, "Killed"))];
        for (window, now, expected) in [
            (1, Some(86_409), "1 fresh, 0 older than 1 days"),
            (1, Some(86_410), "1 fresh, 0 older than 1 days"),
            (1, Some(86_411), "0 fresh, 1 older than 1 days"),
            (0, Some(86_411), "window disabled, 1 verdicts current"),
            (1, None, "unavailable because the system clock is before the Unix epoch"),
        ] {
            let args = MergeArgs {
                inputs: vec![input.clone()],
                json_report: None,
                html_report: None,
                window,
                min_score: None,
                max_flaky: None,
            };
            let mut host = Sink::default();
            assert_eq!(
                merge_reports_at(&mut host, &args, Styler::new(false), now, &inputs).unwrap(),
                EXIT_OK
            );
            let output = host.err();
            assert!(output.contains(expected), "{output}");
            assert!(!output.contains("dropped, tested"));
            assert!(!output.contains("presentation is incompatible"));
            assert!(!output.contains("withdrawals unchecked"));
        }
    }

    #[test]
    fn survivor_incomplete_merge_gate_distinguishes_each_reason_and_propagates_diagnostic_errors() {
        for (reason, expected) in [
            (0, "mutant-ID scheme"),
            (1, "different shard count"),
            (2, "were never run"),
            (3, "still pending"),
        ] {
            let mut merged = crate::merge::Merged::default();
            match reason {
                0 => merged.identity_incompatible.push("other".into()),
                1 => merged.inconsistent.push("other".into()),
                2 => {
                    merged.shard_count = Some(2);
                    let _ = merged.shards_seen.insert(1);
                }
                _ => merged.never_tested = 1,
            }
            let mut host = Sink::default();
            assert!(report_incomplete_gate(&mut host, &merged, "score gate", Styler::new(false)).unwrap());
            assert!(host.err().contains(expected), "{}", host.err());
            crate::testing::fails_at_every_line(1, |host| {
                report_incomplete_gate(host, &merged, "score gate", Styler::new(false)).map(|_| ())
            });
        }
        let mut host = Sink::default();
        assert!(!report_incomplete_gate(&mut host, &crate::merge::Merged::default(), "score gate", Styler::new(false)).unwrap());
        assert!(host.err.is_empty());
    }

    fn survivor_args(input: Utf8PathBuf) -> MergeArgs {
        MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 1,
            min_score: None,
            max_flaky: None,
        }
    }

    #[test]
    fn survivor_merge_collectors_forward_exact_retention_limits_and_propagate_directory_and_entry_errors() {
        let root = Utf8Path::new("synthetic/merge-directory");
        let inputs = [root.to_owned()];
        let reports = collect_reports_using(&inputs, |actual, reports, bytes| {
            assert_eq!(actual, inputs);
            assert_eq!(reports, 4_096);
            assert_eq!(bytes, 512 * 1024 * 1024);
            Ok(Vec::new())
        })
        .unwrap();
        assert!(reports.is_empty());
        let failure = collect_reports_reading::<Vec<std::io::Result<std::path::PathBuf>>>(
            &inputs,
            2,
            100,
            |_| true,
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "directory listing denied",
                ))
            },
        )
        .unwrap_err();
        assert!(failure.to_string().contains("directory listing denied"));
        assert!(failure.message().contains(root.as_str()));
        let failure = collect_reports_reading(
            &inputs,
            2,
            100,
            |_| true,
            |_| {
                Ok(vec![Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "directory entry denied",
                ))])
            },
        )
        .unwrap_err();
        assert!(failure.to_string().contains("directory entry denied"));
        assert!(failure.message().contains(root.as_str()));
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_merge_directory_entries_reject_native_non_utf8_paths_without_panicking() {
        let directory = workdir("merge-native-entry-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        #[cfg(windows)]
        let name = std::ffi::OsString::from_wide(&[0xd800]);
        #[cfg(unix)]
        let name = std::ffi::OsString::from_vec(vec![0xff]);
        #[cfg(any(windows, unix))]
        {
            let path = root.as_std_path().join(name);
            fs::write(&path, "native filename").unwrap();
            let failure = collect_reports_limited(&[root.to_owned()], 2, 100)
                .expect_err("native directory entries that cannot be represented as UTF-8 must return a path error");
            assert!(failure.message().contains("is not a UTF-8 path"));
            assert_eq!(fs::read_to_string(path).unwrap(), "native filename");
        }
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_merge_entrypoint_styles_and_collision_labels_are_preserved() {
        let directory = workdir("merge-styled-entrypoints-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let input = root.join("input.json");
        write_report(&input, &unsharded_report(10, "deadbeefcafe", 1, "Killed"));
        let mut args = survivor_args(input);
        let mut host = Sink::default();
        assert_eq!(merge(&mut host, &args, Styler::new(true)).unwrap(), EXIT_OK);
        assert!(
            host.err().contains("\u{1b}["),
            "both public and fixed-clock entrypoints must retain color"
        );
        let output = root.join("collision.json");
        args.json_report = Some(output.clone());
        args.html_report = Some(output.clone());
        let collision = validate_output_paths(&args).unwrap_err();
        assert!(collision.message().contains("JSON report"));
        assert!(collision.message().contains("HTML report"));
        assert!(!output.exists());
        args.html_report = None;
        fs::create_dir(&output).unwrap();
        merge_at(&mut Sink::default(), &args, Styler::new(false), Some(20))
            .expect_err("a directory blocking JSON output must return its publication error");
        assert!(output.is_dir());
    }

    #[test]
    fn survivor_merge_report_matrix_keeps_all_optional_diagnostics_and_each_writer_error() {
        let mut args = survivor_args("unused.json".into());
        let mut merged = crate::merge::Merged {
            valid: 3,
            detected: 2,
            fresh: 2,
            stale: 1,
            withdrawn: 1,
            incompatible: 1,
            unchecked: 1,
            shard_count: Some(2),
            inconsistent: vec!["inconsistent.json".into()],
            identity_incompatible: vec!["identity.json".into()],
            ..crate::merge::Merged::default()
        };
        let _inserted = merged.shards_seen.insert(0);
        let mut host = Sink::default();
        report_merge(&mut host, &args, &merged, Styler::new(true)).unwrap();
        assert!(host.err().contains("\u{1b}["));
        for required in [
            "1 dropped, tested against code that has since changed",
            "1 verdict excluded because their source presentation is incompatible",
            "withdrawals unchecked for 1 file",
            "identity.json used a different mutant-ID scheme",
        ] {
            assert!(host.err().contains(required), "{}", host.err());
        }
        fails_at_every_line(9, |host| report_merge(host, &args, &merged, Styler::new(false)));
        let mut host = Sink::default();
        assert!(report_incomplete_gate(&mut host, &merged, "score gate", Styler::new(true)).unwrap());
        assert!(host.err().contains("\u{1b}["));
        for (unavailable, window) in [(true, 1), (false, 0)] {
            args.window = window;
            merged.freshness_unavailable = unavailable;
            fails_at_every_line(9, |host| report_merge(host, &args, &merged, Styler::new(false)));
        }
    }

    #[test]
    fn survivor_merge_gate_matrix_checks_only_requested_gates_and_returns_each_output_failure() {
        let input = Utf8PathBuf::from("synthetic/input.json");
        for (status, minimum, flaky, code, message) in [
            ("Pending", None, None, EXIT_OK, None),
            (
                "Pending",
                Some(50.0),
                None,
                EXIT_GATE_FAILED,
                Some("the `--min-score` gate cannot evaluate"),
            ),
            (
                "Pending",
                None,
                Some(0),
                EXIT_GATE_FAILED,
                Some("the `--max-flaky` gate cannot evaluate"),
            ),
            ("CompileError", None, None, EXIT_OK, None),
            (
                "CompileError",
                Some(50.0),
                None,
                EXIT_GATE_FAILED,
                Some("no mutant counted toward the merged score"),
            ),
            (
                "CompileError",
                None,
                Some(0),
                EXIT_GATE_FAILED,
                Some("no mutant counted toward the `--max-flaky` gate"),
            ),
            ("Killed", None, Some(0), EXIT_OK, None),
            (
                "Survived",
                Some(50.0),
                None,
                EXIT_GATE_FAILED,
                Some("merged mutation score 0.0% is below the required 50.0%"),
            ),
        ] {
            let inputs = [(input.to_string(), unsharded_report(10, "deadbeefcafe", 1, status))];
            let mut args = survivor_args(input.clone());
            args.min_score = minimum;
            args.max_flaky = flaky;
            let mut host = Sink::default();
            assert_eq!(
                merge_reports_at(&mut host, &args, Styler::new(false), Some(20), &inputs).unwrap(),
                code,
                "{status}"
            );
            if let Some(message) = message {
                assert!(host.err().contains(message), "{}", host.err());
            }
            fails_at_every_line(host.err().lines().count(), |host| {
                merge_reports_at(host, &args, Styler::new(false), Some(20), &inputs).map(|_| ())
            });
        }
        let mut report = unsharded_report(10, "deadbeefcafe", 1, "Ignored");
        report.files.values_mut().next().unwrap().mutants[0].status_reason = Some("flaky: failed without a mutant".into());
        let inputs = [(input.to_string(), report)];
        let mut args = survivor_args(input);
        args.max_flaky = Some(0);
        let mut host = Sink::default();
        assert_eq!(
            merge_reports_at(&mut host, &args, Styler::new(false), Some(20), &inputs).unwrap(),
            EXIT_GATE_FAILED
        );
        assert!(
            host.err().contains("1 unresolved flaky outcome exceed the configured maximum of 0"),
            "{}",
            host.err()
        );
        fails_at_every_line(host.err().lines().count(), |host| {
            merge_reports_at(host, &args, Styler::new(false), Some(20), &inputs).map(|_| ())
        });
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
            max_flaky: None,
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(root.join("out/report.json").exists());
        assert!(root.join("out/report.html").exists());
        assert!(err.contains("shards never run"), "{err}");
        assert!(err.contains("different shard count"), "{err}");
        assert!(err.contains("cannot evaluate a complete shard rotation"), "{err}");
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
            max_flaky: None,
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
            max_flaky: None,
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
            max_flaky: None,
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("Merged"), "{}", host.err());
        assert!(!host.err().contains("shards never run"), "{}", host.err());
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_score_gate_refuses_an_identity_incompatible_population() {
        let dir = workdir("merge-identity-gate-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let legacy_path = root.join("legacy.json");
        let current_path = root.join("current.json");
        let mut legacy = report(0, 2, "Survived");
        legacy.config.as_mut().expect("config").mutant_id_version = Some(crate::model::MUTANT_ID_VERSION - 1);
        write_report(&legacy_path, &legacy);
        write_report(&current_path, &report(1, 2, "Killed"));
        let args = MergeArgs {
            inputs: vec![legacy_path, current_path],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: Some(100.0),
            max_flaky: None,
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(
            host.err().contains("cannot evaluate the complete requested population"),
            "{}",
            host.err()
        );
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
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
            max_flaky: None,
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
            max_flaky: None,
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

    #[test]
    fn inconsistent_report_paths_are_encoded_before_printing() {
        let args = MergeArgs {
            inputs: Vec::new(),
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
            max_flaky: None,
        };
        let merged = crate::merge::Merged {
            inconsistent: vec!["reports/bad\r\u{1b}[2Kname.json".to_owned()],
            identity_incompatible: vec!["reports/old\nname.json".to_owned()],
            ..crate::merge::Merged::default()
        };
        let mut host = Sink::default();

        report_merge(&mut host, &args, &merged, Styler::new(false)).expect("summary");

        assert!(host.err().contains("bad\\r\\e[2Kname.json"), "{}", host.err());
        assert!(host.err().contains("old\\nname.json"), "{}", host.err());
        assert!(!host.err().contains("\u{1b}[2K"), "{}", host.err());
    }

    /// The merge headline names the two halves of the fraction, not two of the ten outcomes.
    ///
    /// The remainder of the denominator is every mutant the run judged and nothing noticed, which
    /// includes the uncovered ones — mutants no test ever reached. Calling that figure "survived"
    /// sends the reader looking for a test with a weak assertion when the truth is that no test ran
    /// the line at all, and it does so on the summary line that is read most often and checked
    /// least.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
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
            max_flaky: None,
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
            max_flaky: None,
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
    #[cfg_attr(coverage_nightly, coverage(off))]
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
            max_flaky: None,
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
            max_flaky: None,
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
            max_flaky: None,
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
                mutant_id_version: Some(crate::model::MUTANT_ID_VERSION),
                merged: false,
                population_complete: true,
                name_filtered: false,
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
            max_flaky: None,
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
        assert!(host.err().contains("no mutant counted toward the merged score"), "{}", host.err());
    }

    #[test]
    fn a_merge_that_judged_nothing_fails_the_flaky_gate() {
        let dir = workdir("merge-ungraded-flaky-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &population(&["Ignored", "Ignored"]));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
            max_flaky: Some(0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
        assert!(
            host.err().contains("no mutant counted toward the `--max-flaky` gate"),
            "{}",
            host.err()
        );
    }

    #[test]
    fn an_all_flaky_merge_within_budget_passes_the_flaky_gate() {
        let dir = workdir("merge-all-flaky-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        let mut flaky = mutant("flaky", 1, "Ignored");
        flaky.status_reason = Some("flaky: test failed with and without the mutant active".to_owned());
        write_report(&input, &crate::fixtures::report_with(None, 100, vec![flaky]));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
            max_flaky: Some(1),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_OK, "{}", host.err());
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
            max_flaky: None,
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
        assert!(host.err().contains("1 mutant is still pending"), "{}", host.err());
    }

    #[test]
    fn a_merge_with_pending_mutants_fails_a_flaky_only_gate() {
        let dir = workdir("merge-pending-flaky-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        write_report(&input, &population(&["Killed", "Pending"]));

        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: None,
            max_flaky: Some(0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
        assert!(host.err().contains("1 mutant is still pending"), "{}", host.err());
        assert!(host.err().contains("`--max-flaky` gate"), "{}", host.err());
    }

    #[test]
    fn strict_gates_fail_an_incomplete_shard_rotation() {
        for (minimum, maximum, gate) in [
            (Some(0.0), None, "`--min-score` gate"),
            (None, Some(0), "`--max-flaky` gate"),
            (Some(0.0), Some(0), "`--min-score` and `--max-flaky` gates"),
        ] {
            let dir = workdir("merge-incomplete-rotation-");
            let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
            let input = root.join("shard-zero.json");
            write_report(&input, &report(0, 2, "Killed"));
            let args = MergeArgs {
                inputs: vec![input],
                json_report: None,
                html_report: None,
                window: 30,
                min_score: minimum,
                max_flaky: maximum,
            };
            let mut host = Sink::default();

            let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

            assert_eq!(code, EXIT_GATE_FAILED, "{}", host.err());
            assert!(host.err().contains("shards 1 were never run"), "{}", host.err());
            assert!(host.err().contains(gate), "{}", host.err());
        }
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
            max_flaky: None,
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
    fn a_flaky_gate_fails_independently_of_a_perfect_score() {
        let dir = workdir("merge-flaky-gate-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let input = root.join("a.json");
        let mut killed = mutant("killed", 1, "Killed");
        killed.location.start.line = 1;
        let mut flaky = mutant("flaky", 2, "Ignored");
        flaky.status_reason = Some("flaky: test `subject::sometimes` failed with and without the mutant active".to_owned());
        write_report(&input, &crate::fixtures::report_with(None, 100, vec![killed, flaky]));
        let args = MergeArgs {
            inputs: vec![input],
            json_report: None,
            html_report: None,
            window: 30,
            min_score: Some(100.0),
            max_flaky: Some(0),
        };
        let mut host = Sink::default();

        let code = merge(&mut host, &args, Styler::new(false)).expect("merge");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(host.err().contains("flaky"), "{}", host.err());
        assert!(host.err().contains("subject::sometimes"), "{}", host.err());
        assert!(host.err().contains("configured maximum of 0"), "{}", host.err());
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
            max_flaky: None,
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
            max_flaky: None,
        };

        let _error = merge(&mut Sink::default(), &args, Styler::new(false)).expect_err("symlink-aliased outputs must be refused");
    }
}
