// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs;
use std::io::{ErrorKind, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};

use super::cli::RunArgs;
use super::console_events::ConsoleEvents;
use super::dashboard::Dashboard;
use super::dispatch::{DEFAULT_TEST_TIMEOUT_MULTIPLIER, EXIT_CANNOT_PROCEED, EXIT_GATE_FAILED, EXIT_OK};
use super::host::Host;
use super::verdict_log::VerdictLog;
use super::when::When;
use crate::config::Config;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::exec::{self, RecordPreparation};
use crate::model::{Mutant, Outcome};
use crate::report::{Listings, Progress, Stats, Styler, encode_controls, quantity};
#[cfg(any(test, feature = "internals"))]
use crate::testing::pause_after_cache_adoption;

const GITHUB_ACTIONS: &str = "GITHUB_ACTIONS";

#[cfg(feature = "internals")]
#[doc(hidden)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub mod recovery_test_util;

#[cfg(test)]
fn missing_hints(mode: exec::IncrementalMode, root: &Utf8Path) -> bool {
    mode.is_enabled() && crate::discover::Hints::is_missing(root)
}

/// Which of the bulk outcome listings the caller asked for.
const fn listings(args: &RunArgs, announced: bool, promotable: Option<bool>, suppressible: Option<bool>) -> Listings {
    Listings {
        killed: args.show_killed,
        unviable: args.show_unviable,
        announced,
        promotable,
        suppressible,
        stats: None,
    }
}

fn stats(session: &exec::Session, elapsed: Duration) -> Stats {
    let sweep = session.phases.sweep.as_ref();

    Stats {
        elapsed,
        binaries: session.binaries.len(),
        launches: sweep.map_or(0, |cost| cost.launches),
        successful_hints: sweep.map_or(0, |cost| cost.exact_hits + cost.inferred_hits),
    }
}

/// Loads `gamma.toml` and folds it into `args`.
pub(super) fn configure<H: Host>(host: &mut H, args: &mut RunArgs, styler: Styler) -> crate::Result<()> {
    if args.resume_testing {
        Config::resolve_reporting_gates(&args.select)?.apply_reporting_gates(args);
        return Ok(());
    }
    // Said before anything is loaded, because the settings in that file are about to not happen and
    // the run would otherwise look like it honoured them.
    if Config::foreign_present(&args.select.dir) && !args.select.config.no_config && args.select.config.path.is_none() {
        let hint = styler.note("Hint");

        writeln!(
            host.error(),
            "{hint} .cargo/mutants.toml is not supported or read; configure gamma.toml explicitly"
        )?;
    }

    Config::resolve(&args.select)?.apply(args)?;

    Ok(())
}

/// Works out how much memory control the run should place around each test binary.
///
/// The two size flags imply a mode, because asking for a specific ceiling and then being told
/// nothing was enforced would be a surprising way to learn that a separate switch existed. Naming
/// `--memory` explicitly still wins, so a configuration file that turns metering on can be turned
/// back off for one run.
///
/// Whether any of that was said out loud is recorded rather than discarded. Enforcement is the
/// default, and a host that cannot deliver it gets a note and an unbounded run — but a user who
/// named a memory setting gets an error instead, because they asked for a guarantee and silently
/// not having it is the one outcome that could cost them the machine.
pub(super) fn memory_policy(args: &RunArgs) -> exec::MemoryPolicy {
    let implied = exec::implied_memory_control(args.measure.memory_limit, args.measure.baseline_memory_limit);
    let stated = args.measure.memory.or(implied);
    let demand = if stated.is_some() {
        exec::Demand::Stated
    } else {
        exec::Demand::Inherited
    };

    exec::MemoryPolicy {
        control: stated.unwrap_or_default(),
        demand,
        multiplier: args.measure.memory_multiplier.unwrap_or(exec::DEFAULT_MULTIPLIER),
        headroom: args.measure.memory_headroom.unwrap_or(exec::DEFAULT_HEADROOM),
        limit: args.measure.memory_limit,
        baseline_limit: args.measure.baseline_memory_limit,
    }
}

/// Records what `merge` needs to know about this run.
///
/// The shard identity travels in the report rather than in the filename, because a filename is a
/// convention and this has to survive being copied into an artifact bucket by someone who does not
/// know the convention.
fn run_info(args: &RunArgs, tests: Option<usize>, dropped: &[String]) -> crate::elements::RunInfo {
    run_info_at(args, tests, dropped, SystemTime::now())
}

fn run_info_at(args: &RunArgs, tests: Option<usize>, dropped: &[String], now: SystemTime) -> crate::elements::RunInfo {
    crate::elements::RunInfo {
        tests,
        started_at: seconds_since_epoch(now),
        mutant_id_version: Some(crate::model::MUTANT_ID_VERSION),
        merged: false,
        population_complete: args.select.in_diff.is_none()
            && args.select.mutators.is_none()
            && args.select.shard_count.is_none()
            && args.mutants.is_empty()
            && !args.only_survivors,
        shard: args
            .select
            .shard_count
            .zip(args.select.shard_index)
            .map(|(count, index)| crate::elements::ShardInfo { index, count }),
        // Filled in by `build` from the plan it is given, so that it cannot disagree with the
        // mutants in the same report.
        not_built: None,
        dropped_test_packages: dropped.to_vec(),
        merge_provenance: None,
    }
}

fn report_info(
    args: &RunArgs,
    retained: Option<&crate::elements::RunInfo>,
    tests: Option<usize>,
    dropped: &[String],
) -> crate::elements::RunInfo {
    let mut info = retained.cloned().unwrap_or_else(|| run_info(args, tests, dropped));
    info.tests = tests;
    info.dropped_test_packages = dropped.to_vec();
    info
}

fn resumed_report_info(retained: Option<&crate::elements::RunInfo>) -> crate::elements::RunInfo {
    retained.cloned().unwrap_or_else(|| crate::elements::RunInfo {
        started_at: 0,
        mutant_id_version: Some(crate::model::MUTANT_ID_VERSION),
        merged: true,
        population_complete: false,
        shard: None,
        tests: None,
        not_built: None,
        dropped_test_packages: Vec::new(),
        merge_provenance: None,
    })
}

fn seconds_since_epoch(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(Duration::ZERO.as_secs(), |since| since.as_secs())
}

/// Where a run's documents go, once the defaults and any overrides have been settled.
///
/// Every completed run writes all five — HTML, JSON and SARIF reports, Markdown advice and a diagnostics bundle.
/// The reason they are produced by default rather than on request is
/// that the flags were a trap: a run that takes an hour and answers a question about your test
/// suite is exactly the run you do not want to repeat because you forgot to ask for the artifact
/// that answers it. The cost of writing them is a few hundred milliseconds against that hour, and
/// they land under `target` where nothing is precious.
///
/// Normal runs publish under the original workspace's `target/cargo-gamma`; `--artifact-dir`
/// redirects the complete set without changing where reusable cache state lives.
struct Documents {
    /// Where the self-contained HTML report goes.
    html: Utf8PathBuf,

    /// Where the `mutation-testing-elements` JSON report goes.
    json: Utf8PathBuf,

    /// Where the Markdown diagnosis goes.
    advice: Utf8PathBuf,

    /// Where the diagnostics bundle goes.
    diag: Utf8PathBuf,

    /// Where the SARIF report goes.
    sarif: Utf8PathBuf,
}

impl Documents {
    fn directory(args: &RunArgs, root: &Utf8Path) -> Utf8PathBuf {
        args.artifact_dir.clone().unwrap_or_else(|| root.join("target/cargo-gamma"))
    }

    /// The fixed artifact names under the configured directory.
    fn resolve(args: &RunArgs, root: &Utf8Path) -> Self {
        let base = Self::directory(args, root);

        Self {
            html: base.join("gamma-report.html"),
            json: base.join("gamma-report.json"),
            advice: base.join("gamma-perf-advice.md"),
            diag: base.join("gamma-diagnostics.json"),
            sarif: base.join("gamma-report.sarif"),
        }
    }
}

/// Writes the file reports, and says where they went.
///
/// The path is echoed because a report written to a path nobody looks at is the same as no report,
/// and in CI the message is often the only trace that the artifact exists.
fn emit_reports<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    advice: Option<&str>,
    tests: Option<usize>,
    dropped: &[String],
    styler: Styler,
) -> crate::Result<()> {
    emit_reports_from(
        host,
        args,
        plan,
        &ReportContents {
            retained_info: None,
            source_root: None,
            advice,
            tests,
            dropped,
        },
        styler,
    )
}

struct ReportContents<'a> {
    retained_info: Option<&'a crate::elements::RunInfo>,
    source_root: Option<&'a Utf8Path>,
    advice: Option<&'a str>,
    tests: Option<usize>,
    dropped: &'a [String],
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn emit_reports_from<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    contents: &ReportContents<'_>,
    styler: Styler,
) -> crate::Result<()> {
    let documents = Documents::resolve(args, &plan.root);
    let report = crate::elements::build_from(
        plan,
        contents.source_root,
        crate::elements::Thresholds::default(),
        Some(report_info(args, contents.retained_info, contents.tests, contents.dropped)),
    )?;
    let mut stream = host.error();

    crate::elements::write_json(&report, &documents.json)?;
    writeln!(
        stream,
        "{} {}",
        styler.footer_wrote(),
        encode_controls(&platform_path(&documents.json))
    )?;

    crate::html::write_page(&report, &documents.html)?;
    writeln!(
        stream,
        "{} {}",
        styler.footer_wrote(),
        encode_controls(&platform_path(&documents.html))
    )?;
    drop(stream);

    emit_ci(host, args, plan, contents.advice, styler)?;

    Ok(())
}

/// Writes the SARIF log and, when requested, the diff annotations and job summary.
#[cfg_attr(coverage_nightly, coverage(off))]
fn emit_ci<H: Host>(host: &mut H, args: &RunArgs, plan: &Plan, advice: Option<&str>, styler: Styler) -> crate::Result<()> {
    let path = Documents::resolve(args, &plan.root).sarif;
    let (log, truncation) = crate::ci::sarif(&plan.mutants, &plan.root, args.sarif_level)?;

    crate::elements::write(&path, &log)?;

    let mut stream = host.error();

    writeln!(stream, "{} {}", styler.footer_wrote(), encode_controls(&platform_path(&path)))?;

    if let Some(truncation) = truncation {
        // Saying so is the whole difference between a report that is smaller than the truth and
        // a report that is quietly wrong.
        writeln!(
            stream,
            "{} {} of {} findings written; SARIF consumers reject a larger log outright",
            styler.warning(),
            truncation.written,
            truncation.found
        )?;
    }

    drop(stream);

    if !crate::ci::wanted(args.annotations, host.env(GITHUB_ACTIONS).is_some()) {
        return Ok(());
    }

    for line in crate::ci::annotations(&plan.mutants, &plan.root) {
        writeln!(host.results(), "{line}")?;
    }

    if let Some(path) = host.env("GITHUB_STEP_SUMMARY") {
        // The diagnosis rides along with the score rather than in a file of its own: the summary
        // panel is the artifact a team reads every morning, and a score nobody knows what to do
        // about is the reason mutation testing gets run nightly and then ignored.
        let mut summary = crate::ci::summary(&plan.mutants, &plan.root);

        if let Some(advice) = advice {
            summary.push('\n');
            summary.push_str(advice);
        }

        let written = crate::ci::append(Utf8Path::new(&path), &summary);

        if let Err(cause) = written {
            let _ = writeln!(
                host.error(),
                "{} could not write the job summary to `{path}`: {cause}",
                styler.warning()
            );
        }
    }

    Ok(())
}

/// Collects the mutants whose `expect_survived` or `expect_killed` directive did not hold.
///
/// An expectation is a claim about the suite that the author asked to be held to. Parsing it and
/// then ignoring it is worse than not supporting it at all: the directive reads as a guarantee, and
/// the thing it guards can rot indefinitely without anyone hearing about it.
///
/// Only mutants that actually ran are judged. One that failed to compile or was never reached is
/// not evidence either way, and failing a run over it would make the check depend on whether the
/// build happened to produce that mutant at all.
fn broken_expectations(mutants: &[Mutant]) -> Vec<(&Mutant, &'static str)> {
    let mut broken = Vec::new();

    for mutant in mutants {
        let Some(expectation) = &mutant.expectation else {
            continue;
        };

        if !mutant.outcome.is_valid() {
            continue;
        }

        let detected = mutant.outcome.is_detected();

        if detected != expectation.killed {
            broken.push((mutant, if expectation.killed { "killed" } else { "survived" }));
        }
    }

    broken
}

/// Fails a run whose gate never got a population to judge.
///
/// A gate that cannot fail is worse than no gate. Every route to an empty population — an exclude
/// pattern that matched everything, a shard that held nothing but suppressions, a diff that named
/// no code, an incremental run that had already settled the lot — would otherwise end in a
/// summary that said nothing was tested and an exit code that said everything was fine. A job that
/// asked for `--min-score 100` or `--max-flaky 0` would then pass on the strength of having tested
/// nothing at all, indefinitely, which is the exact failure these flags exist to prevent.
///
/// A run that asked for no gate is left alone: an empty population is a perfectly ordinary answer
/// to a narrow selection, and turning it into a failure would break every run that never made a
/// claim about its score.
#[cfg_attr(coverage_nightly, coverage(off))]
fn ungraded<H: Host>(host: &mut H, args: &RunArgs, styler: Styler, flaky_outcomes: bool, expectations: bool) -> i32 {
    let score_unevaluated = args.min_score.is_some();
    let flaky_unevaluated = args.max_flaky.is_some() && !flaky_outcomes;
    let gate = if score_unevaluated && flaky_unevaluated {
        "the `--min-score` and `--max-flaky` gates"
    } else if score_unevaluated {
        "the `--min-score` gate"
    } else if flaky_unevaluated {
        "the `--max-flaky` gate"
    } else if expectations {
        "the expectations this run carried"
    } else {
        return EXIT_OK;
    };

    let _ = writeln!(
        host.error(),
        "{} no mutant counted toward {gate}, so it was never evaluated; \
         check that the selection — `--in-diff`, `--exclude-file`, `--shard-count`/`--shard-index`, `--incremental` — leaves something to test",
        styler.error("error:")
    );

    EXIT_GATE_FAILED
}

/// Renders a failing score and its threshold at just enough precision to tell them apart.
///
/// The gate compares the full-precision `f64`s — gating on the printed value instead would be a real
/// defect, since a score of 79.96% has not met an 80% bar — but at one decimal place that same near
/// miss renders as "80.0% is below the required 80.0%", a sentence that denies itself. The two are
/// shown at the coarsest precision that still distinguishes them, so the common case stays at one
/// decimal and only a genuine near miss grows extra digits. The caller guarantees `score < minimum`,
/// so the two are distinct reals and the search always terminates at a precision that separates them.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn distinguish(score: f64, minimum: f64) -> (String, String) {
    for precision in 1..=12 {
        let shown_score = format!("{score:.precision$}");
        let shown_minimum = format!("{minimum:.precision$}");

        if shown_score != shown_minimum {
            return (shown_score, shown_minimum);
        }
    }

    (format!("{score}"), format!("{minimum}"))
}

// #[gamma::skip(all, reason = "exit-code gating is covered end-to-end through the injected Host; private branch mutants duplicate the directly tested gate predicates and rendered diagnostics")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn run_session<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<i32> {
    #[cfg(target_os = "linux")]
    if args.resume_testing
        && let Some(code) = resume_relaunch(host, args)?
    {
        return Ok(code);
    }
    let Executed { plan, stuck } = measured(host, args, progress_when, styler)?;

    // A build the tool could not make compile is not a run that passed, whatever the mutants it did
    // get to say. It is reported first and it wins: the population the gate would judge is missing
    // a part nobody measured, so a score computed over what is left is not the score the gate was
    // written for, and letting it decide the exit code would turn a run that got stuck into a green
    // tick. The report has already been written by this point, so nothing is lost by refusing.
    if !stuck.is_empty() {
        return Ok(EXIT_CANNOT_PROCEED);
    }

    let Some(plan) = plan else {
        // Nothing was generated at all, so no mutant carries an expectation either: whether a gate
        // exists is entirely what the command line asked for.
        return Ok(ungraded(host, args, styler, false, false));
    };

    let summary = crate::model::Summary::of(&plan.mutants);
    let broken = broken_expectations(&plan.mutants);

    if !broken.is_empty() {
        let mut stream = host.error();

        for (mutant, wanted) in &broken {
            let line = mutant.expectation.as_ref().map_or(mutant.line, |expectation| expectation.line);
            let was = mutant.outcome;
            let _ = writeln!(
                stream,
                "{} {}:{line}: `{}` expected {wanted}, but this mutant was {was}",
                styler.error("error:"),
                mutant.file,
                mutant.mutator
            );
        }

        let count = broken.len();
        let _ = writeln!(
            stream,
            "{} {count} {} not hold",
            styler.error("error:"),
            if count == 1 { "expectation did" } else { "expectations did" }
        );

        return Ok(EXIT_GATE_FAILED);
    }

    if pending_fails_gate(args, summary.pending) {
        let gate = if args.min_score.is_some() && args.max_flaky.is_some() {
            "`--min-score` and `--max-flaky` gates"
        } else if args.min_score.is_some() {
            "`--min-score` gate"
        } else {
            "`--max-flaky` gate"
        };
        let _ = writeln!(
            host.error(),
            "{} {} {} still pending, so the {gate} cannot evaluate the complete population",
            styler.error("error:"),
            summary.pending,
            if summary.pending == 1 { "mutant is" } else { "mutants are" }
        );

        return Ok(EXIT_GATE_FAILED);
    }

    if let Some(maximum) = flaky_excess(args, summary.flaky) {
        let mut stream = host.error();

        for mutant in plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Flaky) {
            let reason = mutant.note.as_deref().unwrap_or("a test failed with and without the mutant active");
            let _ = writeln!(stream, "{} {} [{}]: {reason}", styler.error("error:"), mutant.describe(), mutant.id);
        }

        let _ = writeln!(
            stream,
            "{} {} unresolved flaky {} exceed the configured maximum of {maximum}",
            styler.error("error:"),
            summary.flaky,
            if summary.flaky == 1 { "outcome" } else { "outcomes" }
        );

        return Ok(EXIT_GATE_FAILED);
    }

    // Every mutant was suppressed, unviable, or never built, so the score is a ratio with nothing
    // in its denominator. That prints as 100%, which is the right answer to "how much of what ran
    // was caught" and the wrong one to hand a threshold.
    let Some(score) = summary.scored() else {
        return Ok(ungraded(
            host,
            args,
            styler,
            summary.flaky > 0,
            plan.mutants.iter().any(|mutant| mutant.expectation.is_some()),
        ));
    };

    if let Some(minimum) = args.min_score
        && score < minimum
    {
        let (shown_score, shown_minimum) = distinguish(score, minimum);
        let mut stream = host.error();
        let _ = writeln!(
            stream,
            "{} mutation score {shown_score}% is below the required {shown_minimum}%",
            styler.error("error:")
        );

        return Ok(EXIT_GATE_FAILED);
    }

    Ok(EXIT_OK)
}

#[cfg(target_os = "linux")]
fn resume_relaunch<H: Host>(host: &H, args: &RunArgs) -> crate::Result<Option<i32>> {
    resume_relaunch_with(
        args,
        host.may_replace_process(),
        exec::relaunch::relaunched(),
        || exec::memory_support().is_ok(),
        || {
            let selected = crate::paths::physical(&args.select.dir)?;
            let (_, base, lock) = exec::claim_testing_state(&selected, args.measure.cache_dir.as_deref())?;
            let (state, _) = exec::TestingState::load(&base)?;
            drop(lock);
            Ok(!state.is_settled() && state.session.metered)
        },
        || {
            exec::relaunch::relaunch().map_err(|refusal| {
                error!(
                    "cannot resume the campaign's memory-controlled tests without cgroup delegation ({refusal:?}); \
                 run cargo-gamma inside a delegated systemd user scope"
                )
            })
        },
    )
}

#[cfg(any(target_os = "linux", test))]
fn resume_relaunch_with(
    args: &RunArgs,
    may_replace: bool,
    relaunched: bool,
    supported: impl FnOnce() -> bool,
    saved_metered: impl FnOnce() -> crate::Result<bool>,
    relaunch: impl FnOnce() -> crate::Result<Option<i32>>,
) -> crate::Result<Option<i32>> {
    if !may_replace || args.measure.no_relaunch || relaunched || supported() || !saved_metered()? {
        return Ok(None);
    }
    relaunch()
}

fn flaky_excess(args: &RunArgs, flaky: u32) -> Option<usize> {
    args.max_flaky
        .filter(|maximum| usize::try_from(flaky).unwrap_or(usize::MAX) > *maximum)
}

// #[gamma::skip(all, reason = "the gate truth table is asserted directly; instrumentation of this private predicate is not distinguishable from the calling gate branch")]
fn pending_fails_gate(args: &RunArgs, pending: u32) -> bool {
    (args.min_score.is_some() || args.max_flaky.is_some()) && pending > 0
}

/// Collects the arguments every test binary should receive.
///
/// `--cargo-test-arg` and everything after `--` mean the same thing to the harness, so they are
/// concatenated in the order they were written rather than kept apart.
fn test_arguments(args: &RunArgs) -> Vec<String> {
    let mut collected = args.measure.cargo_test_args.clone();

    collected.extend(args.measure.test_args.iter().cloned());
    collected
}

/// Reads the verdicts an earlier report already settled.
///
/// How many of this run's mutants the record settled for free.
///
/// Counted against the population rather than taken from the size of the record, because an entry
/// only spares a run if the mutant is still there to spare: a file that was deleted, a `--package`
/// that no longer selects it, or a shard that never held it all leave an entry that matched
/// nothing.
// #[gamma::skip(all, reason = "the exact adopted count is exercised through cache reporting; mutating this private projection duplicates that observable contract")]
fn adopted_from_cache(plan: &Plan, cached: &crate::HashMap<crate::model::MutantId, Outcome>) -> usize {
    plan.mutants
        .iter()
        .filter(|mutant| mutant.outcome == Outcome::CompileError && cached.contains_key(&mutant.id))
        .count()
}

/// Says how many mutants did not have to be rebuilt to be found unviable again.
///
/// Said out loud rather than left implicit, because this is on by default and it changes how long a
/// run takes. A user comparing two runs' timings deserves to know which of them started warm.
/// Names the skip directives that suppressed nothing this run, so they can be reconsidered.
///
/// A skip directive is a standing claim that something there cannot be tested. Once the code under
/// it changes, the claim keeps applying to nothing, and nothing in the report says so — which makes
/// a stale directive indistinguishable from a live one, and leaves the next reader believing a
/// decision that no longer holds. Reporting it is what lets that claim be audited.
///
/// Each is named with its file, line, selectors and stated reason. It is a note rather than a
/// failure, because a directive can legitimately be idle for a run — a mutant whose site the
/// diff or the shard excluded is not there to be suppressed — and the run's verdict must not turn
/// on how the population happened to be narrowed.
///
/// See [`crate::suppress::idle`] for exactly which directives reach here.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn report_idle<H: Host>(host: &mut H, plan: &Plan, styler: Styler) -> crate::Result<()> {
    if plan.idle.is_empty() {
        return Ok(());
    }

    writeln!(
        host.error(),
        "{} {} suppressed nothing and may no longer be needed",
        styler.verb("Unused"),
        quantity(plan.idle.len(), "skip directive")
    )?;

    for idle in &plan.idle {
        let reason = idle.reason.as_ref().map_or_else(String::new, |reason| format!(" — {reason}"));

        writeln!(host.error(), "  {}:{}: skip({}){reason}", idle.file, idle.line, idle.selectors)?;
    }

    Ok(())
}

// #[gamma::skip(all, reason = "diagnostic emission is asserted through an injected Host, while write-failure paths are intentionally best-effort and cannot add a distinct correctness verdict")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn report_cache<H: Host>(host: &mut H, adopted: usize, styler: Styler) -> crate::Result<()> {
    if adopted == 0 {
        return Ok(());
    }

    writeln!(
        host.error(),
        "{} {} known not to compile, carried forward rather than rebuilt",
        styler.verb("Cached"),
        quantity(adopted, "mutant")
    )?;

    Ok(())
}

/// Digests the things other than the sources that decide whether a mutant compiles.
///
/// `None` means this run has no trustworthy key — the compiler could not be asked what it is — and
/// the cache is then neither read nor written. That costs the run the time it would have saved,
/// which is the right side to fail on for a cache whose entries are believed rather than re-checked.
// #[gamma::skip(all, reason = "process toolchain and rustflags discovery is process-global and cannot be replaced safely in parallel tests; record-context hashing is tested independently with injected values")]
fn cache_context(args: &RunArgs) -> Option<crate::discover::ContextDigest> {
    let features = &args.select.features;
    let toolchain = crate::discover::toolchain();
    let rustflags = crate::discover::rustflags();

    crate::discover::record_context(&crate::discover::RecordContext {
        features: &features.features,
        all_features: features.all_features,
        no_default_features: features.no_default_features,
        profile: args.measure.profile.as_deref(),
        extra: &args.measure.cargo_args,
        rustflags: rustflags.as_deref(),
        toolchain: toolchain.as_deref(),
        test_packages: &args.measure.test_packages,
        test_lib: args.measure.test_lib,
        include_tests: &args.measure.include_tests,
        exclude_tests: &args.measure.exclude_tests,
        test_workspace: args.measure.test_workspace,
        whole_test_binaries: args.measure.whole_test_binaries,
        nextest: args.measure.nextest,
        cargo_test_args: &args.measure.cargo_test_args,
        test_args: &args.measure.test_args,
        baseline: !args.no_baseline,
        confirm: !args.no_confirm,
        stall: !args.no_stall_detection,
        test_timeout_multiplier: args.measure.test_timeout_multiplier,
        minimum_test_timeout: args.measure.minimum_test_timeout,
        memory: args.measure.memory,
        memory_multiplier: args.measure.memory_multiplier,
        memory_headroom: args.measure.memory_headroom,
        memory_limit: args.measure.memory_limit,
        baseline_memory_limit: args.measure.baseline_memory_limit,
        no_relaunch: args.measure.no_relaunch,
        copy_ignored: args.measure.copy_ignored,
        jobs: args.measure.jobs,
        resource_concurrency: &args.measure.resource_concurrency,
        build_timeout: args.limits.build_timeout,
        build_timeout_multiplier: args.limits.build_timeout_multiplier,
        rollback_rounds: args.limits.rollback_rounds,
    })
}

impl RecordPreparation {
    fn for_run(args: &RunArgs, survey: &crate::discover::Survey, context: crate::discover::ContextDigest) -> Self {
        let base = exec::campaign_base(&survey.root, &survey.target, args.measure.cache_dir.as_deref());
        let inputs = crate::discover::RunRecord::snapshot_with_external(
            &survey.root,
            &base,
            survey.external_inputs(),
            survey.has_untracked_build_script_inputs(),
        );

        Self {
            report_info: Some(run_info(args, None, &[])),
            base,
            context,
            inputs,
            killers: survey.killers(),
        }
    }
}

fn incremental_enabled(args: &RunArgs) -> bool {
    // Exact-mutant reruns intentionally bypass cached outcomes. Their requested IDs must resolve
    // to current pending, runnable mutants; stale, suppressed, unknown, or filtered IDs fail.
    !args.dry_run && args.mutants.is_empty() && args.incremental.unwrap_or(exec::IncrementalMode::Build).is_enabled()
}

#[cfg(test)]
fn incremental_context(args: &RunArgs) -> Option<crate::discover::ContextDigest> {
    incremental_enabled(args).then(|| cache_context(args)).flatten()
}

fn postprocessing_contexts(
    args: &RunArgs,
    observed: Option<crate::discover::ContextDigest>,
) -> Option<(crate::discover::ContextDigest, Option<crate::discover::ContextDigest>)> {
    (!args.dry_run).then(|| {
        let reusable = incremental_enabled(args).then(|| observed.clone()).flatten();
        (
            observed.unwrap_or_else(crate::discover::ContextDigest::from_inherited_environment),
            reusable,
        )
    })
}

fn store_completed_record(
    prepared: RecordPreparation,
    plan: &Plan,
    learning: Option<&crate::exec::Killers>,
) -> crate::Result<(bool, bool)> {
    let (base, stored) = publish_completed_record(prepared, plan, learning)?;
    let reachable =
        exec::remember_campaign_base(&plan.root, &base) && exec::campaign_base_from_state(&plan.root, None).as_ref() == Some(&base);
    Ok(postprocessing_advice(&plan.root, &stored, reachable))
}

fn publish_completed_record(
    prepared: RecordPreparation,
    plan: &Plan,
    learning: Option<&crate::exec::Killers>,
) -> crate::Result<(Utf8PathBuf, crate::discover::RunRecord)> {
    let Some(mut record) =
        crate::discover::RunRecord::from_completed_plan_snapshot(plan, &prepared.context, prepared.inputs, &prepared.killers)
    else {
        return Err(crate::error::error!(
            "could not construct completed campaign state from the captured source generation"
        ));
    };

    if let Some(learning) = learning {
        learning.stage(&mut record, &plan.mutants);
    }

    let stored = record.store_completed(&prepared.base)?;
    exec::finish_testing_record(&prepared.base)?;
    Ok((prepared.base, stored))
}

fn store_incomplete_convergence(prepared: &RecordPreparation, plan: &Plan) -> crate::Result<()> {
    let Some(record) =
        crate::discover::RunRecord::from_completed_plan_snapshot(plan, &prepared.context, prepared.inputs.clone(), &prepared.killers)
    else {
        return Err(crate::error::error!(
            "could not construct incomplete compiler learning from the captured source generation"
        ));
    };

    let _stored = record.store_incomplete_convergence(&prepared.base)?;
    Ok(())
}

fn store_failed_convergence(prepared: Option<&RecordPreparation>, failed: &exec::FailedMeasurement) -> crate::Result<()> {
    if let (Some(prepared), Some(plan)) = (prepared, failed.plan.as_ref()) {
        store_incomplete_convergence(prepared, plan)?;
    }
    Ok(())
}

fn postprocessing_advice(root: &Utf8Path, stored: &crate::discover::RunRecord, reachable: bool) -> (bool, bool) {
    if !reachable {
        return (false, false);
    }

    let suppressible = stored
        .outcomes()
        .iter()
        .any(|entry| matches!(entry.outcome, Outcome::Timeout | Outcome::OutOfMemory));

    (crate::discover::Hints::record_promotion_is_useful(root, stored), suppressible)
}

fn requested_mutants(values: &[String]) -> crate::Result<crate::HashSet<super::CanonicalMutantId>> {
    let mut requested = crate::HashSet::default();

    for value in values {
        let id = super::CanonicalMutantId::parse(value).map_err(|cause| error!("`--mutant {}`", cause).usage())?;
        if !requested.insert(id) {
            return Err(error!("mutant ID `{value}` was requested more than once").usage());
        }
    }

    Ok(requested)
}

/// Settles everything the measured run needs from the command line.
pub(super) fn run_config(args: &RunArgs, styler: Styler) -> exec::Config {
    exec::Config {
        jobs: exec::resolve_jobs(args.measure.jobs),
        test_timeout_multiplier: args.measure.test_timeout_multiplier.unwrap_or(DEFAULT_TEST_TIMEOUT_MULTIPLIER),
        baseline: !args.no_baseline,
        confirm: !args.no_confirm,
        stall: !args.no_stall_detection,
        cargo: exec::CargoOptions {
            features: args.select.features.to_cargo_args(),
            profile: args.measure.profile.clone(),
            extra: args.measure.cargo_args.clone(),
            test_args: test_arguments(args),
            color: styler.enabled(),
        },
        memory: memory_policy(args),
        build: exec::BuildLimits {
            timeout: args.limits.build_timeout.map(Duration::from_secs_f64),
            multiplier: args.limits.build_timeout_multiplier,
            rollback_rounds: args.limits.rollback_rounds,
        },
        leak_dirs: args.leak_dirs,
        cache_dir: args.measure.cache_dir.clone(),
        copy_ignored: args.measure.copy_ignored,
        test_packages: args.measure.test_packages.clone(),
        test_lib: args.measure.test_lib,
        include_tests: args.measure.include_tests.clone(),
        exclude_tests: args.measure.exclude_tests.clone(),
        test_workspace: args.measure.test_workspace,
        whole_test_binaries: args.measure.whole_test_binaries,
        nextest: args.measure.nextest,
        resources: args.measure.resource_concurrency.clone(),
        incremental: args.incremental.unwrap_or(exec::IncrementalMode::Build),
        timeout_floor: args
            .measure
            .minimum_test_timeout
            .map_or_else(|| exec::Config::default().timeout_floor, Duration::from_secs_f64),
        ..exec::Config::default()
    }
}

/// What a run produced: the plan, and whatever the build could not be made to compile.
pub(super) struct Executed {
    /// The completed plan, or `None` when nothing was generated at all.
    pub(super) plan: Option<Plan>,

    /// One entry per build the run gave up on, already rendered for a reader.
    ///
    /// Carried out of the run rather than turned into an error at the point it happens, because
    /// the whole point is that the report is still written: a run that got stuck has verdicts,
    /// diagnostics and a population worth publishing, and the failure belongs in the exit code
    /// rather than in place of all of that.
    pub(super) stuck: Vec<String>,
}

/// Discovers, runs and reports, returning everything [`run_session`] needs to decide an exit code.
/// Everything an earlier run already answered, folded into the survey before anything is built.
///
/// Incremental mode governs whether compiler unviability is reused from `last-gamma-run.json`.
// #[gamma::skip(all, reason = "cache adoption combines filesystem locks, process-global compiler context, and persisted records; deterministic record-settlement tests cover the data contract while isolated branch mutants are not safely observable")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn adopt(
    args: &RunArgs,
    survey: &mut crate::discover::Survey,
    base: &Utf8Path,
    context: Option<&crate::discover::ContextDigest>,
    inputs: &crate::discover::WorkspaceSnapshot,
) -> (crate::HashMap<crate::model::MutantId, Outcome>, usize, Vec<crate::discover::Term>) {
    let mode = args.incremental.unwrap_or(exec::IncrementalMode::Build);

    if !mode.is_enabled() {
        return (crate::HashMap::default(), 0, Vec::new());
    }

    // Read before the tree is copied. A dry run is deliberately left out of it: nothing is built,
    // so there is nothing to save, and marking a mutant unviable in a listing of what *would* run
    // would answer a question nobody asked.
    let (recorded, declined, moved) = match context {
        Some(context) if !args.dry_run => {
            let completed = crate::discover::RunRecord::load(base);
            let incomplete = crate::discover::RunRecord::load_incomplete(base);

            // Asked before the record is consumed, because what it costs the run is decided here
            // and reported nowhere else. A record whose unviability is refused still holds a build
            // order, so the reader is told which axis moved rather than that the cache "did not
            // apply" — the first is something to act on or accept, the second sends them through
            // their whole configuration.
            //
            // Resolved against the workspace first, because the record's own digest was. A term
            // one side does not state is not reported as a difference, so comparing against the
            // command line's unresolved digest would answer "nothing moved" for precisely the
            // configuration terms — the target and the `.cargo/config.toml` body — that a reader
            // has no other way to find.
            let resolved = context.resolved_at(&survey.root);
            let mut settled = crate::HashMap::default();
            let mut declined = 0usize;
            let mut moved = Vec::new();

            for record in [&completed, &incomplete] {
                if record.holds_unviability() {
                    for term in record.context().differences(&resolved) {
                        if !moved.contains(&term) {
                            moved.push(term);
                        }
                    }
                }

                let (recorded, record_declined) = record.settled_against(
                    &survey.root,
                    crate::discover::Trust::Free,
                    &crate::discover::Killers::default(),
                    context,
                    inputs,
                );
                settled.extend(recorded);
                declined = declined.saturating_add(record_declined);
            }

            (settled, declined, moved)
        }
        _absent_or_disabled => (crate::HashMap::default(), 0, Vec::new()),
    };

    // What the record contributed for free, which is what `report_cache` announces. A run that
    // asked for more must not have the rest credited to the free tier.
    let free: crate::HashMap<crate::model::MutantId, Outcome> = recorded
        .iter()
        .filter(|(_id, settled)| settled.outcome == Outcome::CompileError)
        .map(|(id, settled)| (id.clone(), settled.outcome))
        .collect();

    if !recorded.is_empty() {
        survey.settle_recorded(recorded);
    }

    (free, declined, moved)
}

#[expect(
    clippy::too_many_lines,
    reason = "the command orchestrator keeps its ordered reporting and resource-cleanup paths together"
)]
// #[gamma::skip(all, reason = "the command orchestrator coordinates subprocesses, cache locks, terminal state, and cleanup; its externally observable branches are covered by integration tests, while isolated mutations are platform/resource dependent")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn measured<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<Executed> {
    if args.resume_testing {
        return resumed_testing(host, args, progress_when, styler);
    }
    campaign_preflight(args, || {
        let metadata = crate::discover::load_metadata(&args.select.dir, &args.select.features)?;
        Ok(exec::campaign_base(&metadata.workspace_root, &metadata.target_directory, None))
    })?;
    let started = Instant::now();
    let selection = args.select.selection()?;
    let shard = args.select.shard()?;
    let visible = progress_when.resolve(host.is_terminal());
    let mut progress = Progress::new(visible, styler, host.terminal_width());

    progress.begin(host, "Analyzing", "Analyzed", "the workspace");

    // Settled before discovery rather than just before the run, because discovery evaluates
    // `#[cfg(...)]` against the build these options describe. Resolving it once and handing the
    // same value to both is what keeps the tree that is surveyed and the tree that is compiled from
    // describing different builds — a dry run included, since its listing is a claim about what a
    // real run would do.
    let config = run_config(args, styler);
    let observed_context = incremental_enabled(args).then(|| cache_context(args)).flatten();
    let contexts = postprocessing_contexts(args, observed_context);
    let requested = (!args.mutants.is_empty()).then(|| requested_mutants(&args.mutants)).transpose()?;
    let mut survey =
        match crate::discover::Survey::for_build_with_cache_inputs(&args.select, shard, &config.cargo, incremental_enabled(args)) {
            Ok(survey) => survey,
            Err(failure) => {
                progress.abandon(host);
                return Err(failure);
            }
        };
    progress.complete(host, "the workspace");

    if let Some(requested) = requested {
        survey.retain_only(requested.iter().cloned().map(super::CanonicalMutantId::into_inner).collect());

        let mut ordinals = 0;
        let scanned = survey.scan(None, &selection, &mut ordinals)?;
        let found: crate::HashSet<crate::model::MutantId> = scanned
            .mutants
            .iter()
            .filter(|mutant| mutant.outcome == Outcome::Pending)
            .map(|mutant| mutant.id.clone())
            .collect();
        let mut missing: Vec<&str> = requested
            .iter()
            .filter(|id| !found.contains(id.as_inner()))
            .map(super::CanonicalMutantId::as_str)
            .collect();
        missing.sort_unstable();

        if !missing.is_empty() {
            return Err(error!(
                "the requested mutant {} could not be selected from the current source and filters: {}. \
                 Re-run `cargo gamma list mutants --json` to obtain current IDs.",
                if missing.len() == 1 { "ID" } else { "IDs" },
                missing.join(", ")
            )
            .usage());
        }

        let purpose = if args.dry_run {
            "for a selection preview"
        } else {
            "for a fresh verdict"
        };
        writeln!(
            host.error(),
            "{} {} {purpose}",
            styler.verb("Selected"),
            quantity(requested.len(), "explicit mutant")
        )?;
    } else if args.only_survivors {
        let path = Documents::resolve(args, &survey.root).json;
        let report = crate::merge::read_limited(&path, u64::MAX)?.report;
        let survivors = crate::elements::surviving_mutants(&report)
            .map_err(|cause| error!("cannot select survivors from `{path}`: {cause}").usage())?;

        survey.retain_only(survivors);
    }

    let artifact_dir = Documents::directory(args, &survey.root);
    fs::create_dir_all(&artifact_dir).map_err(|cause| error!("could not create artifact directory `{artifact_dir}`").caused_by(cause))?;

    let (record_context, reusable_context) = contexts.map_or((None, None), |(record, reusable)| (Some(record), reusable));
    let record = record_context.map(|context| RecordPreparation::for_run(args, &survey, context));
    let cache_locks = if should_claim_cache(record.is_some(), args.dry_run) {
        Some(exec::claim_cache(&survey.root, &survey.target, args.measure.cache_dir.as_deref())?)
    } else {
        None
    };
    if let Some(prepared) = record.as_ref() {
        crate::discover::RunRecord::validate_update(&prepared.base)?;
    }
    #[cfg(any(test, feature = "internals"))]
    let cache_lock_identity = cache_locks.as_ref().map(exec::cache_lock_identity);
    let (cached, _declined, _moved) = reusable_context.as_ref().zip(record.as_ref()).map_or_else(
        || (crate::HashMap::default(), 0, Vec::new()),
        |(context, prepared)| adopt(args, &mut survey, &prepared.base, Some(context), &prepared.inputs),
    );

    // A dry run reports on the whole population and builds nothing, so there is no package-by-
    // package sequence to interleave the scan with; it is simply scanned.
    if args.dry_run {
        let mut ordinals = 0;
        let base = exec::campaign_base(&survey.root, &survey.target, args.measure.cache_dir.as_deref());
        let exclusions = crate::discover::CompilerExclusions::load(&base, &survey.root)?;
        let scanned = survey.scan_excluding(None, &selection, &mut ordinals, Some(&exclusions))?;
        let plan = survey.into_plan(scanned);

        progress.finish(host);
        if plan.mutants.is_empty() {
            let _ = writeln!(host.error(), "no mutants were generated");

            return Ok(Executed {
                plan: None,
                stuck: Vec::new(),
            });
        }

        crate::report::summarize(host, &plan, styler, listings(args, false, None, None))?;
        emit_reports(host, args, &plan, None, None, &[], styler)?;
        emit_diag(host, args, &plan, None, started, styler)?;

        return Ok(Executed {
            plan: Some(plan),
            stuck: Vec::new(),
        });
    }

    clear_baseline_failures(&artifact_dir)?;

    let dashboard = Dashboard::new(visible && args.dashboard, host.terminal_width(), styler);
    let mut events = ConsoleEvents {
        host,
        progress,
        dashboard,
        styler,
        show_build: args.measure.show_build,
        verdict_log: VerdictLog::default(),
    };

    // Exposes the post-adoption, pre-preparation boundary so the session test can prove that the
    // cache lock remains held across both phases.
    #[cfg(any(test, feature = "internals"))]
    if let Some(lock_identity) = cache_lock_identity {
        pause_after_cache_adoption(&survey.root, lock_identity);
    }
    let mut failed = exec::FailedMeasurement::default();
    if let Some(record) = &record {
        exec::invalidate_testing(&record.base)?;
    }
    let mut outcome = exec::run_with_locks(&survey, &selection, &config, &mut events, cache_locks, &mut failed, record.as_ref());

    // A phase that failed never got to say what it found, so the line it opened is still waiting
    // for an ending. Close it before the error is printed, or the error arrives as the rest of
    // that sentence.
    if outcome.is_err() {
        events.abandon();

        if let Err(failure) = store_failed_convergence(record.as_ref(), &failed) {
            crate::notes::note(format!("incomplete compiler learning was not saved: {failure}"));
        }
    }

    let log_result = events.finish_verdict_log();

    if let Err(failure) = &mut outcome {
        let fallback = survey.skeleton();
        let plan = failed.plan.as_ref().unwrap_or(&fallback);
        emit_failure_artifacts(&mut events, args, plan, failed.session.as_ref(), failure, &artifact_dir, started);
    }

    if let Some(mut work) = failed.work
        && let Err(cleanup) = work.teardown()
        && let Err(failure) = &mut outcome
    {
        failure.append_message(&format!("\nCleanup:       {cleanup}"));
    }

    let exec::Measured {
        plan,
        built,
        stuck,
        dropped,
        learning,
    } = outcome?;

    let log_failure = log_result.err();

    let mut progress = events.progress;
    let mut dashboard = events.dashboard;

    // The live display named every survivor and timeout as it happened, so the summary must not
    // name them again.
    let announced = progress.is_enabled();

    dashboard.finish(host);
    progress.finish(host);

    // Written from the whole population so an adopted cache is preserved. Failing here is not failing the run:
    // every verdict has already been reached, and a scratch file that could not be written must
    // only ever cost the next run some time.
    let mut promotable = false;
    let mut suppressible = false;
    let retained_info = record.as_ref().and_then(|record| record.report_info.clone());
    if let Some(prepared) = record {
        let record_locks = if built.is_none() {
            match exec::claim_cache(&plan.root, &survey.target, args.measure.cache_dir.as_deref()) {
                Ok(locks) => Some(locks),
                Err(failure) => {
                    crate::notes::note(format!("could not lock the run-record cache: {failure}"));
                    None
                }
            }
        } else {
            None
        };

        if built.is_some() || record_locks.is_some() {
            match store_completed_record(prepared, &plan, learning.as_ref()) {
                Ok((stored_promotable, stored_suppressible)) => {
                    promotable = stored_promotable;
                    suppressible = stored_suppressible;
                }
                Err(failure) => {
                    writeln!(
                        host.error(),
                        "{} completed campaign state was not saved: {failure}",
                        styler.warning()
                    )?;
                }
            }
        }
    }

    let adopted = adopted_from_cache(&plan, &cached);

    report_cache(host, adopted, styler)?;
    if plan.mutants.is_empty() {
        let _ = writeln!(host.error(), "no mutants were generated");
        warn_auxiliary(host, log_failure.as_ref(), styler);

        return Ok(Executed { plan: None, stuck });
    }

    // With nothing live there was no build to pay for, or the build that would have decided the run
    // could not be made to compile. Either way the summary already accounts for every mutant —
    // suppressed, sharded away, already settled, or never built — and it is written before the
    // failure is reported, because a report that exists is the whole point of getting this far.
    let Some(mut built) = built else {
        crate::report::summarize(host, &plan, styler, listings(args, announced, Some(promotable), Some(suppressible)))?;

        emit_reports(host, args, &plan, stuck_panel(&stuck).as_deref(), None, &dropped, styler)?;
        emit_diag(host, args, &plan, None, started, styler)?;
        report_dropped(host, &dropped, styler)?;
        report_stuck(host, &stuck, styler)?;
        warn_auxiliary(host, log_failure.as_ref(), styler);

        return Ok(Executed { plan: Some(plan), stuck });
    };

    built.work.snapshot_report_sources(&plan)?;

    if args.leak_dirs {
        let tree = exec::scratch_tree(&plan.root, args.measure.cache_dir.as_deref());

        writeln!(host.error(), "{} {tree}", styler.verb("Kept"))?;
    }

    let mut footer = listings(args, announced, Some(promotable), Some(suppressible));
    footer.stats = Some(stats(&built.session, started.elapsed()));
    crate::report::summarize(host, &plan, styler, footer)?;
    crate::report::session_notes(host, &built.session, false, false, styler)?;

    let wall = started.elapsed();
    let panel = summary_panel(args, &plan, &built.session, &stuck, &dropped, wall);

    emit_reports_from(
        host,
        args,
        &plan,
        &ReportContents {
            retained_info: retained_info.as_ref(),
            source_root: Some(&built.work.report_source_root()),
            advice: Some(&panel),
            tests: built.session.tests,
            dropped: &dropped,
        },
        styler,
    )?;
    emit_advice(host, args, &plan, &built.session, wall, styler)?;
    emit_diag(host, args, &plan, Some(&built.session), started, styler)?;
    report_dropped(host, &dropped, styler)?;
    report_stuck(host, &stuck, styler)?;
    warn_auxiliary(host, log_failure.as_ref(), styler);

    // Driven here rather than left to the destructor. The scratch tree is a copy of the workspace
    // plus its build artifacts, so removing it walks every file in both, and doing that on the way
    // out of the function looks from outside like a tool that has finished and will not exit. Its
    // failure is said out loud rather than swallowed, and does not fail a run whose every verdict
    // is already reached and reported: what is left behind costs the disk, not the answer.
    if let Err(failure) = built.work.teardown() {
        writeln!(host.error(), "{} {failure}", styler.warning())?;
    }

    Ok(Executed { plan: Some(plan), stuck })
}

fn should_claim_cache(persisting_record: bool, dry_run: bool) -> bool {
    persisting_record && !dry_run
}

fn campaign_preflight(args: &RunArgs, resolve_default: impl FnOnce() -> crate::Result<Utf8PathBuf>) -> crate::Result<()> {
    if args.dry_run {
        return Ok(());
    }
    let base = match args.measure.cache_dir.as_deref() {
        Some(cache) => exec::gamma_base(&args.select.dir, Some(cache)),
        None => resolve_default()?,
    };
    crate::discover::RunRecord::validate_update(&base)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn resumed_testing<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<Executed> {
    resumed_testing_with_support(host, args, progress_when, styler, exec::memory_support, |_| {})
}

#[expect(
    clippy::too_many_lines,
    reason = "resume keeps execution and publication-only recovery in one ordered reporting and lock-cleanup path"
)]
fn resumed_testing_with_support<H: Host, E: std::error::Error + Send + Sync + 'static>(
    host: &mut H,
    args: &RunArgs,
    progress_when: When,
    styler: Styler,
    support: impl FnOnce() -> Result<(), E>,
    configure_log: impl FnOnce(&mut VerdictLog),
) -> crate::Result<Executed> {
    let started = Instant::now();
    let selected = crate::paths::physical(&args.select.dir)?;
    let (root, base, lock) = exec::claim_testing_state(&selected, args.measure.cache_dir.as_deref())?;
    let (state, valid_bytes) = exec::TestingState::load(&base)?;
    // Settled replay needs no execution capabilities, prepared artifacts, or testing workspace.
    // Frozen evidence and captured-source provenance remain mandatory, and publication keeps the lock.
    let publication_only = state.is_settled();
    if !publication_only && state.session.metered {
        support().map_err(|cause| {
            error!(
                "cannot resume the campaign's memory-controlled tests on this host; restore memory-control support or start a new campaign"
            )
            .caused_by(cause)
        })?;
    }
    if !exec::same_existing_path(&state.plan.root, &root) {
        return Err(error!(
            "cannot resume testing: the saved campaign belongs to a different workspace; start a new campaign"
        ));
    }
    validate_resumed_environment(&state.plan.mutants, || state.record.context.validate_inherited_environment())?;
    crate::discover::RunRecord::validate_update(&base)?;
    if !publication_only {
        state.validate_artifacts()?;
    }
    let record = state.record.clone();
    let retained_info = resumed_report_info(record.report_info.as_ref());
    let retained_session = publication_only.then(|| state.session.clone());
    let (work, _publication_lock) = if publication_only {
        (None, Some(lock))
    } else {
        (
            Some(exec::Workspace::resume_testing(
                &state.plan.root,
                &state.record.base,
                state.workspace.clone(),
                lock,
                valid_bytes,
                exec::TestingResumeIo::FILESYSTEM,
            )?),
            None,
        )
    };
    let profile = match work.as_ref() {
        Some(work) => work.reporting_profile(),
        None => state
            .workspace
            .get("cargo")
            .and_then(|cargo| cargo.get("profile"))
            .and_then(serde_json::Value::as_str),
    };
    let reporting_args = prepare_resumed_reporting(host, args, &state, profile, styler)?;
    let visible = progress_when.resolve(host.is_terminal());
    let mut verdict_log = VerdictLog::default();
    verdict_log.resume(&base)?;
    configure_log(&mut verdict_log);
    let mut events = ConsoleEvents {
        progress: Progress::new(visible, styler, host.terminal_width()),
        dashboard: Dashboard::new(visible && args.dashboard, host.terminal_width(), styler),
        host,
        styler,
        show_build: false,
        verdict_log,
    };
    let settled = events
        .progress
        .restore_outcomes(state.plan.mutants.iter().map(|mutant| mutant.outcome));
    writeln!(
        events.host.error(),
        "{} testing with {} retained",
        styler.verb("Resuming"),
        quantity(settled, "settled mutant")
    )?;
    let outcome = match work {
        Some(work) => exec::resume_testing(state, work, &mut events),
        None => state.complete_testing().map(|()| exec::Measured {
            plan: state.plan,
            built: None,
            learning: Some(state.killers),
            stuck: state.stuck,
            dropped: state.dropped,
        }),
    };
    let log_failure = events.finish_verdict_log().err();
    if outcome.is_err() {
        events.abandon();
    }
    let exec::Measured {
        plan,
        built,
        stuck,
        dropped,
        learning,
    } = outcome?;
    events.dashboard.finish(events.host);
    events.progress.finish(events.host);
    let announced = events.progress.is_enabled();
    let host = events.host;
    let (base, stored) = publish_completed_record(record, &plan, learning.as_ref())?;
    let reachable = exec::remember_campaign_base(&plan.root, &base);
    let (promotable, suppressible) = postprocessing_advice(&plan.root, &stored, reachable);
    let mut built = built;
    let (session, source_root) = if let Some(built) = built.as_mut() {
        built.work.snapshot_report_sources(&plan)?;
        (built.session.clone(), built.work.report_source_root())
    } else {
        (
            retained_session.expect("only publication-only recovery completes without a built session"),
            snapshot_captured_report_sources(&base, &plan)?,
        )
    };
    let wall = started.elapsed();
    let mut footer = listings(args, announced, Some(promotable), Some(suppressible));
    footer.stats = Some(stats(&session, wall));
    crate::report::summarize(host, &plan, styler, footer)?;
    crate::report::session_notes(host, &session, false, false, styler)?;
    let panel = summary_panel(&reporting_args, &plan, &session, &stuck, &dropped, wall);
    emit_reports_from(
        host,
        args,
        &plan,
        &ReportContents {
            retained_info: Some(&retained_info),
            source_root: Some(&source_root),
            advice: Some(&panel),
            tests: session.tests,
            dropped: &dropped,
        },
        styler,
    )?;
    emit_advice(host, &reporting_args, &plan, &session, wall, styler)?;
    emit_diag(host, &reporting_args, &plan, Some(&session), started, styler)?;
    report_dropped(host, &dropped, styler)?;
    report_stuck(host, &stuck, styler)?;
    warn_auxiliary(host, log_failure.as_ref(), styler);
    if let Some(built) = built.as_mut()
        && let Err(failure) = built.work.teardown()
    {
        writeln!(host.error(), "{} {failure}", styler.warning())?;
    }
    Ok(Executed { plan: Some(plan), stuck })
}

fn validate_resumed_environment(mutants: &[Mutant], validate: impl FnOnce() -> crate::Result<()>) -> crate::Result<()> {
    if mutants.iter().any(|mutant| mutant.outcome == Outcome::Pending) {
        validate()?;
    }
    Ok(())
}

/// Materializes frozen campaign sources for publication-only recovery, independently of the
/// current checkout and execution artifacts.
fn snapshot_captured_report_sources(base: &Utf8Path, plan: &Plan) -> crate::Result<Utf8PathBuf> {
    let root = base.join(".cargo-gamma-report-source");
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(error!("report source directory `{root}` is a link"));
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(&root).map_err(|cause| error!("could not clear report source directory `{root}`").caused_by(cause))?;
        }
        Ok(_) => return Err(error!("report source path `{root}` is not a directory")),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
        Err(cause) => return Err(error!("could not inspect report source directory `{root}`").caused_by(cause)),
    }
    fs::create_dir(&root).map_err(|cause| error!("could not create report source directory `{root}`").caused_by(cause))?;
    for file in &plan.files {
        if let Some(source) = &file.source {
            crate::elements::write(&root.join(&file.path), source)?;
        }
    }
    Ok(root)
}

fn prepare_resumed_reporting<H: Host>(
    host: &mut H,
    args: &RunArgs,
    state: &exec::TestingState,
    profile: Option<&str>,
    styler: Styler,
) -> crate::Result<RunArgs> {
    if state.record.report_info.is_none() {
        writeln!(
            host.error(),
            "{} the legacy checkpoint has no report provenance; its report uses an unknown timestamp and cannot withdraw other report populations",
            styler.warning()
        )?;
    }
    if state.settings.mutators.is_none() {
        writeln!(
            host.error(),
            "{} the legacy checkpoint has no original mutator selection; diagnostics list the frozen population's mutators instead",
            styler.warning()
        )?;
    }
    Ok(resumed_reporting_args(
        args,
        state.settings.jobs,
        state.settings.mutators.as_deref(),
        &state.plan.mutants,
        state.record.report_info.as_ref().and_then(|info| info.shard.as_ref()),
        profile,
    ))
}

fn resumed_reporting_args(
    args: &RunArgs,
    jobs: usize,
    selection: Option<&str>,
    population: &[crate::model::Mutant],
    shard: Option<&crate::elements::ShardInfo>,
    profile: Option<&str>,
) -> RunArgs {
    let mut reporting = RunArgs {
        artifact_dir: args.artifact_dir.clone(),
        diag: args.diag,
        diag_names: args.diag_names,
        ..RunArgs::default()
    };
    reporting.measure.jobs = Some(jobs);
    reporting.measure.profile = profile.map(str::to_owned);
    reporting.select.mutators = Some(selection.map_or_else(
        || {
            let mut mutators: Vec<_> = population.iter().map(|mutant| mutant.mutator.as_ref()).collect();
            mutators.sort_unstable();
            mutators.dedup();
            mutators.join(",")
        },
        str::to_owned,
    ));
    if let Some(shard) = shard {
        reporting.select.shard_count = Some(shard.count);
        reporting.select.shard_index = Some(shard.index);
    }
    reporting
}

fn clear_baseline_failures(artifact_dir: &Utf8Path) -> crate::Result<()> {
    let directory = artifact_dir.join("baseline-failures");

    match fs::remove_dir_all(directory.as_std_path()) {
        Ok(()) => Ok(()),
        Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(error!("could not remove stale baseline failures from `{directory}`").caused_by(cause)),
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn emit_failure_artifacts<H: Host>(
    events: &mut ConsoleEvents<'_, H>,
    args: &RunArgs,
    plan: &Plan,
    session: Option<&exec::Session>,
    failure: &mut Error,
    artifact_dir: &Utf8Path,
    started: Instant,
) {
    if failure.artifacts().is_empty() {
        return;
    }

    let canonical = Documents::resolve(args, &plan.root).diag;
    if let Err(diag_failure) = write_diag_bundle_to(args, plan, session, started, &canonical) {
        crate::exec::Events::warn(
            events,
            &format!("could not preserve the canonical diagnostics bundle after the baseline failure: {diag_failure}"),
        );
    } else {
        let _ = writeln!(
            events.host.error(),
            "{} {}",
            events.styler.footer_wrote(),
            encode_controls(&platform_path(&canonical))
        );
    }

    let mut written_records = 0;

    for artifact in failure.artifacts() {
        let directory = artifact_dir.join(&artifact.directory);
        let path = directory.join(artifact.file_name);
        let diagnostics = directory.join("diags.json");
        let written = serde_json::to_string_pretty(&artifact.value)
            .map_err(|cause| error!("could not encode `{path}`").caused_by(cause))
            .and_then(|contents| crate::elements::write(&path, &contents));

        if let Err(artifact_failure) = written {
            crate::exec::Events::warn(
                events,
                &format!("could not preserve the baseline failure record: {artifact_failure}"),
            );
        } else {
            written_records += 1;
            let _ = writeln!(
                events.host.error(),
                "{} {}",
                events.styler.footer_wrote(),
                encode_controls(&platform_path(&path))
            );
        }

        if let Err(diag_failure) = write_diag_bundle_to(args, plan, session, started, &diagnostics) {
            crate::exec::Events::warn(
                events,
                &format!("could not preserve the diagnostics bundle after the baseline failure: {diag_failure}"),
            );
        } else {
            let _ = writeln!(
                events.host.error(),
                "{} {}",
                events.styler.footer_wrote(),
                encode_controls(&platform_path(&diagnostics))
            );
        }
    }

    let directory = artifact_dir.join("baseline-failures");
    let count = failure.artifacts().len();
    if written_records == count {
        failure.set_message(format!(
            "baseline measurement failed due to {count} test failures; see the generated files in {} for details",
            encode_controls(&platform_path(&directory))
        ));
    } else {
        failure.set_message(format!(
            "baseline measurement failed due to {count} test failures; only {written_records} of {count} failure files could be written under {}",
            encode_controls(&platform_path(&directory))
        ));
    }
}

fn platform_path(path: &Utf8Path) -> String {
    #[cfg(windows)]
    {
        path.as_str().replace('/', "\\")
    }

    #[cfg(not(windows))]
    {
        path.as_str().to_owned()
    }
}

// #[gamma::skip(all, reason = "auxiliary diagnostic writes are deliberately best-effort, so an injected write failure and an omitted write have the same command result")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn warn_auxiliary<H: Host>(host: &mut H, failure: Option<&Error>, styler: Styler) {
    if let Some(failure) = failure {
        let _ = writeln!(host.error(), "{} {failure}", styler.warning());
    }
}

/// Renders the builds that could not be made to compile for the job summary.
///
/// The console says this too, but a console is not an artifact: the summary panel is what a team
/// reads the next morning, and a score with a silently missing population is exactly the thing that
/// should not be readable without the caveat beside it.
// #[gamma::skip(all, reason = "the exact artifact fragment is covered by deterministic formatting tests; mutating this private formatter duplicates the report contract")]
fn stuck_panel(stuck: &[String]) -> Option<String> {
    if stuck.is_empty() {
        return None;
    }

    let mut panel = "### Builds that could not be made to compile\n\n".to_owned();

    for reason in stuck {
        panel.push_str("```\n");
        panel.push_str(reason);
        panel.push_str("\n```\n");
    }

    Some(panel)
}

/// Assembles the job summary: the advice, and every caveat that belongs beside a score.
///
/// The caveats go under the advice rather than in place of it. The run did produce verdicts, and
/// the panel is where a team reads them, so the part of the population nobody could build — and
/// the part of the suite nobody could run — belongs on the same page as the score they are missing
/// from.
// #[gamma::skip(all, reason = "the assembled summary is asserted as an artifact; mutations in this private composition layer duplicate the component formatter contracts")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn summary_panel(args: &RunArgs, plan: &Plan, session: &exec::Session, stuck: &[String], dropped: &[String], wall: Duration) -> String {
    // The job summary wants a fragment under the heading it already owns; the artifact wants a whole
    // document. Same analysis, two shapes.
    let mut panel = advice_markdown(args, plan, session, wall, crate::advise::Layout::Embedded);

    for section in [stuck_panel(stuck), dropped_panel(dropped)].into_iter().flatten() {
        panel.push('\n');
        panel.push_str(&section);
    }

    panel
}

/// Renders the dropped test packages for the job summary.
// #[gamma::skip(all, reason = "the exact artifact fragment is covered by deterministic formatting tests; mutating this private formatter duplicates the report contract")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn dropped_panel(dropped: &[String]) -> Option<String> {
    if dropped.is_empty() {
        return None;
    }

    Some(format!(
        "### Test packages dropped from this run\n\n         These packages do not compile in this workspace, and neither the selected packages nor the          whole workspace would check while they were in it. The run went ahead over the packages          being mutated instead of refusing to start.\n\n         Their test targets were not built and did not run, so any mutant they would have killed is          reported here as a survivor. Fix them, or narrow the run yourself with `--test-package`,          before reading this score against one taken over the whole workspace.\n\n```\n{}\n```\n",
        dropped.join("\n")
    ))
}

/// Says which test packages the run had to drop, and what that costs the score.
///
/// A warning rather than an error: every verdict this run reached is real, and the run is not a
/// failure. What it is not is comparable with a run over the whole workspace, and nothing else on
/// screen says so.
// #[gamma::skip(all, reason = "the warning text is covered through an injected Host; mutations here duplicate the artifact panel's dropped-package contract")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn report_dropped<H: Host>(host: &mut H, dropped: &[String], styler: Styler) -> crate::Result<()> {
    if dropped.is_empty() {
        return Ok(());
    }

    writeln!(
        host.error(),
        "{} these packages do not compile, so their tests were neither built nor run and could \
         convict nothing: {}. Mutants they would have killed are reported as survivors.",
        styler.warning(),
        dropped.join(", ")
    )?;

    Ok(())
}

/// Says, loudly and last, which builds could not be made to compile.
///
/// Last because it has to be the thing still on screen when the run ends, and on the error stream
/// because it is a failure: the report beside it is real, but it describes a population the tool
/// could not finish judging, and a reader who scrolls past this line would take a partial answer
/// for a complete one.
fn report_stuck<H: Host>(host: &mut H, stuck: &[String], styler: Styler) -> crate::Result<()> {
    for reason in stuck {
        writeln!(host.error(), "{} {reason}", styler.error("error:"))?;
    }

    Ok(())
}

/// Dumps the run's own numbers, when the hidden `--diag` asked for them.
///
/// Last, and to the diagnostic stream, because it is neither a result nor something a person
/// reading the summary asked to see.
fn emit_diag<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    session: Option<&exec::Session>,
    started: Instant,
    styler: Styler,
) -> crate::Result<()> {
    let jobs = exec::resolve_jobs(args.measure.jobs);

    if args.diag {
        write!(host.error(), "\n{}", crate::diag::render(plan, session, jobs, started.elapsed()))?;
    }

    emit_diag_bundle(host, args, plan, session, started, styler)
}

/// Writes the diagnostics bundle.
///
/// Written on every run, like the reports beside it: the file is only useful if it is already there
/// when someone decides the run was slow, and a flag they have to know about beforehand is a flag
/// nobody has set by the time it matters.
fn emit_diag_bundle<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    session: Option<&exec::Session>,
    started: Instant,
    styler: Styler,
) -> crate::Result<()> {
    let path = Documents::resolve(args, &plan.root).diag;

    emit_diag_bundle_to(host, args, plan, session, started, styler, &path)
}

fn emit_diag_bundle_to<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    session: Option<&exec::Session>,
    started: Instant,
    styler: Styler,
    path: &Utf8Path,
) -> crate::Result<()> {
    write_diag_bundle_to(args, plan, session, started, path)?;
    writeln!(host.error(), "{} {}", styler.footer_wrote(), encode_controls(&platform_path(path)))?;

    Ok(())
}

fn write_diag_bundle_to(
    args: &RunArgs,
    plan: &Plan,
    session: Option<&exec::Session>,
    started: Instant,
    path: &Utf8Path,
) -> crate::Result<()> {
    let jobs = exec::resolve_jobs(args.measure.jobs);
    let context = crate::diag::Context {
        cores: exec::available_parallelism(),
        jobs,
        wall: started.elapsed(),
        mutators: args.select.selection().map(|selection| selection.sorted()).unwrap_or_default(),
        shard: args.select.shard().ok().flatten(),

        // Only when `--diag` asked for it. Measuring it is a walk of every build artifact the run
        // produced, which on a large workspace costs more than the figure is worth to a run that
        // did not ask a question about disk.
        scratch_bytes: session
            .filter(|_measured| args.diag)
            .map(|session| exec::footprint(&session.scratch)),
        redaction: args.diag_names,
        version: env!("CARGO_PKG_VERSION"),
    };

    let bundle = crate::diag::bundle(plan, session, &context);

    crate::elements::write(path, &crate::diag::to_json(&bundle)?)
        .map_err(|cause| crate::error::error!("could not write the diagnostics bundle to `{path}`").caused_by(cause))?;

    Ok(())
}

/// Writes the Markdown diagnosis.
fn emit_advice<H: Host>(
    host: &mut H,
    args: &RunArgs,
    plan: &Plan,
    session: &exec::Session,
    wall: Duration,
    styler: Styler,
) -> crate::Result<()> {
    let path = Documents::resolve(args, &plan.root).advice;
    let advice = advice_markdown(args, plan, session, wall, crate::advise::Layout::Document);

    // Whole or not at all, like the reports beside it: a half-written diagnosis is one a reader
    // takes for the whole story.
    crate::elements::write(&path, &advice)
        .map_err(|cause| crate::error::error!("could not write the advice to `{path}`").caused_by(cause))?;

    writeln!(host.error(), "{} {}", styler.footer_wrote(), encode_controls(&platform_path(&path)))?;

    Ok(())
}

/// Renders the diagnosis and the family table as Markdown.
///
/// The family table is part of the diagnosis rather than a separate feature: knowing that a run's
/// time went somewhere is only actionable alongside what that somewhere caught.
fn advice_markdown(args: &RunArgs, plan: &Plan, session: &exec::Session, wall: Duration, layout: crate::advise::Layout) -> String {
    let timing = crate::advise::Timing {
        build: session.build,
        baseline: session.baseline_wall,
        wall,
        jobs: exec::resolve_jobs(args.measure.jobs),
    };

    let findings = crate::advise::analyze_run(&plan.mutants, &timing, args.measure.profile.as_deref(), &session.binaries);

    let summary = crate::model::Summary::of(&plan.mutants);

    crate::advise::render_markdown(&findings, &crate::advise::yields(&plan.mutants), summary, &timing, layout)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod miri_tests {
    use super::*;

    #[test]
    fn resumed_reporting_uses_frozen_execution_and_live_reporting_options() {
        let mut live = RunArgs {
            artifact_dir: Some("reports".into()),
            diag: true,
            diag_names: crate::diag::Redaction::Names,
            ..RunArgs::default()
        };
        live.measure.jobs = Some(99);
        live.measure.profile = Some("test".to_owned());
        live.select.mutators = Some("invalid-live-selection".to_owned());
        live.select.shard_count = Some(0);
        live.select.shard_index = Some(99);
        let shard = crate::elements::ShardInfo { count: 3, index: 1 };
        let frozen = resumed_reporting_args(&live, 7, Some("relational.lt_to_le"), &[], Some(&shard), Some("gamma"));
        assert_eq!(frozen.measure.jobs, Some(7));
        assert_eq!(frozen.measure.profile.as_deref(), Some("gamma"));
        assert_eq!(frozen.select.selection().unwrap().sorted(), ["relational.lt_to_le"]);
        assert_eq!(frozen.select.shard().unwrap(), Some((3, 1)));
        assert_eq!(frozen.artifact_dir, live.artifact_dir);
        assert!(frozen.diag);
        assert!(matches!(frozen.diag_names, crate::diag::Redaction::Names));
        assert_eq!(live.measure.jobs, Some(99));
    }

    #[test]
    fn legacy_resumed_reporting_uses_population_without_live_selection_or_shards() {
        let mut live = RunArgs::default();
        live.select.mutators = Some("relational".to_owned());
        live.select.shard_count = Some(5);
        live.select.shard_index = Some(2);
        let first = crate::fixtures::mutant_at("first", "src/lib.rs", 1, "relational.lt_to_le", Outcome::Pending);
        let second = crate::fixtures::mutant_at("second", "src/lib.rs", 2, "arith.add_to_sub", Outcome::Killed);
        let frozen = resumed_reporting_args(&live, 4, None, &[first.clone(), second, first], None, None);
        assert_eq!(frozen.measure.jobs, Some(4));
        assert_eq!(frozen.measure.profile, None);
        assert_eq!(
            frozen.select.selection().unwrap().sorted(),
            ["arith.add_to_sub", "relational.lt_to_le"]
        );
        assert_eq!(frozen.select.shard().unwrap(), None);
        assert!(!frozen.diag);
    }

    #[test]
    fn exact_mutant_ids_are_validated_and_deduplicated() {
        let selected = requested_mutants(&["deadbeefcafe".to_owned(), "0123456789ab".to_owned()]).expect("valid IDs");

        assert_eq!(selected.len(), 2);
        let _ = requested_mutants(&["DEADBEEFCAFE".to_owned()]).expect_err("uppercase is not canonical");
        let _ = requested_mutants(&["short".to_owned()]).expect_err("short IDs are invalid");
        let _ = requested_mutants(&["deadbeefcafe".to_owned(), "deadbeefcafe".to_owned()]).expect_err("duplicate IDs are ambiguous");
    }

    #[test]
    fn exact_mutant_selection_disables_cached_verdict_adoption() {
        let mut args = RunArgs::default();

        assert!(incremental_enabled(&args));
        args.mutants.push("deadbeefcafe".to_owned());
        assert!(!incremental_enabled(&args));
    }

    #[test]
    fn population_authority_is_explicit_and_filtered_runs_cannot_claim_it() {
        let mut args = RunArgs::default();
        assert!(run_info(&args, None, &[]).population_complete);
        args.mutants.push("deadbeefcafe".into());
        assert!(!run_info(&args, None, &[]).population_complete);
        args.mutants.clear();
        args.only_survivors = true;
        assert!(!run_info(&args, None, &[]).population_complete);
        args.only_survivors = false;
        args.select.in_diff = Some("changes.patch".into());
        assert!(!run_info(&args, None, &[]).population_complete);
        args.select.in_diff = None;
        args.select.shard_count = Some(2);
        args.select.shard_index = Some(0);
        assert!(!run_info(&args, None, &[]).population_complete);
        args.select.shard_count = None;
        args.select.shard_index = None;
        args.select.files.push("src/*.rs".into());
        args.select.exclude_files.push("src/generated.rs".into());
        args.select.packages.push("included-package".into());
        assert!(run_info(&args, None, &[]).population_complete);
        for mutators in ["arith.add_to_sub", "all", "@all"] {
            args.select.mutators = Some(mutators.into());
            assert!(!run_info(&args, None, &[]).population_complete, "{mutators}");
        }
        assert!(!resumed_report_info(None).population_complete);
    }

    #[test]
    fn legacy_frozen_run_info_defaults_to_non_authoritative_population_on_resume() {
        let mut frozen = serde_json::to_value(run_info(&RunArgs::default(), None, &[])).unwrap();
        assert!(frozen.as_object_mut().unwrap().remove("populationComplete").is_some());
        let decoded: crate::elements::RunInfo = serde_json::from_value(frozen).unwrap();
        assert!(!decoded.population_complete);
        let resumed = resumed_report_info(Some(&decoded));
        assert!(!resumed.population_complete);
        let reported = report_info(&RunArgs::default(), Some(&resumed), Some(3), &[]);
        assert!(!reported.population_complete);
    }

    #[test]
    fn exact_id_and_diff_report_metadata_preserve_unchanged_historical_survivors_through_the_wire() {
        let exact = RunArgs {
            mutants: vec!["deadbeefcafe".into()],
            ..RunArgs::default()
        };
        let mut diff = RunArgs::default();
        diff.select.in_diff = Some("changes.patch".into());
        for args in [exact, diff] {
            let old = crate::fixtures::report_with(
                None,
                100,
                vec![
                    crate::fixtures::mutant_result_at("a", 1, "Survived"),
                    crate::fixtures::mutant_result_at("b", 2, "Survived"),
                ],
            );
            let mut newer = crate::fixtures::report_with(None, 200, vec![crate::fixtures::mutant_result_at("a", 1, "Killed")]);
            let mut info = run_info(&args, None, &[]);
            info.started_at = 200;
            newer.config = Some(info);
            let wire = crate::elements::to_json(&newer).unwrap();
            let decoded = serde_json::from_str(&wire).unwrap();
            let merged = crate::merge::merge(&[("old".into(), old), ("new".into(), decoded)], 300, None);
            assert_eq!((merged.withdrawn, merged.valid, merged.detected), (0, 2, 1));
        }
    }

    #[test]
    fn mutator_filtered_reports_preserve_other_mutator_survivors_in_the_same_file_through_the_wire() {
        for selector in ["arith.add_to_sub", "all", "@all"] {
            let mut args = RunArgs::default();
            args.select.mutators = Some(selector.into());
            let mut arithmetic = crate::fixtures::mutant_result_at("arithmetic", 1, "Survived");
            arithmetic.mutator_name = "arith.add_to_sub".into();
            let mut relational = crate::fixtures::mutant_result_at("relational", 1, "Survived");
            relational.mutator_name = "relational.lt_to_le".into();
            let old = crate::fixtures::report_with(None, 100, vec![arithmetic.clone(), relational]);
            arithmetic.status = "Killed".into();
            let mut newer = crate::fixtures::report_with(None, 200, vec![arithmetic]);
            let mut info = run_info(&args, None, &[]);
            info.started_at = 200;
            newer.config = Some(info);
            let wire = crate::elements::to_json(&newer).unwrap();
            let decoded: crate::elements::Report = serde_json::from_str(&wire).unwrap();
            assert!(!decoded.config.as_ref().unwrap().population_complete, "{selector}");
            let merged = crate::merge::merge(&[("old".into(), old), ("new".into(), decoded)], 300, None);
            assert_eq!((merged.withdrawn, merged.valid, merged.detected), (0, 2, 1), "{selector}");
            let report = merged.report.unwrap();
            let retained = report.files["src/lib.rs"]
                .mutants
                .iter()
                .find(|mutant| mutant.id == "relational")
                .unwrap();
            assert_eq!(retained.mutator_name, "relational.lt_to_le");
            assert_eq!(retained.status, "Survived");
        }
    }

    #[test]
    fn flaky_gate_is_independent_of_the_score_gate() {
        let mut args = RunArgs {
            min_score: Some(100.0),
            max_flaky: Some(0),
            ..RunArgs::default()
        };

        assert_eq!(flaky_excess(&args, 1), Some(0));
        assert_eq!(flaky_excess(&args, 0), None);

        args.max_flaky = Some(2);
        assert_eq!(flaky_excess(&args, 2), None);
        assert_eq!(flaky_excess(&args, 3), Some(2));
    }

    #[test]
    fn a_flaky_only_gate_requires_a_complete_population() {
        let args = RunArgs {
            max_flaky: Some(0),
            ..RunArgs::default()
        };

        assert!(pending_fails_gate(&args, 1));
        assert!(!pending_fails_gate(&args, 0));
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    use super::super::cli::MeasureArgs;
    use super::*;
    use crate::discover::{RunRecord, TargetFile};
    use crate::exec::Session;
    use crate::fixtures;

    #[test]
    fn resume_delegation_uses_frozen_metering_and_preserves_child_exit_status() {
        let mut args = RunArgs::default();
        args.measure.memory = Some(exec::MemoryControl::Off);
        let code = resume_relaunch_with(&args, true, false, || false, || Ok(true), || Ok(Some(73))).unwrap();
        assert_eq!(code, Some(73), "live settings must not disable a metered checkpoint's delegation");
        assert_eq!(
            resume_relaunch_with(
                &args,
                true,
                false,
                || false,
                || Ok(false),
                || panic!("an unmetered checkpoint needs no delegated scope")
            )
            .unwrap(),
            None
        );
        assert_eq!(
            resume_relaunch_with(&args, true, false, || false, || Ok(true), || Err(error!("delegation unavailable")))
                .unwrap_err()
                .to_string(),
            "delegation unavailable"
        );
        assert_eq!(
            resume_relaunch_with(
                &args,
                true,
                false,
                || false,
                || Err(error!("checkpoint unreadable")),
                || panic!("invalid state must not relaunch")
            )
            .unwrap_err()
            .to_string(),
            "checkpoint unreadable"
        );
    }

    #[test]
    fn resume_delegation_skips_relaunch_for_supported_disabled_or_recursive_invocations() {
        for (may_replace, no_relaunch, relaunched, supported) in [
            (false, false, false, false),
            (true, true, false, false),
            (true, false, true, false),
            (true, false, false, true),
        ] {
            let mut args = RunArgs::default();
            args.measure.no_relaunch = no_relaunch;
            assert_eq!(
                resume_relaunch_with(
                    &args,
                    may_replace,
                    relaunched,
                    || supported,
                    || panic!("the skip path must not claim or read campaign state"),
                    || panic!("the skip path must not relaunch")
                )
                .unwrap(),
                None
            );
        }
    }

    /// Lays out an empty single-package `subject` workspace in `dir` and returns its root.
    ///
    /// The manifest, offline Cargo configuration, and `src` directory are shared; each caller
    /// writes its own `src/lib.rs` so deliberately missing or malformed sources remain testable.
    fn subject_root(dir: &tempfile::TempDir) -> Utf8PathBuf {
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir(root.join("src")).expect("src");
        crate::testing::write_project(
            root.as_std_path(),
            &[(
                "Cargo.toml",
                "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
            )],
        );
        root
    }

    use crate::testing::{Broken, Sink, fails_at_every_line, private_system_tempdir, workdir};

    #[test]
    fn survivor_ungraded_gate_matrix_names_only_the_gates_that_were_not_evaluated() {
        for (score, flaky, observed_flaky, expectations, expected) in [
            (false, false, false, false, None),
            (true, false, false, false, Some("the `--min-score` gate")),
            (false, true, false, false, Some("the `--max-flaky` gate")),
            (true, true, false, false, Some("the `--min-score` and `--max-flaky` gates")),
            (true, true, true, false, Some("the `--min-score` gate")),
            (false, true, true, false, None),
            (false, false, false, true, Some("the expectations this run carried")),
        ] {
            let args = RunArgs {
                min_score: score.then_some(80.0),
                max_flaky: flaky.then_some(0),
                ..RunArgs::default()
            };
            let mut host = Sink::default();
            assert_eq!(
                ungraded(&mut host, &args, Styler::new(false), observed_flaky, expectations),
                if expected.is_some() { EXIT_GATE_FAILED } else { EXIT_OK }
            );
            assert_eq!(host.out(), "");
            assert_eq!(host.err(), expected.map_or_else(String::new, |gate| format!(
                "error: no mutant counted toward {gate}, so it was never evaluated; check that the selection — `--in-diff`, `--exclude-file`, `--shard-count`/`--shard-index`, `--incremental` — leaves something to test\n"
            )));
        }
    }

    #[test]
    fn survivor_expectation_scan_continues_past_unexpected_and_pending_entries() {
        let mutants = [
            mutant(0, Outcome::Survived, 0),
            expecting(Outcome::Pending, true),
            expecting(Outcome::Survived, true),
            expecting(Outcome::Killed, false),
            expecting(Outcome::Killed, true),
        ];
        let broken = broken_expectations(&mutants);
        assert_eq!(broken.len(), 2);
        assert!(core::ptr::eq(broken[0].0, &raw const mutants[2]));
        assert!(core::ptr::eq(broken[1].0, &raw const mutants[3]));
    }

    #[test]
    fn survivor_run_metadata_and_empty_sweep_statistics_keep_unknowns_distinct_from_measurements() {
        let mut args = RunArgs::default();
        args.select.shard_count = Some(7);
        args.select.shard_index = Some(3);
        let now = UNIX_EPOCH + Duration::from_secs(1_234_567);
        let info = run_info_at(&args, Some(19), &["omitted".to_owned()], now);
        assert_eq!(info.started_at, 1_234_567);
        assert_eq!(info.tests, Some(19));
        assert_eq!(info.dropped_test_packages, ["omitted"]);
        assert_eq!(info.shard.map(|shard| (shard.count, shard.index)), Some((7, 3)));
        let mut session = session();
        session.phases.sweep = None;
        let stats = stats(&session, Duration::from_millis(37));
        assert_eq!(stats.elapsed, Duration::from_millis(37));
        assert_eq!(stats.binaries, session.binaries.len());
        assert_eq!(stats.launches, 0);
        assert_eq!(stats.successful_hints, 0);
    }

    #[test]
    fn survivor_platform_paths_and_postprocessing_context_reuse_preserve_the_supplied_values() {
        let path = Utf8Path::new("artifacts/sub/report.json");
        #[cfg(windows)]
        assert_eq!(platform_path(path), r"artifacts\sub\report.json");
        #[cfg(not(windows))]
        assert_eq!(platform_path(path), "artifacts/sub/report.json");
        let observed = crate::discover::record_context(&crate::discover::RecordContext {
            toolchain: Some("distinct-survivor-toolchain"),
            ..crate::discover::RecordContext::default()
        })
        .unwrap();
        let mut args = RunArgs::default();
        let (actual, reusable) = postprocessing_contexts(&args, Some(observed.clone())).unwrap();
        assert_eq!(actual, observed);
        assert_eq!(reusable, Some(observed.clone()));
        args.incremental = Some(exec::IncrementalMode::No);
        let (actual, reusable) = postprocessing_contexts(&args, Some(observed.clone())).unwrap();
        assert_eq!(actual, observed);
        assert!(reusable.is_none());
        args.dry_run = true;
        assert!(postprocessing_contexts(&args, Some(observed)).is_none());
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_artifact_failures_remain_errors_and_do_not_print_success_messages() {
        let directory = workdir("run-artifact-survivors-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let plan = plan_at(root.clone());
        let blocked = root.join("not-a-directory");
        fs::write(&blocked, "preserved").unwrap();
        let args = RunArgs {
            artifact_dir: Some(blocked.clone()),
            ..RunArgs::default()
        };
        let mut host = Sink::default();
        let styler = Styler::new(false);
        let started = Instant::now();
        let session = session();
        assert!(emit_reports(&mut host, &args, &plan, None, None, &[], styler).is_err());
        assert!(emit_ci(&mut host, &args, &plan, None, styler).is_err());
        assert!(emit_diag_bundle(&mut host, &args, &plan, Some(&session), started, styler).is_err());
        assert!(emit_diag_bundle_to(&mut host, &args, &plan, None, started, styler, &blocked.join("diag")).is_err());
        assert!(emit_advice(&mut host, &args, &plan, &session, Duration::from_secs(1), styler).is_err());
        assert!(host.err.is_empty());
        assert!(host.out.is_empty());
        assert_eq!(fs::read_to_string(blocked).unwrap(), "preserved");
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_artifact_diagnostic_errors_propagate_after_each_written_document() {
        let directory = workdir("run-diagnostic-survivors-");
        let plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        let args = RunArgs::default();
        let session = session();
        let styler = Styler::new(false);
        crate::testing::fails_at_every_line(3, |host| emit_reports(host, &args, &plan, None, Some(19), &[], styler));
        crate::testing::fails_at_every_line(1, |host| emit_ci(host, &args, &plan, None, styler));
        crate::testing::fails_at_every_line(1, |host| {
            emit_diag_bundle(host, &args, &plan, Some(&session), Instant::now(), styler)
        });
        crate::testing::fails_at_every_line(1, |host| emit_advice(host, &args, &plan, &session, Duration::from_secs(1), styler));
        crate::testing::fails_at_every_line(2, |host| report_stuck(host, &["first build".into(), "second build".into()], styler));
        let report = crate::merge::read_limited(&Documents::resolve(&args, &plan.root).json, u64::MAX)
            .unwrap()
            .report;
        assert_eq!(report.config.unwrap().tests, Some(19));
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_ci_summary_separates_advice_and_respects_explicitly_disabled_annotations() {
        let directory = workdir("run-summary-survivors-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
        let plan = plan_at(root.clone());
        let path = root.join("step-summary.md");
        let mut host = Sink::default().with_env("GITHUB_STEP_SUMMARY", path.as_str());
        let mut args = RunArgs {
            annotations: crate::ci::Annotations::Github,
            ..RunArgs::default()
        };
        emit_ci(&mut host, &args, &plan, Some("DISTINCT ADVICE"), Styler::new(true)).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{}\nDISTINCT ADVICE", crate::ci::summary(&plan.mutants, &plan.root))
        );
        assert!(host.err().contains('\u{1b}'));
        assert!(!host.out.is_empty());
        fs::remove_file(&path).unwrap();
        host.out.clear();
        args.annotations = crate::ci::Annotations::None;
        emit_ci(&mut host, &args, &plan, Some("SHOULD NOT BE APPENDED"), Styler::new(false)).unwrap();
        assert!(host.out.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn survivor_configuration_preserves_nondefault_execution_fields_and_fine_score_precision() {
        let mut args = RunArgs::default();
        args.measure.test_lib = true;
        args.measure.whole_test_binaries = true;
        args.measure.resource_concurrency = vec![exec::ResourceLimit::new("survivor-resource", 2).unwrap()];
        let config = run_config(&args, Styler::new(true));
        assert!(config.test_lib);
        assert!(config.whole_test_binaries);
        assert_eq!(config.resources, args.measure.resource_concurrency);
        assert!(config.cargo.color);
        assert_eq!(
            distinguish(1.0, 1.000_000_000_001),
            ("1.000000000000".to_owned(), "1.000000000001".to_owned())
        );
        assert_eq!(
            distinguish(1.0, 1.000_000_000_000_1),
            ("1".to_owned(), "1.0000000000001".to_owned())
        );
    }

    #[test]
    fn survivor_ci_annotations_propagate_non_broken_pipe_result_errors() {
        struct Denied;
        impl Write for Denied {
            fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "results denied"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        #[derive(Debug, Default)]
        struct DeniedResults {
            diagnostics: Vec<u8>,
        }
        impl Host for DeniedResults {
            fn output(&mut self) -> impl Write {
                Denied
            }
            fn error(&mut self) -> impl Write {
                &mut self.diagnostics
            }
            fn is_terminal(&self) -> bool {
                false
            }
            fn terminal_width(&self) -> Option<u16> {
                None
            }
        }
        let directory = workdir("ci-denied-survivors-");
        let plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        let args = RunArgs {
            annotations: crate::ci::Annotations::Github,
            ..RunArgs::default()
        };
        let failure = emit_ci(&mut DeniedResults::default(), &args, &plan, None, Styler::new(false)).unwrap_err();
        assert!(failure.to_string().contains("results denied"));
        assert!(Documents::resolve(&args, &plan.root).sarif.is_file());
    }

    #[cfg(feature = "system-tests")]
    mod native_resume_acceptance {
        use super::*;

        #[test]
        fn survivor_resumed_testing_returns_an_absent_selected_workspace_error_before_claiming_or_loading_state() {
            let directory = workdir("resume-absent-selected-workspace-");
            let root = Utf8Path::from_path(directory.path()).expect("fixture roots are UTF-8");
            for (component, expected) in [("absent", "no previous campaign under"), ("\0", "could not resolve")] {
                let args = RunArgs {
                    select: crate::commands::SelectArgs {
                        dir: root.join(component),
                        ..crate::commands::SelectArgs::default()
                    },
                    resume_testing: true,
                    ..RunArgs::default()
                };
                let mut host = Sink::default();
                let Err(error) = resumed_testing(&mut host, &args, When::Never, Styler::new(false)) else {
                    panic!("an absent or physically invalid selected workspace cannot load a resumable campaign");
                };
                assert!(error.to_string().contains(expected), "{error}");
                assert!(
                    host.err().is_empty(),
                    "no resume announcement may precede selected-workspace validation"
                );
            }
        }

        fn survivor_resume_checkpoint() -> (tempfile::TempDir, RunArgs, serde_json::Value) {
            let directory = private_system_tempdir("resumed-command-checkpoint-");
            let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).expect("fixture roots can be normalized"))
                .expect("fixture roots are UTF-8");
            let mut plan = plan_at(root.clone());
            let text = fs::read_to_string(root.join("src/lib.rs")).expect("actual fixture source exists");
            let parsed = crate::parse::SourceFile::parse("src/lib.rs", text).expect("actual fixture source parses");
            let selection = crate::ops::Selection::parse("all").expect("the built-in selection is valid");
            let candidates = crate::ops::collect_candidates(&parsed, &selection);
            plan.mutants = crate::ops::into_mutants(&parsed, "subject", candidates)
                .into_iter()
                .take(3)
                .collect();
            assert_eq!(plan.mutants.len(), 3);
            for (index, (mutant, outcome)) in plan
                .mutants
                .iter_mut()
                .zip([Outcome::CompileError, Outcome::Timeout, Outcome::Survived])
                .enumerate()
            {
                mutant.ordinal = u32::try_from(index + 1).expect("the fixture has only three mutants");
                mutant.outcome = outcome;
            }
            let base = crate::paths::physical(&root.join("target").join("campaign"))
                .expect("the fixture campaign uses normalized native path components");
            let prepared = base.join("workspace");
            let runtime = base.join("rt");
            crate::exec::create_private_dir_all_for_test(&runtime);
            fs::create_dir_all(base.join("target")).expect("compiled-output fixture directory can be created");
            for file in ["Cargo.toml", "src/lib.rs"] {
                let destination = prepared.join(file);
                fs::create_dir_all(destination.parent().expect("fixture files have parents")).expect("prepared parents can be created");
                fs::copy(root.join(file), &destination).expect("the prepared generation copies actual source");
            }
            crate::exec::mark_cache_owned_for_test(&root, &base);
            let mut measured = session();
            measured.tests = Some(3);
            measured.scratch = base.clone();
            measured.filtered = 1;
            measured.widened = true;
            let mut artifacts = Vec::new();
            for name in ["Cargo.toml", "src/lib.rs"] {
                let path = prepared.join(name);
                #[cfg(unix)]
                let executable = fs::metadata(&path).expect("prepared file exists").permissions().mode() & 0o111;
                #[cfg(not(unix))]
                let executable = 0;
                artifacts.push(serde_json::json!({
                    "path": path, "digest": crate::discover::digest(&fs::read(&path).expect("prepared bytes are readable")),
                    "generated_output": false, "metadata": { "kind": "File", "executable": executable, "link": null }
                }));
            }
            let state = survivor_resume_snapshot(&plan, &measured, &base, &prepared, &runtime, &artifacts);
            let args = RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                measure: crate::commands::MeasureArgs {
                    cache_dir: Some(base.clone()),
                    ..crate::commands::MeasureArgs::default()
                },
                resume_testing: true,
                artifact_dir: Some(root.join("reports")),
                diag: true,
                dashboard: true,
                ..RunArgs::default()
            };
            survivor_write_resume_snapshot(&base, &state);
            {
                let (claimed_root, claimed_base, _lock) = exec::claim_testing_state(&root, Some(&base))
                    .expect("the fixture's actual testing-state owner and native lock are valid");
                assert!(
                    exec::same_existing_path(&claimed_root, &root),
                    "claimed root {claimed_root:?} differs from fixture root {root:?}"
                );
                assert!(
                    exec::same_existing_path(&claimed_base, &base),
                    "claimed base {claimed_base:?} differs from fixture base {base:?}"
                );
            }
            let (loaded, _) =
                exec::TestingState::load(&base).expect("the checkpoint passes the actual version, identity, confidence and journal reader");
            loaded
                .validate_artifacts()
                .expect("the actual prepared files match the fixture's retained native metadata and bytes");
            (directory, args, state)
        }

        fn survivor_resume_snapshot(
            plan: &Plan,
            measured: &Session,
            base: &Utf8Path,
            prepared: &Utf8Path,
            runtime: &Utf8Path,
            artifacts: &[serde_json::Value],
        ) -> serde_json::Value {
            serde_json::json!({
                "version": 2, "tool": env!("CARGO_PKG_VERSION"),
                "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
                "plan": plan, "confidences": {}, "session": measured,
                "oracle": { "packages": ["subject"], "package_local": true, "whole_workspace": false },
                "workspace": {
                    "base": base, "root": prepared, "target": base.join("target"), "runtime": runtime,
                    "libraries": [], "cargo": exec::CargoOptions::default(), "nextest": null, "test_environment": null,
                    "scheduler_version": 2,
                    "launch": { "loader": null, "stack": "2097152" }, "harness_threads": null,
                    "resources": { "capacities": {}, "binaries": {} }
                },
                "settings": { "jobs": 1, "mutators": "all", "timeout_floor": Duration::from_secs(1),
                    "confirm": false, "narrowed": false, "incremental": false },
                "killers": exec::Killers::default(),
                "record": {
                    "report_info": fixtures::report_with(None, 77, Vec::new()).config,
                    "base": base, "context": crate::discover::record_context(&crate::discover::RecordContext {
                        toolchain: Some("fixture"), ..crate::discover::RecordContext::default()
                    }).expect("the fixture names its compiler and captures the real inherited environment"),
                    "inputs": RunRecord::snapshot_with_external(&plan.root, base, &[], false), "killers": crate::discover::Killers::default()
                },
                "stuck": [], "dropped": ["missing-fixture-package"], "artifact_roots": [prepared, runtime], "artifacts": artifacts
            })
        }

        fn survivor_write_resume_snapshot(base: &Utf8Path, state: &serde_json::Value) {
            let bytes = serde_json::to_vec(state).expect("the concrete checkpoint fixture serializes");
            fs::write(base.join("testing-session.json"), &bytes).expect("checkpoint fixture is writable");
            fs::write(base.join("testing-verdicts.jsonl"), "").expect("settled fixture has no uncommitted verdict");
            fs::write(
                base.join("testing-phase.json"),
                serde_json::to_vec(&serde_json::json!({
                    "phase": "Testing", "snapshot": crate::discover::digest(&bytes)
                }))
                .expect("phase fixture serializes"),
            )
            .expect("phase fixture is writable");
        }

        type SurvivorResumeHook = Option<(&'static str, Box<dyn FnOnce()>)>;

        /// Diagnostic-driven fault injection using accumulated per-stream markers and a one-shot hook.
        #[derive(Default)]
        struct SurvivorResumeHost {
            sink: Sink,
            failure: Option<&'static str>,
            failed: bool,
            hook: SurvivorResumeHook,
        }

        impl Host for SurvivorResumeHost {
            fn output(&mut self) -> impl Write {
                SurvivorResumeWriter {
                    bytes: &mut self.sink.out,
                    failure: self.failure,
                    failed: &mut self.failed,
                    hook: &mut self.hook,
                }
            }

            fn error(&mut self) -> impl Write {
                SurvivorResumeWriter {
                    bytes: &mut self.sink.err,
                    failure: self.failure,
                    failed: &mut self.failed,
                    hook: &mut self.hook,
                }
            }

            fn is_terminal(&self) -> bool {
                self.sink.is_terminal()
            }

            fn terminal_width(&self) -> Option<u16> {
                self.sink.terminal_width()
            }

            fn env(&self, name: &str) -> Option<String> {
                self.sink.env(name)
            }
        }

        /// Writer adapter implementing the host's accumulated-marker control plane.
        struct SurvivorResumeWriter<'a> {
            bytes: &'a mut Vec<u8>,
            failure: Option<&'static str>,
            failed: &'a mut bool,
            hook: &'a mut SurvivorResumeHook,
        }

        impl Write for SurvivorResumeWriter<'_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                // Retain bytes before matching; run the hook once before returning any selected error.
                // This fixes the observed resume phase without racing a concurrent filesystem edit.
                self.bytes.extend_from_slice(buf);
                let output = String::from_utf8_lossy(self.bytes);
                if self.hook.as_ref().is_some_and(|(marker, _)| output.contains(*marker)) {
                    let (_, callback) = self.hook.take().expect("the preceding presence check found this one-shot callback");
                    callback();
                }
                if self.failure.is_some_and(|marker| output.contains(marker)) {
                    *self.failed = true;
                    return Err(std::io::Error::new(
                        ErrorKind::PermissionDenied,
                        "selected resumed-command output was denied",
                    ));
                }
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn survivor_actual_resumed_command_propagates_each_selected_output_failure_at_its_own_caller_boundary() {
            for marker in [
                "Resuming",
                "Summary:",
                "the narrowed build did not compile",
                "gamma-report.json",
                "gamma-perf-advice.md",
                "gamma-diagnostics.json",
                "these packages do not compile",
            ] {
                let (_directory, args, _) = survivor_resume_checkpoint();
                let mut host = SurvivorResumeHost {
                    failure: Some(marker),
                    ..SurvivorResumeHost::default()
                };
                let Err(_error) = resumed_testing(&mut host, &args, When::Never, Styler::new(false)) else {
                    panic!("the checked {marker:?} output boundary must return its writer error");
                };
                assert!(host.failed, "the native command must actually reach the selected {marker:?} caller");
            }
        }

        #[test]
        fn survivor_resumed_publication_failure_returns_the_actual_phase_error_without_starting_execution_progress() {
            let (_directory, args, _) = survivor_resume_checkpoint();
            let phase = args
                .measure
                .cache_dir
                .as_ref()
                .expect("fixture cache exists")
                .join("testing-phase.json");
            let blocked = phase.clone();
            let mut host = SurvivorResumeHost {
                hook: Some((
                    "Resuming",
                    Box::new(move || {
                        fs::remove_file(&blocked).expect("the validated phase can disappear after the resume announcement");
                        fs::create_dir(&blocked).expect("a native directory can prevent the next committed phase publication");
                    }),
                )),
                ..SurvivorResumeHost::default()
            };
            let Err(error) = resumed_testing(&mut host, &args, When::Always, Styler::new(false)) else {
                panic!("a failed FinishedTesting publication must propagate from actual resumed execution");
            };
            assert!(
                phase.is_dir(),
                "the actual callback performed its deterministic native filesystem race"
            );
            assert!(error.to_string().contains("testing-phase.json"), "{error}");
            let diagnostics = host.sink.err();
            assert!(
                !diagnostics.contains("\x1b[?2026h"),
                "publication does not start an execution dashboard"
            );
            assert!(diagnostics.contains("Resuming"), "the actual publication path announced recovery");
            assert!(
                !diagnostics.contains("\r\x1b[2K"),
                "publication does not draw an execution progress bar"
            );
        }

        #[test]
        fn survivor_resumed_auxiliary_verdict_failure_is_warned_after_publishing_the_completed_population() {
            let (_directory, args, mut state) = survivor_resume_checkpoint();
            state["plan"]["mutants"][2]["outcome"] = serde_json::json!(Outcome::Pending);
            let base = args.measure.cache_dir.as_ref().expect("fixture cache exists");
            survivor_write_resume_snapshot(base, &state);
            let mut host = Sink::default();
            let result = resumed_testing_with_support(
                &mut host,
                &args,
                When::Never,
                Styler::new(false),
                || Ok::<(), std::io::Error>(()),
                |log| {
                    let VerdictLog::Writing { file, path, .. } = log else {
                        panic!("native resume installed the actual verdict file");
                    };
                    *file = fs::File::open(path).expect("the same native journal can be substituted with a read-only descriptor");
                },
            )
            .expect("the verdict log is auxiliary, so the real no-binary NoCoverage outcome remains reportable");
            let plan = result.plan.expect("the population survives an auxiliary logging failure");
            assert_eq!(plan.mutants[2].outcome, Outcome::NoCoverage);
            assert!(host.err().contains("could not write the campaign log"), "{}", host.err());
            assert!(
                base.join("last-gamma-run.json").is_file(),
                "the warning is emitted only after durable completion publication"
            );
            assert!(
                Documents::resolve(&args, &args.select.dir).json.is_file(),
                "the checked report publication still completed"
            );
        }

        #[test]
        fn survivor_actual_resumed_command_preserves_settled_outcomes_and_publishes_frozen_report_metadata() {
            let (_directory, args, _) = survivor_resume_checkpoint();
            let summary = args.select.dir.join("step-summary.md");
            let mut host = Sink::default()
                .with_env("GITHUB_ACTIONS", "true")
                .with_env("GITHUB_STEP_SUMMARY", summary.as_str());
            let executed = resumed_testing(&mut host, &args, When::Never, Styler::new(false))
                .expect("settled testing resumes without launching any build or test process");
            let plan = executed
                .plan
                .expect("a successfully resumed command returns its completed population");
            assert_eq!(
                plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
                [Outcome::CompileError, Outcome::Timeout, Outcome::Survived]
            );
            assert!(host.err().contains("testing with 2 settled mutants retained"), "{}", host.err());
            assert!(
                host.out().contains(&plan.mutants[2].describe()),
                "an unannounced retained survivor must appear in the results listing: {}",
                host.out()
            );
            assert!(host.err().contains("missing-fixture-package"), "{}", host.err());
            let phase: serde_json::Value = serde_json::from_slice(
                &fs::read(args.measure.cache_dir.as_ref().expect("fixture cache").join("testing-phase.json"))
                    .expect("phase survives completion"),
            )
            .expect("phase remains valid JSON");
            assert_eq!(phase["phase"], "Completed");
            let reports = args.artifact_dir.as_ref().expect("fixture has an explicit report directory");
            assert!(reports.is_dir(), "the actual postprocessing path creates its report artifacts");
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(reports.join("gamma-report.json")).expect("the actual JSON report was published"))
                    .expect("the actual report is valid JSON");
            assert_eq!(report["config"]["startedAt"], 77);
            assert_eq!(report["config"]["tests"], 3);
            let diagnostic: serde_json::Value = serde_json::from_slice(
                &fs::read(reports.join("gamma-diagnostics.json")).expect("the actual diagnostic bundle was published"),
            )
            .expect("the actual diagnostic bundle is valid JSON");
            assert_eq!(diagnostic["build"]["tests"], 3);
            assert!(host.out().contains("Stats  :"), "{}", host.out());
            assert!(host.out().contains("0 test processes launched"), "{}", host.out());
            assert!(
                fs::read_to_string(summary)
                    .expect("the real CI summary was written")
                    .contains("missing-fixture-package")
            );
        }

        #[test]
        fn survivor_resumed_memory_support_is_required_only_for_the_frozen_metered_session() {
            for metered in [false, true] {
                let (_directory, args, mut state) = survivor_resume_checkpoint();
                state["session"]["metered"] = serde_json::Value::Bool(metered);
                if metered {
                    state["plan"]["mutants"][2]["outcome"] = serde_json::json!(Outcome::Pending);
                }
                let base = args.measure.cache_dir.as_ref().expect("fixture has a cache");
                survivor_write_resume_snapshot(base, &state);
                let called = std::cell::Cell::new(false);
                let mut host = Sink::default();
                let result = resumed_testing_with_support(
                    &mut host,
                    &args,
                    When::Never,
                    Styler::new(false),
                    || {
                        called.set(true);
                        Err(std::io::Error::new(
                            ErrorKind::Unsupported,
                            "fixture host has no delegated memory boundary",
                        ))
                    },
                    |_| {},
                );
                assert_eq!(called.get(), metered);
                if metered {
                    let Err(error) = result else {
                        panic!("a memory-controlled checkpoint cannot silently lose its boundary")
                    };
                    assert!(
                        error.to_string().contains("cannot resume the campaign's memory-controlled tests"),
                        "{error}"
                    );
                    assert!(host.err().is_empty(), "host support is checked before the resume announcement");
                } else {
                    assert!(
                        matches!(result, Ok(Executed { plan: Some(_), .. })),
                        "unmetered settled work needs no host boundary"
                    );
                }
            }
        }

        #[test]
        fn survivor_actual_resumed_terminal_matrix_does_not_start_execution_progress_for_publication_only() {
            for visible in [false, true] {
                for dashboard in [false, true] {
                    let (_directory, mut args, _) = survivor_resume_checkpoint();
                    args.dashboard = dashboard;
                    args.show_unviable = true;
                    let mut host = Sink::default();
                    resumed_testing(
                        &mut host,
                        &args,
                        if visible { When::Always } else { When::Never },
                        Styler::new(true),
                    )
                    .expect("the settled terminal matrix never launches a process");
                    let diagnostics = host.err();
                    assert!(
                        diagnostics.contains("\x1b["),
                        "requested styling reaches the actual resume announcement"
                    );
                    assert!(
                        !diagnostics.contains("\x1b[?2026h"),
                        "publication-only recovery never starts an execution dashboard"
                    );
                    assert!(!diagnostics.contains("\r\x1b[2K"), "publication has no execution progress bar");
                    assert!(
                        host.out().contains("unviable"),
                        "the requested bulk unviable listing survives footer construction: {}",
                        host.out()
                    );
                }
            }
        }

        #[test]
        fn survivor_actual_resumed_advice_uses_persisted_campaign_reachability_instead_of_outcome_only_fallbacks() {
            for reachable in [false, true] {
                let notes = crate::notes::Run::new();
                let _scope = crate::notes::enter(Some(&notes));
                let (_directory, args, _) = survivor_resume_checkpoint();
                if !reachable {
                    fs::create_dir_all(exec::gamma_base(&args.select.dir, None).join("campaign-location"))
                        .expect("a real directory can block the campaign-location file publication");
                }
                let mut host = Sink::default();
                resumed_testing(&mut host, &args, When::Never, Styler::new(false))
                    .expect("completed verdicts remain reportable when only the locator cannot be published");
                assert_eq!(host.out().contains("Run `cargo gamma suppress`"), reachable, "{}", host.out());
                assert_eq!(host.out().contains("Run `cargo gamma hints`"), reachable, "{}", host.out());
                assert_eq!(
                    crate::notes::drain()
                        .iter()
                        .any(|note| note.contains("could not save campaign-state location")),
                    !reachable
                );
            }
        }

        #[test]
        fn survivor_actual_resumed_command_returns_each_native_validation_and_postprocessing_collision() {
            for case in [
                "workspace",
                "artifact",
                "prepared",
                "log",
                "record",
                "snapshot",
                "reports",
                "advice",
                "diagnostics",
            ] {
                let (_directory, args, mut state) = survivor_resume_checkpoint();
                let base = args.measure.cache_dir.as_ref().expect("the fixture has an explicit cache");
                let expected = match case {
                    "workspace" => {
                        let other = args.select.dir.join("other-workspace");
                        let _other_plan = plan_at(other.clone());
                        state["plan"]["root"] = serde_json::json!(other);
                        state["plan"]["files"][0]["absolute"] = serde_json::json!(other.join("src/lib.rs"));
                        survivor_write_resume_snapshot(base, &state);
                        "saved campaign belongs to a different workspace"
                    }
                    "artifact" => {
                        state["plan"]["mutants"][2]["outcome"] = serde_json::json!(Outcome::Pending);
                        survivor_write_resume_snapshot(base, &state);
                        fs::remove_file(base.join("workspace/src/lib.rs")).expect("an inspected source artifact can actually disappear");
                        "prepared"
                    }
                    "prepared" => {
                        state["plan"]["mutants"][2]["outcome"] = serde_json::json!(Outcome::Pending);
                        state["workspace"]["target"] = serde_json::json!(base.join("foreign-target"));
                        survivor_write_resume_snapshot(base, &state);
                        "prepared testing workspace does not belong"
                    }
                    "log" => {
                        fs::create_dir(base.join(crate::commands::verdict_log::TESTING_PROGRESS_LOG))
                            .expect("a real directory blocks reopening the progress file");
                        "could not reopen testing progress log"
                    }
                    "record" => {
                        fs::create_dir(base.join("last-gamma-run.json")).expect("a real directory blocks completed-record publication");
                        "refusing to overwrite campaign state"
                    }
                    "snapshot" => {
                        fs::write(base.join(".cargo-gamma-report-source"), "occupied")
                            .expect("a native file blocks the report-source directory");
                        "report source path"
                    }
                    "reports" => {
                        fs::create_dir_all(Documents::resolve(&args, &args.select.dir).json)
                            .expect("a real directory blocks report JSON publication");
                        "could not write"
                    }
                    "advice" => {
                        fs::create_dir_all(Documents::resolve(&args, &args.select.dir).advice)
                            .expect("a real directory blocks advice publication");
                        "could not write the advice"
                    }
                    "diagnostics" => {
                        fs::create_dir_all(Documents::resolve(&args, &args.select.dir).diag)
                            .expect("a real directory blocks diagnostic publication");
                        "could not write the diagnostics bundle"
                    }
                    _ => unreachable!("the case list is exhaustive"),
                };
                let Err(error) = resumed_testing(&mut Sink::default(), &args, When::Never, Styler::new(false)) else {
                    panic!("the native {case} failure must be returned");
                };
                assert!(error.to_string().contains(expected), "{case}: {error}");
            }
        }
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_configuration_warning_and_resume_gates_respect_every_effective_source() {
        let directory = workdir("run-config-conditions-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        fs::create_dir(root.join(".cargo")).unwrap();
        let foreign = root.join(".cargo/mutants.toml");
        let custom = root.join("custom.toml");
        fs::write(&custom, "jobs = 7\n").unwrap();
        for (present, no_config, explicit, warning) in [
            (false, false, false, false),
            (true, false, false, true),
            (true, true, false, false),
            (true, false, true, false),
        ] {
            if present {
                fs::write(&foreign, "not gamma syntax").unwrap();
            }
            let mut args = RunArgs::default();
            args.select.dir = root.to_owned();
            args.select.config.no_config = no_config;
            args.select.config.path = explicit.then(|| custom.clone());
            let mut host = Sink::default();
            configure(&mut host, &mut args, Styler::new(true)).unwrap();
            assert_eq!(host.err().contains(".cargo/mutants.toml is not supported or read"), warning);
            assert_eq!(
                host.err().contains('\u{1b}'),
                warning,
                "the supplied colored warning style must survive forwarding"
            );
            assert_eq!(args.measure.jobs, explicit.then_some(7));
            if present {
                fs::remove_file(&foreign).unwrap();
            }
        }
        fs::write(root.join("gamma.toml"), "jobs = 7\nfeatures = ['configured']\nmin-score = 82.5\n").unwrap();
        let mut args = RunArgs::default();
        args.select.dir = root.to_owned();
        args.resume_testing = true;
        configure(&mut Sink::default(), &mut args, Styler::new(false)).unwrap();
        assert_eq!(args.min_score, Some(82.5));
        assert_eq!(
            args.measure.jobs, None,
            "resumption reads reporting gates, not current execution configuration"
        );
        assert!(args.select.features.features.is_empty());
        fs::write(root.join("gamma.toml"), "[").unwrap();
        args.resume_testing = false;
        configure(&mut Sink::default(), &mut args, Styler::new(false)).expect_err("malformed ordinary configuration must return an error");
        fs::write(root.join("gamma.toml"), "workspace = true\n").unwrap();
        args.select.packages = vec!["subject".into()];
        configure(&mut Sink::default(), &mut args, Styler::new(false))
            .expect_err("a configured workspace cannot be combined with an explicit package selection");
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_report_advice_diagnostic_and_gate_entrypoints_retain_the_supplied_color_style() {
        let directory = workdir("run-style-forwarding-");
        let plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        let mut args = RunArgs::default();
        let session = session();
        let styler = Styler::new(true);
        for emitter in 0..8 {
            let mut host = Sink::default();
            match emitter {
                0 => emit_reports(&mut host, &args, &plan, None, Some(19), &[], styler).unwrap(),
                1 => report_stuck(&mut host, &["distinct failure".into()], styler).unwrap(),
                2 => emit_diag(&mut host, &args, &plan, Some(&session), Instant::now(), styler).unwrap(),
                3 => emit_diag_bundle(&mut host, &args, &plan, Some(&session), Instant::now(), styler).unwrap(),
                4 => emit_diag_bundle_to(
                    &mut host,
                    &args,
                    &plan,
                    None,
                    Instant::now(),
                    styler,
                    &plan.root.join("custom-diag.json"),
                )
                .unwrap(),
                5 => emit_advice(&mut host, &args, &plan, &session, Duration::from_secs(1), styler).unwrap(),
                6 => {
                    args.min_score = Some(80.0);
                    assert_eq!(ungraded(&mut host, &args, styler, false, false), EXIT_GATE_FAILED);
                }
                _ => {
                    args.diag = true;
                    emit_diag(&mut host, &args, &plan, Some(&session), Instant::now(), styler).unwrap();
                }
            }
            assert!(
                host.err().contains("\u{1b}["),
                "emitter {emitter} discarded its explicit color style: {}",
                host.err()
            );
        }
        args.diag = true;
        fails_at_every_line(1, |host| emit_diag(host, &args, &plan, Some(&session), Instant::now(), styler));
    }

    fn prepared_record(plan: &Plan, base: &Utf8Path) -> RecordPreparation {
        RecordPreparation {
            report_info: None,
            base: base.to_owned(),
            context: crate::discover::record_context(&crate::discover::RecordContext {
                toolchain: Some("survivor-record-context"),
                ..crate::discover::RecordContext::default()
            })
            .unwrap(),
            inputs: crate::discover::RunRecord::snapshot_with_external(&plan.root, base, &[], false),
            killers: crate::discover::Killers::default(),
        }
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_completed_and_incomplete_records_propagate_real_filesystem_and_phase_failures() {
        let directory = workdir("run-record-failures-");
        let mut plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        plan.mutants[0].outcome = Outcome::CompileError;
        let base = plan.root.join("blocked-cache");
        let prepared = prepared_record(&plan, &base);
        fs::write(&base, "retained cache collision").unwrap();
        publish_completed_record(prepared.clone(), &plan, None)
            .expect_err("a completed record cannot replace a file that blocks its cache directory");
        store_incomplete_convergence(&prepared, &plan).expect_err("incomplete convergence must return its cache publication error");
        let failed = exec::FailedMeasurement {
            plan: Some(plan.clone()),
            ..exec::FailedMeasurement::default()
        };
        store_failed_convergence(Some(&prepared), &failed)
            .expect_err("a failed measurement must propagate incomplete-record publication errors");
        assert_eq!(fs::read_to_string(&base).unwrap(), "retained cache collision");
        fs::remove_file(&base).unwrap();
        fs::create_dir(&base).unwrap();
        fs::write(base.join("testing-phase.json"), "{").unwrap();
        publish_completed_record(prepared, &plan, None)
            .expect_err("a malformed testing phase must prevent marking a completed record as finished");
        assert!(
            base.join("last-gamma-run.json").is_file(),
            "the phase error follows successful completed-record publication"
        );
        assert_eq!(fs::read_to_string(base.join("testing-phase.json")).unwrap(), "{");
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_missing_snapshot_source_is_an_incomplete_learning_error_and_advice_uses_any_resource_failure() {
        let directory = workdir("run-record-generation-");
        let mut plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        let base = plan.root.join("campaign");
        fs::remove_file(plan.root.join("src/lib.rs")).unwrap();
        let prepared = prepared_record(&plan, &base);
        assert_eq!(
            store_incomplete_convergence(&prepared, &plan).unwrap_err().message(),
            "could not construct incomplete compiler learning from the captured source generation"
        );
        assert!(!base.exists());
        fs::write(plan.root.join("src/lib.rs"), plan.files[0].source.as_ref().unwrap()).unwrap();
        plan.mutants[0].outcome = Outcome::Survived;
        plan.mutants[1].outcome = Outcome::Timeout;
        let stored = crate::discover::RunRecord::from_run(&plan.root, &plan.mutants, &prepared.context, &[plan.root.join("src")]);
        assert_eq!(postprocessing_advice(&plan.root, &stored, false), (false, false));
        assert!(
            postprocessing_advice(&plan.root, &stored, true).1,
            "one resource failure beside a nonresource outcome remains suppressible"
        );
        let snapshot = plan.root.join("unavailable-report-source");
        let failure = emit_reports_from(
            &mut Sink::default(),
            &RunArgs::default(),
            &plan,
            &ReportContents {
                retained_info: None,
                source_root: Some(&snapshot),
                advice: None,
                tests: None,
                dropped: &[],
            },
            Styler::new(false),
        )
        .expect_err("missing synchronized report source must return an error rather than panic");
        assert!(failure.to_string().contains("could not read campaign source"));
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_published_campaign_advice_requires_the_locator_to_resolve_back_to_that_generation() {
        let directory = workdir("run-locator-validation-");
        let mut plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        plan.mutants[0].outcome = Outcome::CompileError;
        let base = plan.root.join("campaign");
        let prepared = prepared_record(&plan, &base);
        fs::create_dir(&base).unwrap();
        crate::exec::mark_cache_owned_for_test(&plan.root, &base);
        fs::write(plan.root.join("Cargo.toml"), "[").unwrap();
        let advice = store_completed_record(prepared, &plan, None).unwrap();
        assert_eq!(
            advice,
            (false, false),
            "publishing a locator alone does not prove the requested workspace can resolve it"
        );
        let stored = crate::discover::RunRecord::load_required(&base).unwrap();
        assert!(
            crate::discover::Hints::record_promotion_is_useful(&plan.root, &stored),
            "the saved compiler ordering really would be promotable if its locator were reachable"
        );
    }

    fn quiet_events(host: &mut Sink) -> ConsoleEvents<'_, Sink> {
        ConsoleEvents {
            host,
            progress: Progress::new(false, Styler::new(false), Some(80)),
            dashboard: Dashboard::new(false, Some(80), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        }
    }

    #[test]
    fn survivor_empty_failure_artifacts_leave_the_original_cause_and_filesystem_untouched() {
        let directory = workdir("run-empty-failure-artifacts-");
        let plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        let args = RunArgs::default();
        let mut failure = Error::new("baseline failed before any test outcome");
        let mut host = Sink::default();
        emit_failure_artifacts(
            &mut quiet_events(&mut host),
            &args,
            &plan,
            None,
            &mut failure,
            &plan.root,
            Instant::now(),
        );
        assert_eq!(failure.message(), "baseline failed before any test outcome");
        assert!(host.err().is_empty());
        assert!(!Documents::resolve(&args, &plan.root).diag.exists());
    }

    #[test]
    #[cfg(feature = "system-tests")]
    fn survivor_partial_failure_artifact_writes_preserve_session_data_and_every_specific_warning() {
        let directory = workdir("run-partial-failure-artifacts-");
        let plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap());
        let args = RunArgs {
            artifact_dir: Some(plan.root.join("canonical")),
            ..RunArgs::default()
        };
        let artifacts = plan.root.join("nested");
        for blocked in [
            Documents::resolve(&args, &plan.root).diag,
            artifacts.join("baseline-failures/first/failure.json"),
            artifacts.join("baseline-failures/first/diags.json"),
        ] {
            fs::create_dir_all(blocked).unwrap();
        }
        let mut failure = Error::new("baseline failed")
            .with_nested_artifact(
                "baseline-failures/first".into(),
                "failure.json",
                serde_json::json!({"failure": "first"}),
            )
            .with_nested_artifact(
                "baseline-failures/second".into(),
                "failure.json",
                serde_json::json!({"failure": "second"}),
            );
        let mut measured = session();
        measured.tests = Some(73);
        let mut host = Sink::default();
        emit_failure_artifacts(
            &mut quiet_events(&mut host),
            &args,
            &plan,
            Some(&measured),
            &mut failure,
            &artifacts,
            Instant::now(),
        );
        assert_eq!(
            failure.message(),
            format!(
                "baseline measurement failed due to 2 test failures; only 1 of 2 failure files could be written under {}",
                platform_path(&artifacts.join("baseline-failures"))
            )
        );
        for warning in [
            "could not preserve the canonical diagnostics bundle after the baseline failure:",
            "could not preserve the baseline failure record:",
            "could not preserve the diagnostics bundle after the baseline failure:",
        ] {
            assert_eq!(host.err().matches(warning).count(), 1, "{}", host.err());
        }
        let record: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(artifacts.join("baseline-failures/second/failure.json")).unwrap()).unwrap();
        assert_eq!(record, serde_json::json!({"failure": "second"}));
        let diagnostic: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(artifacts.join("baseline-failures/second/diags.json")).unwrap()).unwrap();
        assert_eq!(diagnostic["build"]["tests"], 73);
        assert!(artifacts.join("baseline-failures/first/failure.json").is_dir());
        assert!(artifacts.join("baseline-failures/first/diags.json").is_dir());
    }

    #[derive(Debug, Default)]
    struct ClosedResults {
        diagnostics: Vec<u8>,
    }

    impl Host for ClosedResults {
        fn output(&mut self) -> impl Write {
            Broken
        }

        fn error(&mut self) -> impl Write {
            &mut self.diagnostics
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        fn is_terminal(&self) -> bool {
            false
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        fn terminal_width(&self) -> Option<u16> {
            None
        }
    }

    #[test]
    fn a_flaky_only_gate_is_evaluated_by_an_in_budget_flaky_outcome() {
        let args = RunArgs {
            max_flaky: Some(1),
            ..RunArgs::default()
        };
        let mut host = Sink::default();

        let code = ungraded(&mut host, &args, Styler::new(false), true, false);

        assert_eq!(code, EXIT_OK, "{}", host.err());
        assert!(host.err().is_empty(), "{}", host.err());
    }

    #[test]
    fn disabled_incremental_mode_suppresses_missing_hints_advice() {
        let dir = workdir("run-missing-hints-");
        let root = Utf8Path::from_path(dir.path()).expect("UTF-8 work directory");

        assert!(missing_hints(exec::IncrementalMode::Build, root));
        assert!(!missing_hints(exec::IncrementalMode::No, root));
    }

    #[test]
    fn disabled_incremental_and_dry_runs_prepare_no_cache_context() {
        let disabled = RunArgs {
            incremental: Some(exec::IncrementalMode::No),
            ..RunArgs::default()
        };
        let dry = RunArgs {
            dry_run: true,
            ..RunArgs::default()
        };

        assert!(incremental_context(&disabled).is_none());
        assert!(incremental_context(&dry).is_none());
        assert!(
            incremental_context(&RunArgs::default()).is_some(),
            "an ordinary run has the toolchain identity needed for incremental reuse"
        );
    }

    #[test]
    fn incremental_no_disables_context_reuse_not_authoritative_historical_exclusions() {
        let args = RunArgs {
            incremental: Some(exec::IncrementalMode::No),
            ..RunArgs::default()
        };
        assert!(incremental_context(&args).is_none());
        let directory = crate::testing::workdir("incremental-independent-exclusions-");
        let mut plan = plan_at(Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap());
        let base = plan.root.join("historical-exclusions");
        fs::create_dir(&base).unwrap();
        let mut inventory = crate::discover::CompilerExclusions::load(&base, &plan.root).unwrap();
        let mutant = &mut plan.mutants[0];
        mutant.outcome = Outcome::Pending;
        mutant.ordinal = 1;
        inventory.remember(core::slice::from_ref(mutant), &crate::HashSet::from_iter([1]), |_| "E0308".into());
        inventory.store(&base).unwrap();
        let inventory = crate::discover::CompilerExclusions::load(&base, &plan.root).unwrap();
        assert_eq!(inventory.exclude(&mut plan.mutants), 1);
        assert_eq!(plan.mutants[0].outcome, Outcome::CompileError);
        assert_eq!(plan.mutants[0].ordinal, 0);
    }

    #[test]
    fn disabled_incremental_still_persists_postprocessing_state_without_adoption() {
        let args = RunArgs {
            incremental: Some(exec::IncrementalMode::No),
            ..RunArgs::default()
        };
        let (record_context, reusable) = postprocessing_contexts(&args, None).expect("real runs prepare a record");
        assert!(record_context.states(crate::discover::Term::Environment));
        let (plan, _directory) = private_plan();
        let base = plan.root.join("campaign-state");
        let _locks = exec::claim_cache(&plan.root, &plan.root.join("target"), Some(&base)).expect("owned campaign cache");
        let inputs = crate::discover::RunRecord::snapshot_with_external(&plan.root, &base, &[], false);
        let prepared = RecordPreparation {
            report_info: None,
            base: base.clone(),
            context: record_context,
            inputs,
            killers: crate::discover::Killers::default(),
        };
        let killer = crate::discover::Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: "tests::caught".to_owned(),
        };
        let mut learning = crate::exec::Killers::default();
        learning.record(plan.mutants[0].id.clone(), killer.clone());

        let (promotable, _suppressible) = store_completed_record(prepared, &plan, Some(&learning)).expect("completed campaign publication");
        let stored = crate::discover::RunRecord::load_required(&base).expect("completed campaign state");

        assert!(reusable.is_none(), "disabled incremental mode must not admit prior outcomes");
        assert!(promotable, "newly learned probes remain promotable");
        assert_eq!(stored.outcomes().len(), plan.mutants.len());
        assert_eq!(stored.probes().get(&plan.mutants[0].id), Some(&killer));
    }

    #[test]
    fn post_convergence_failure_publishes_run_level_compiler_learning() {
        let (mut plan, _directory) = private_plan();
        plan.mutants[0].outcome = Outcome::CompileError;
        plan.mutants[0].note = Some("E0308: mismatched types".to_owned());
        let base = plan.root.join("target/campaign-state");
        fs::create_dir_all(&base).expect("campaign base");
        let prepared = RecordPreparation {
            report_info: None,
            inputs: crate::discover::RunRecord::snapshot_with_external(&plan.root, &base, &[], false),
            base: base.clone(),
            context: crate::discover::record_context(&crate::discover::RecordContext {
                toolchain: Some("test"),
                ..crate::discover::RecordContext::default()
            })
            .expect("record context"),
            killers: crate::discover::Killers::default(),
        };
        let failed = exec::FailedMeasurement {
            plan: Some(plan),
            ..exec::FailedMeasurement::default()
        };

        store_failed_convergence(Some(&prepared), &failed).expect("failed campaign learning");

        let stored = crate::discover::RunRecord::load_incomplete(&base);
        assert_eq!(stored.outcomes().len(), 1);
        assert_eq!(stored.ordering().len(), 1);
    }

    #[test]
    fn completed_campaign_atomically_clears_stale_learning() {
        let plan = plan();
        let base = plan.root.join("campaign-state");
        let stale = core::iter::once((
            plan.mutants[0].id.clone(),
            crate::discover::Killer {
                package: "subject".to_owned(),
                target: "lib".to_owned(),
                test: "tests::stale".to_owned(),
            },
        ))
        .collect();
        crate::discover::RunRecord::store_probes(&base, &stale);
        let prepared = RecordPreparation {
            report_info: None,
            inputs: crate::discover::RunRecord::snapshot_with_external(&plan.root, &base, &[], false),
            base: base.clone(),
            context: crate::discover::record_context(&crate::discover::RecordContext {
                toolchain: Some("test"),
                ..crate::discover::RecordContext::default()
            })
            .expect("record context"),
            killers: crate::discover::Killers::default(),
        };

        store_completed_record(prepared, &plan, Some(&crate::exec::Killers::default())).expect("completed campaign publication");

        assert!(
            crate::discover::RunRecord::load_required(&base)
                .expect("completed campaign state")
                .probes()
                .is_empty(),
            "the completed campaign's empty learning replaces stale probes"
        );
    }

    #[test]
    fn completed_campaign_records_the_killing_tests_source_file() {
        let (mut plan, _directory) = private_plan();
        let source = format!("{}\n#[test]\nfn caught() {{}}\n", plan.files[0].source.as_deref().expect("source"));
        fs::write(&plan.files[0].absolute, &source).expect("source with killing test");
        plan.files[0].source = Some(source.clone());
        let _previous = plan
            .digests
            .insert(plan.files[0].path.clone(), crate::discover::digest(source.as_bytes()));
        plan.mutants[0].outcome = Outcome::Killed;
        plan.mutants[0].killed_by = Some("caught".to_owned());
        let base = plan.root.join("campaign-state");
        let _locks = exec::claim_cache(&plan.root, &plan.root.join("target"), Some(&base)).expect("owned campaign cache");
        let prepared = RecordPreparation {
            report_info: None,
            inputs: crate::discover::RunRecord::snapshot_with_external(&plan.root, &base, &[], false),
            base: base.clone(),
            context: crate::discover::ContextDigest::default(),
            killers: crate::discover::Killers::scan(&[plan.files[0].absolute.clone()]),
        };

        store_completed_record(prepared, &plan, None).expect("completed campaign publication");

        let record: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(base.join("last-gamma-run.json")).expect("completed record")).expect("record JSON");
        let recorded = record["files"]
            .as_array()
            .expect("recorded files")
            .iter()
            .flat_map(|file| file["mutants"].as_array().expect("recorded mutants"))
            .find(|entry| entry["id"] == plan.mutants[0].id.as_str())
            .expect("killed mutant");

        assert_eq!(recorded["killer_file"], "src/lib.rs");
    }

    #[test]
    fn completed_resource_outcomes_survive_an_incomplete_or_changed_cache_snapshot() {
        let (mut plan, _directory) = private_plan();
        plan.mutants[0].outcome = Outcome::Timeout;
        plan.mutants[1].outcome = Outcome::OutOfMemory;
        let base = plan.root.join("campaign-state");
        let _locks = exec::claim_cache(&plan.root, &plan.root.join("target"), Some(&base)).expect("owned campaign cache");
        assert!(exec::remember_campaign_base(&plan.root, &base), "owned campaign locator");
        assert_eq!(exec::campaign_base_from_state(&plan.root, None), Some(base.clone()));
        let inputs = crate::discover::RunRecord::snapshot_with_external(&plan.root, &base, &[], true);
        let prepared = RecordPreparation {
            report_info: None,
            base: base.clone(),
            context: crate::discover::ContextDigest::default(),
            inputs,
            killers: crate::discover::Killers::default(),
        };
        let probes = core::iter::once((
            plan.mutants[0].id.clone(),
            crate::discover::Killer {
                package: "subject".to_owned(),
                target: "lib".to_owned(),
                test: "tests::caught".to_owned(),
            },
        ))
        .collect();
        crate::discover::RunRecord::store_probes(&base, &probes);
        fs::write(&plan.files[0].absolute, "pub fn changed_during_campaign() {}\n").expect("change live source");

        let (promotable, suppressible) =
            store_completed_record(prepared, &plan, None).expect("completed campaign state does not require cache eligibility");
        let stored = crate::discover::RunRecord::load_required(&base).expect("completed campaign state");
        let outcomes: crate::HashMap<_, _> = stored.iter().map(|(id, outcome)| (id.to_owned(), outcome)).collect();

        assert!(promotable, "the intermediate exact learning is preserved");
        assert!(suppressible, "the completed ledger contains resource outcomes");
        assert_eq!(
            postprocessing_advice(&plan.root, &stored, false),
            (false, false),
            "commands that cannot resolve the record must not be recommended"
        );
        assert_eq!(outcomes.get(plan.mutants[0].id.as_str()), Some(&Outcome::Timeout));
        assert_eq!(outcomes.get(plan.mutants[1].id.as_str()), Some(&Outcome::OutOfMemory));
        assert_eq!(stored.probes().get(&plan.mutants[0].id), probes.get(&plan.mutants[0].id));
    }

    #[test]
    fn completed_campaign_rejects_a_snapshot_from_before_discovery() {
        let mut plan = plan();
        let base = plan.root.join("campaign-state");
        let prepared = RecordPreparation {
            report_info: None,
            inputs: crate::discover::RunRecord::snapshot_with_external(&plan.root, &base, &[], false),
            base,
            context: crate::discover::ContextDigest::default(),
            killers: crate::discover::Killers::default(),
        };
        let changed = "pub fn changed() -> bool { true }\n";
        fs::write(&plan.files[0].absolute, changed).expect("changed source");
        plan.files[0].source = Some(changed.to_owned());
        let _previous = plan
            .digests
            .insert(plan.files[0].path.clone(), crate::discover::digest(changed.as_bytes()));

        let error = store_completed_record(prepared, &plan, None).expect_err("stale pre-discovery snapshot must not be published");

        assert!(error.to_string().contains("captured source generation"), "{error}");
    }

    #[test]
    fn cache_claiming_requires_persisted_state_and_a_real_run() {
        assert!(should_claim_cache(true, false));
        assert!(!should_claim_cache(false, false));
        assert!(!should_claim_cache(true, true));
        assert!(!should_claim_cache(false, true));
    }

    #[test]
    fn analysis_progress_names_its_phase_and_subject_before_discovery() {
        let dir = workdir("run-analysis-status-");
        let missing = Utf8PathBuf::from_path_buf(dir.path().join("missing")).expect("UTF-8 path");
        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: missing,
                ..crate::commands::SelectArgs::default()
            },
            dry_run: true,
            ..RunArgs::default()
        };
        let mut host = Sink::default();

        assert!(
            measured(&mut host, &args, When::Always, Styler::new(false)).is_err(),
            "a missing workspace must fail discovery"
        );

        assert!(host.err().contains("Analyzing the workspace"), "{}", host.err());
    }

    #[test]
    fn analysis_progress_completes_after_discovery() {
        let dir = workdir("run-analysis-complete-");
        let root = subject_root(&dir);
        fs::write(root.join("src/lib.rs"), "pub fn less(a: i32, b: i32) -> bool { a < b }\n").expect("lib");
        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                ..crate::commands::SelectArgs::default()
            },
            dry_run: true,
            ..RunArgs::default()
        };
        let mut host = Sink::default();

        measured(&mut host, &args, When::Always, Styler::new(false)).expect("workspace discovery");

        assert!(host.err().contains("Analyzing the workspace"), "{}", host.err());
        assert!(host.err().contains("Analyzed the workspace"), "{}", host.err());
    }

    #[test]
    fn test_arguments_concatenate_both_sources_in_command_line_order() {
        let args = RunArgs {
            measure: MeasureArgs {
                cargo_test_args: vec!["--nocapture".to_owned(), "--exact".to_owned()],
                test_args: vec!["subject::works".to_owned(), "--ignored".to_owned()],
                ..MeasureArgs::default()
            },
            ..RunArgs::default()
        };

        assert_eq!(test_arguments(&args), ["--nocapture", "--exact", "subject::works", "--ignored"]);
    }

    #[test]
    fn discovery_resolves_external_inputs_only_for_cache_provenance() {
        let dir = workdir("run-cache-inputs-");
        let container = Utf8Path::from_path(dir.path()).expect("UTF-8 work directory");
        let root = container.join("workspace");
        let dependency = container.join("dependency");
        crate::testing::write_project(
            root.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
                 [dependencies]\ndependency = { path = \"../dependency\" }\n\n[workspace]\n",
                ),
                ("src/lib.rs", "pub fn subject() {}\n"),
            ],
        );
        crate::testing::write_fixture(
            dependency.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"dependency\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
                ),
                ("src/lib.rs", "pub fn dependency() {}\n"),
            ],
        );
        let select = crate::commands::SelectArgs {
            dir: root,
            ..crate::commands::SelectArgs::default()
        };
        let cargo = exec::CargoOptions::default();

        let uncached = crate::discover::Survey::for_build_with_cache_inputs(&select, None, &cargo, false).expect("uncached survey");
        let cached = crate::discover::Survey::for_build_with_cache_inputs(&select, None, &cargo, true).expect("cached survey");

        assert!(uncached.external_inputs().is_empty());
        assert_eq!(
            cached.external_inputs(),
            &[crate::paths::physical(&dependency).expect("dependency path")]
        );
    }

    fn plan() -> Plan {
        let dir = workdir("run-plan-");
        let root = Utf8PathBuf::from_path_buf(dir.keep()).expect("utf8");
        plan_at(root)
    }

    fn private_plan() -> (Plan, tempfile::TempDir) {
        let dir = private_system_tempdir("secure-run-plan-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        (plan_at(root), dir)
    }

    fn plan_at(root: Utf8PathBuf) -> Plan {
        let src = root.join("src");
        let source = include_str!("../../tests/fixtures/completed-recovery/lib.rs");
        crate::testing::write_project(
            root.as_std_path(),
            &[
                ("Cargo.toml", include_str!("../../tests/fixtures/completed-recovery/Cargo.toml")),
                ("src/lib.rs", source),
            ],
        );
        let absolute = src.join("lib.rs");
        let start = source.find("a < b").expect("span");

        let mut digests = crate::HashMap::default();
        let _previous = digests.insert(Utf8PathBuf::from("src/lib.rs"), crate::discover::digest(source.as_bytes()));

        Plan {
            skipped: Vec::new(),
            digests,
            root,
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute,
                package: "subject".to_owned(),
                source: Some(source.to_owned()),
            }],
            mutants: vec![mutant(start, Outcome::Survived, 120), mutant(start, Outcome::NoCoverage, 80)],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    fn mutant(start: usize, outcome: Outcome, elapsed_ms: u64) -> Mutant {
        Mutant {
            id: format!("m{elapsed_ms}").into(),
            span: start..start + 5,
            column: start + 1,
            item_path: ("subject::less".to_owned()).into(),
            original: "a < b".to_owned().into(),
            replacement: "a <= b".to_owned().into(),
            outcome,
            elapsed_ms,
            ..fixtures::mutant()
        }
    }

    fn expecting(outcome: Outcome, killed: bool) -> Mutant {
        Mutant {
            expectation: Some(crate::model::Expectation {
                killed,
                line: 3,
                reason: None,
            }),
            ..mutant(0, outcome, 1)
        }
    }

    /// Naming only `--memory-limit` implies enforcement without the caller having to also spell
    /// out `--memory enforce`; failing to imply it would mean a ceiling that was asked for
    /// silently does nothing, which is the exact surprise the memory guard exists to prevent.
    #[test]
    fn a_memory_limit_alone_implies_enforcement() {
        let args = RunArgs {
            measure: MeasureArgs {
                memory_limit: Some(1024),
                ..MeasureArgs::default()
            },
            ..Default::default()
        };

        let policy = memory_policy(&args);

        assert_eq!(policy.control, exec::MemoryControl::Enforce);
        assert_eq!(policy.demand, exec::Demand::Stated);
    }

    /// Naming only `--baseline-memory-limit` implies measurement rather than enforcement, because
    /// a ceiling on the baseline alone is a request to observe, not to kill the run over.
    #[test]
    fn a_baseline_memory_limit_alone_implies_measurement() {
        let args = RunArgs {
            measure: MeasureArgs {
                baseline_memory_limit: Some(1024),
                ..MeasureArgs::default()
            },
            ..Default::default()
        };

        let policy = memory_policy(&args);

        assert_eq!(policy.control, exec::MemoryControl::Measure);
        assert_eq!(policy.demand, exec::Demand::Stated);
    }

    #[test]
    fn the_default_memory_policy_is_inherited_with_the_exact_default_headroom() {
        let policy = memory_policy(&RunArgs::default());

        assert_eq!(policy.demand, exec::Demand::Inherited);
        assert_eq!(policy.headroom, exec::DEFAULT_HEADROOM);
    }

    #[test]
    fn run_metadata_stamps_the_identity_scheme_and_preserves_optional_inputs() {
        let info = run_info(&RunArgs::default(), Some(17), &["unavailable".to_owned()]);

        assert_eq!(info.tests, Some(17));
        assert_eq!(info.mutant_id_version, Some(crate::model::MUTANT_ID_VERSION));
        assert!(!info.merged);
        assert!(info.not_built.is_none());
        assert_eq!(info.dropped_test_packages, ["unavailable"]);
        assert!(info.merge_provenance.is_none());
    }

    #[test]
    fn resumed_report_provenance_preserves_shard_age_and_other_shards() {
        let retained = crate::fixtures::report_with(Some((1, 3)), 100, Vec::new()).config.unwrap();
        let info = resumed_report_info(Some(&retained));
        let info = report_info(&RunArgs::default(), Some(&info), Some(17), &["dropped".to_owned()]);
        let mut expected = retained;
        expected.tests = Some(17);
        expected.dropped_test_packages = vec!["dropped".to_owned()];
        assert_eq!(serde_json::to_value(&info).unwrap(), serde_json::to_value(expected).unwrap());
        let mut resumed = crate::fixtures::report_with(None, 300, vec![crate::fixtures::mutant_result_at("older", 1, "Killed")]);
        resumed.config = Some(info);
        let other = crate::fixtures::report_with(Some((0, 3)), 110, vec![crate::fixtures::mutant_result_at("other", 2, "Survived")]);
        let merged = crate::merge::merge(&[("resumed".to_owned(), resumed), ("other".to_owned(), other)], 300, Some(50));
        assert_eq!(
            (merged.withdrawn, merged.stale, merged.fresh, merged.shard_count),
            (0, 2, 0, Some(3))
        );
        assert_eq!(merged.shards_seen, std::collections::BTreeSet::from([0, 1]));
    }

    #[test]
    fn legacy_resume_reports_cannot_refresh_evidence_or_withdraw_other_populations() {
        let info = resumed_report_info(None);
        assert_eq!(info.started_at, 0);
        assert!(info.merged);
        assert!(info.shard.is_none());
        let mut resumed = crate::fixtures::report_with(None, 300, vec![crate::fixtures::mutant_result_at("older", 1, "Killed")]);
        resumed.config = Some(info);
        let other = crate::fixtures::report_with(Some((0, 3)), 110, vec![crate::fixtures::mutant_result_at("other", 2, "Survived")]);
        let merged = crate::merge::merge(&[("legacy".to_owned(), resumed), ("other".to_owned(), other)], 300, Some(50));
        assert_eq!((merged.withdrawn, merged.stale, merged.fresh), (0, 2, 0));
    }

    #[test]
    fn timestamps_before_the_unix_epoch_use_zero() {
        let before_epoch = UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .expect("SystemTime represents instants before the Unix epoch");

        assert_eq!(seconds_since_epoch(before_epoch), 0);
        assert_eq!(seconds_since_epoch(UNIX_EPOCH + Duration::from_secs(7)), 7);
    }

    #[test]
    fn pending_mutants_fail_only_an_enabled_score_gate() {
        let gated = RunArgs {
            min_score: Some(0.0),
            ..RunArgs::default()
        };

        assert!(pending_fails_gate(&gated, 1));
        assert!(!pending_fails_gate(&gated, 0));
        assert!(!pending_fails_gate(&RunArgs::default(), 1));
    }

    /// Resource exhaustion satisfies `expect_survived`, not `expect_killed`, because no assertion
    /// rejected the mutant.
    #[test]
    fn every_scoring_outcome_is_judged_by_assertion_detection() {
        let mutants = vec![
            expecting(Outcome::Killed, true),
            expecting(Outcome::Survived, false),
            expecting(Outcome::Timeout, false),
            expecting(Outcome::OutOfMemory, false),
        ];

        assert!(broken_expectations(&mutants).is_empty());
    }

    #[test]
    fn an_expectation_that_does_not_hold_is_reported_with_what_was_wanted() {
        let mutants = vec![expecting(Outcome::Survived, true), expecting(Outcome::Killed, false)];
        let broken = broken_expectations(&mutants);

        assert_eq!(broken.len(), 2);
        assert_eq!(broken[0].1, "killed");
        assert_eq!(broken[1].1, "survived");
    }

    #[test]
    fn an_expectation_on_a_mutant_that_never_ran_is_not_judged() {
        // A mutant that failed to compile or that nothing reaches is not evidence about the suite
        // either way, so holding the author to a claim about it would fail runs for no reason.
        let mutants = vec![
            expecting(Outcome::CompileError, true),
            expecting(Outcome::Ignored, true),
            expecting(Outcome::Pending, true),
        ];

        assert!(broken_expectations(&mutants).is_empty());
    }

    #[test]
    fn an_uncovered_mutant_is_judged_against_its_expectation() {
        // Nothing reaching a site is exactly what `expect_survived` claims, and the opposite of what
        // `expect_killed` claims, so both are real answers.
        assert!(broken_expectations(&[expecting(Outcome::NoCoverage, false)]).is_empty());
        assert_eq!(broken_expectations(&[expecting(Outcome::NoCoverage, true)]).len(), 1);
    }

    #[test]
    fn a_mutant_with_no_expectation_is_never_reported() {
        assert!(broken_expectations(&[mutant(0, Outcome::Survived, 1)]).is_empty());
    }

    fn session() -> Session {
        Session {
            census: Vec::new(),
            baseline: Duration::from_millis(20),
            baseline_wall: Duration::from_millis(20),
            tests: None,
            quiet: Duration::from_millis(10),
            stall: Some(Duration::from_millis(30)),
            build: Duration::from_millis(40),
            metered: false,
            unbounded: None,
            withdrawn: 0,
            rounds: 1,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            peak: None,
            scratch: Utf8PathBuf::new(),
            filtered: 0,
            widened: false,
            ordering: exec::OrderingHints::default(),
            phases: exec::Phases::default(),
        }
    }

    #[test]
    fn footer_stats_use_sweep_launches_and_confirmed_hint_hits() {
        let mut measured = session();
        measured.phases.sweep = Some(exec::SweepCost {
            launches: 821,
            exact_hits: 384,
            inferred_hits: 2,
            ..exec::SweepCost::default()
        });

        let footer = stats(&measured, Duration::from_mins(42));

        assert_eq!(footer.elapsed, Duration::from_mins(42));
        assert_eq!(footer.binaries, 0);
        assert_eq!(footer.launches, 821);
        assert_eq!(footer.successful_hints, 386);
    }

    #[test]
    fn advice_uses_elapsed_wall_time_for_concurrent_baselines_and_sparse_workers() {
        let mut measured = session();
        measured.build = Duration::from_secs(7);
        measured.baseline = Duration::from_secs(5);
        measured.baseline_wall = Duration::from_secs(2);
        let mut args = RunArgs::default();

        args.measure.jobs = Some(8);

        let text = advice_markdown(&args, &plan(), &measured, Duration::from_secs(20), crate::advise::Layout::Document);

        assert!(text.contains("| Build | 7.0s | 35% |"), "{text}");
        assert!(text.contains("| Baseline | 2.0s | 10% |"), "{text}");
        assert!(text.contains("| Testing mutants | 11.0s | 55% |"), "{text}");
        assert!(text.contains("45% of the run was the build and baseline"), "{text}");
    }

    /// Every line the report writer emits is checked to propagate a closed stream.
    #[test]
    fn a_closed_stream_stops_the_report_writer_at_whichever_line_it_reached() {
        let dir = workdir("run-reports-closed-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let plan = plan();

        // A run that has written a SARIF log, a JSON report and an HTML report emits several
        // lines; a pipe that closes partway through must fail the command rather than be ignored,
        // or CI would record a success for reports nobody received.
        fails_at_every_line(3, |host| {
            let args = RunArgs {
                artifact_dir: Some(root.clone()),
                annotations: crate::ci::Annotations::None,
                ..Default::default()
            };

            emit_reports(host, &args, &plan, None, None, &[], Styler::new(false))
        });
    }

    /// Annotations go to the results stream, where a closed consumer is successful completion.
    #[test]
    fn a_closed_results_stream_ends_annotation_writing_successfully() {
        let plan = plan();
        let args = RunArgs {
            annotations: crate::ci::Annotations::Github,
            ..Default::default()
        };

        emit_ci(&mut ClosedResults::default(), &args, &plan, None, Styler::new(false)).expect("closed results pipe");
    }

    #[test]
    fn reports_write_json_html_sarif_annotations_and_summary() {
        let dir = workdir("run-reports-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let summary = root.join("summary.md");
        let args = RunArgs {
            artifact_dir: Some(root.join("reports")),
            annotations: crate::ci::Annotations::Github,
            ..Default::default()
        };

        let mut host = Sink::default().with_env("GITHUB_STEP_SUMMARY", summary.as_str());
        let plan = plan();

        emit_reports(&mut host, &args, &plan, Some("embedded advice"), None, &[], Styler::new(false)).expect("reports");

        let out = String::from_utf8(host.out).expect("utf-8");
        let err = String::from_utf8(host.err).expect("utf-8");
        let summary_text = fs::read_to_string(summary).expect("summary");

        let documents = Documents::resolve(&args, &plan.root);
        let report: crate::elements::Report =
            serde_json::from_str(&fs::read_to_string(&documents.json).expect("JSON report")).expect("valid report");

        assert!(documents.json.exists());
        assert!(documents.html.exists());
        assert!(documents.sarif.exists());
        assert!(report.config.is_some(), "a run report must retain its run metadata");
        assert!(out.contains("::warning"), "{out}");
        assert!(err.contains("Wrote"), "{err}");
        assert!(summary_text.contains("embedded advice"), "{summary_text}");
    }

    #[test]
    fn automatic_annotations_detect_the_github_actions_variable() {
        assert_eq!(GITHUB_ACTIONS, "GITHUB_ACTIONS");

        let args = RunArgs {
            annotations: crate::ci::Annotations::Auto,
            ..RunArgs::default()
        };
        let plan = plan();
        let mut host = Sink::default().with_env(GITHUB_ACTIONS, "true");

        emit_ci(&mut host, &args, &plan, None, Styler::new(false)).expect("CI output");

        assert!(host.out().contains("::warning"), "{}", host.out());
    }

    /// A run that names no report path still gets every report under the gamma directory.
    ///
    /// This is the whole point of the change that made them default: the run that answers a
    /// question about your suite is the one you cannot afford to repeat because the flag was
    /// forgotten.
    #[test]
    fn a_run_that_asks_for_nothing_still_writes_all_reports_under_the_gamma_directory() {
        let mut host = Sink::default();
        let plan = plan();
        let args = RunArgs::default();
        let base = plan.root.join("target/cargo-gamma");

        emit_reports(&mut host, &args, &plan, None, None, &[], Styler::new(false)).expect("reports");

        assert!(base.join("gamma-report.json").exists());
        assert!(base.join("gamma-report.html").exists());
        assert!(base.join("gamma-report.sarif").exists());
        let output = host.err();
        for path in [
            base.join("gamma-report.json"),
            base.join("gamma-report.html"),
            base.join("gamma-report.sarif"),
        ] {
            let rendered = platform_path(&path);
            assert!(
                output.contains(&format!("Wrote  : {rendered}")),
                "{path} was not announced with a native aligned label in:\n{output}",
            );
        }
    }

    #[test]
    fn default_document_names_identify_their_gamma_artifacts() {
        let plan = plan();
        let base = plan.root.join("target/cargo-gamma");
        let documents = Documents::resolve(&RunArgs::default(), &plan.root);

        assert_eq!(documents.json, base.join("gamma-report.json"));
        assert_eq!(documents.html, base.join("gamma-report.html"));
        assert_eq!(documents.advice, base.join("gamma-perf-advice.md"));
        assert_eq!(documents.diag, base.join("gamma-diagnostics.json"));
        assert_eq!(documents.sarif, base.join("gamma-report.sarif"));
    }

    /// Naming an artifact directory moves every artifact and leaves the default directory alone.
    #[test]
    fn naming_an_artifact_directory_moves_every_report() {
        let dir = workdir("run-report-override-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();
        let plan = plan();
        let base = plan.root.join("target/cargo-gamma");
        let artifact_dir = root.join("elsewhere");
        let args = RunArgs {
            artifact_dir: Some(artifact_dir.clone()),
            ..Default::default()
        };

        emit_reports(&mut host, &args, &plan, None, None, &[], Styler::new(false)).expect("reports");

        assert!(artifact_dir.join("gamma-report.json").exists());
        assert!(artifact_dir.join("gamma-report.html").exists());
        assert!(artifact_dir.join("gamma-report.sarif").exists());
        assert!(!base.join("gamma-report.json").exists());
        assert!(!base.exists());
    }

    /// Cache relocation is internal state and must not move user-facing reports.
    #[test]
    fn a_cache_directory_does_not_move_the_default_reports() {
        let dir = workdir("run-report-scratch-");
        let cache = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();
        let plan = plan();
        let args = RunArgs {
            measure: MeasureArgs {
                cache_dir: Some(cache.clone()),
                ..Default::default()
            },
            ..Default::default()
        };

        emit_reports(&mut host, &args, &plan, None, None, &[], Styler::new(false)).expect("reports");

        assert!(plan.root.join("target/cargo-gamma/gamma-report.json").exists());
        assert!(!cache.join("gamma-report.json").exists());
    }

    /// A default report location that cannot be written fails the run rather than being skipped.
    ///
    /// Defaulting the reports made this reachable without anyone asking for it: before, a report
    /// only existed because a flag named it, and a caller who named a path was there to see it
    /// fail. Now every run writes three files nobody requested, and the temptation to make those
    /// writes best-effort is real. It must be resisted — a run that spent an hour and produced no
    /// report has failed, and exiting zero would hide that behind a score printed to a terminal
    /// that CI throws away.
    ///
    /// The block is a *directory* standing where each default report file belongs, which the
    /// writer cannot rename a file onto. Blocking one artifact at a time is what makes the test
    /// say which write was dropped, rather than merely that some write was.
    #[test]
    fn a_default_report_path_that_cannot_be_written_fails_the_run() {
        for blocked in [
            "gamma-report.json",
            "gamma-report.html",
            "gamma-report.sarif",
            "gamma-perf-advice.md",
        ] {
            let dir = workdir("run-report-blocked-");
            let artifact_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
            let plan = plan();
            let base = artifact_dir.clone();

            fs::create_dir_all(base.join(blocked).as_std_path()).expect("blocker");

            let mut host = Sink::default();
            let args = RunArgs {
                artifact_dir: Some(artifact_dir),
                measure: MeasureArgs {
                    jobs: Some(0),
                    ..Default::default()
                },
                ..Default::default()
            };

            let error = if blocked == "gamma-perf-advice.md" {
                emit_advice(&mut host, &args, &plan, &session(), Duration::from_secs(20), Styler::new(false))
            } else {
                emit_reports(&mut host, &args, &plan, None, None, &[], Styler::new(false))
            }
            .expect_err("a blocked default artifact path is an error");

            assert!(error.to_string().contains(blocked), "{blocked}: {error}");
        }
    }

    /// When no advice was generated, the job summary still gets written with the score alone; a
    /// summary that only appeared once advice existed would make the panel disappear on exactly
    /// the runs where nothing needed following up.
    #[test]
    fn a_step_summary_omits_advice_when_none_was_given() {
        let dir = workdir("run-summary-no-advice-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let summary = root.join("summary.md");
        let args = RunArgs {
            annotations: crate::ci::Annotations::Github,
            ..Default::default()
        };

        let mut host = Sink::default().with_env("GITHUB_STEP_SUMMARY", summary.as_str());
        let plan = plan();

        emit_ci(&mut host, &args, &plan, None, Styler::new(false)).expect("ci");

        let summary_text = fs::read_to_string(summary).expect("summary");

        assert!(!summary_text.is_empty(), "{summary_text}");
    }

    /// A broken job-summary path is auxiliary: it is warned about only after the durable reports
    /// have been written, and cannot turn a completed run into one that "could not proceed".
    #[test]
    fn an_empty_step_summary_path_warns_after_writing_the_reports() {
        let plan = plan();
        let base = plan.root.join("target/cargo-gamma");
        let args = RunArgs {
            annotations: crate::ci::Annotations::Github,
            ..Default::default()
        };

        let mut host = Sink::default().with_env("GITHUB_STEP_SUMMARY", "");

        emit_reports(&mut host, &args, &plan, None, None, &[], Styler::new(false)).expect("primary reports");

        assert!(base.join("gamma-report.json").exists());
        assert!(base.join("gamma-report.html").exists());
        assert!(host.err().contains("could not write the job summary"), "{}", host.err());
    }

    /// A closed results stream that stops the SARIF truncation note must fail the command, just
    /// like every other line the run writes; letting the note alone go unenforced would mean the
    /// one line that says the log is incomplete is exactly the one line allowed to vanish.
    #[test]
    fn a_closed_stream_is_reported_by_the_sarif_truncation_note() {
        let dir = workdir("run-sarif-truncation-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut plan = plan();
        let survivor = plan.mutants[0].clone();

        plan.mutants = (0..=crate::ci::sarif::SARIF_LIMIT)
            .map(|index| {
                let mut mutant = survivor.clone();

                mutant.id = format!("m{index}").into();
                mutant
            })
            .collect();

        // The SARIF log is written, then the "Wrote" line, then the truncation note; both of the
        // latter go to the error stream, so closing the stream at either point must fail the run.
        fails_at_every_line(2, |host| {
            let args = RunArgs {
                artifact_dir: Some(root.clone()),
                annotations: crate::ci::Annotations::None,
                ..Default::default()
            };

            emit_ci(host, &args, &plan, None, Styler::new(false))
        });
    }

    /// The advice document goes in the artifact directory, while prose diagnostics remain opt-in.
    #[test]
    fn advice_goes_where_it_was_asked_for_and_diag_is_emitted_only_when_requested() {
        let dir = workdir("run-advice-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);
        args.artifact_dir = Some(root.clone());
        args.diag = true;

        let plan = plan();
        let session = session();
        let mut host = Sink::default();

        emit_advice(&mut host, &args, &plan, &session, Duration::from_secs(20), Styler::new(false)).expect("advice");
        emit_diag(&mut host, &args, &plan, Some(&session), Instant::now(), Styler::new(false)).expect("diag");

        let advice = fs::read_to_string(root.join("gamma-perf-advice.md")).expect("advice file");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(advice.contains("Mutation testing"), "{advice}");
        assert!(err.contains("Wrote"), "{err}");
        assert!(err.contains("── diag ─"), "{err}");
    }

    #[test]
    fn resumed_reporting_artifacts_describe_frozen_settings() {
        let directory = workdir("resumed-reporting-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf8");
        let mut live = RunArgs {
            artifact_dir: Some(root.clone()),
            ..RunArgs::default()
        };
        live.measure.jobs = Some(99);
        live.measure.profile = Some("test".to_owned());
        let shard = crate::elements::ShardInfo { count: 3, index: 1 };
        let frozen = resumed_reporting_args(&live, 7, Some("relational.lt_to_le"), &[], Some(&shard), Some("gamma"));
        let plan = plan();
        let mut measured = session();
        measured.baseline_wall = Duration::from_secs(20);
        let mut host = Sink::default();
        emit_advice(&mut host, &frozen, &plan, &measured, Duration::from_mins(2), Styler::new(false)).unwrap();
        emit_diag(&mut host, &frozen, &plan, Some(&measured), Instant::now(), Styler::new(false)).unwrap();
        let advice = fs::read_to_string(root.join("gamma-perf-advice.md")).unwrap();
        assert!(advice.contains("at 7 jobs"), "{advice}");
        assert!(!advice.contains("under Cargo's default test profile"), "{advice}");
        assert!(!advice.contains("under test"), "{advice}");
        let diagnostics: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(root.join("gamma-diagnostics.json")).unwrap()).unwrap();
        assert_eq!(diagnostics["config"]["jobs"], 7);
        assert_eq!(diagnostics["config"]["mutators"], serde_json::json!(["relational.lt_to_le"]));
        assert_eq!(diagnostics["config"]["shard"], serde_json::json!([1, 3]));
        let panel = summary_panel(&frozen, &plan, &measured, &[], &[], Duration::from_mins(2));
        assert!(panel.contains("at 7 jobs"), "{panel}");
        assert!(!panel.contains("under test"), "{panel}");
        assert!(summary_panel(&live, &plan, &measured, &[], &[], Duration::from_mins(2)).contains("under test"));
    }

    /// The bundle exists to be attached to an issue, which only works if it is already on disk by
    /// the time anyone decides the run was worth reporting.
    #[test]
    fn every_run_writes_the_diagnostics_bundle_without_being_asked() {
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);

        let plan = plan();
        let session = session();
        let mut host = Sink::default();

        emit_diag(&mut host, &args, &plan, Some(&session), Instant::now(), Styler::new(false)).expect("diag");

        let path = Documents::resolve(&args, &plan.root).diag;
        let text = fs::read_to_string(&path).expect("bundle file");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(!args.diag, "the prose dump was not asked for and must not be the trigger");
        assert!(!err.contains("── diag ─"), "{err}");
        assert_eq!(parsed["schemaVersion"], "7");
        assert!(parsed["run"]["wallMs"].is_number(), "{text}");

        assert!(parsed["phases"]["copy"]["elapsedMs"].is_number(), "{text}");
        assert!(parsed["phases"]["preflight"]["elapsedMs"].is_number(), "{text}");
        assert!(parsed["phases"]["baseline"]["elapsedMs"].is_number(), "{text}");

        assert!(err.contains(&platform_path(&path)), "{err}");
    }

    #[test]
    fn baseline_errors_write_distinct_records_and_diagnostics_bundles() {
        let dir = workdir("run-baseline-failure-artifacts-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = RunArgs {
            artifact_dir: Some(root.clone()),
            ..Default::default()
        };
        let plan = plan();
        let mut failure = Error::new("baseline failed")
            .with_nested_artifact(
                "baseline-failures/first/shared/tests/one".to_owned(),
                "failure.json",
                serde_json::json!({
                    "schemaVersion": 1, "kind": "testFailure", "package": "first",
                    "target": "shared", "stderrTail": "assertion failed",
                }),
            )
            .with_nested_artifact(
                "baseline-failures/second/shared/timeout".to_owned(),
                "failure.json",
                serde_json::json!({
                    "schemaVersion": 1, "kind": "timeout", "package": "second",
                    "target": "shared", "reason": "time budget exceeded",
                }),
            );
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(false, Styler::new(false), Some(80)),
            dashboard: Dashboard::new(false, Some(80), Styler::new(false)),
            styler: Styler::new(false),
            show_build: false,
            verdict_log: VerdictLog::default(),
        };

        emit_failure_artifacts(&mut events, &args, &plan, None, &mut failure, &root, Instant::now());

        let first = root.join("baseline-failures/first/shared/tests/one");
        let second = root.join("baseline-failures/second/shared/timeout");
        let first_baseline = fs::read_to_string(first.join("failure.json")).expect("first record");
        let second_baseline = fs::read_to_string(second.join("failure.json")).expect("second record");
        let first_diagnostics = fs::read_to_string(first.join("diags.json")).expect("first diagnostics");
        let second_diagnostics = fs::read_to_string(second.join("diags.json")).expect("second diagnostics");

        assert!(first_baseline.contains("assertion failed"), "{first_baseline}");
        assert!(second_baseline.contains("time budget exceeded"), "{second_baseline}");
        assert!(first_diagnostics.contains("\"schemaVersion\": \"7\""), "{first_diagnostics}");
        assert!(second_diagnostics.contains("\"schemaVersion\": \"7\""), "{second_diagnostics}");
        assert_eq!(
            failure.to_string(),
            format!(
                "baseline measurement failed due to 2 test failures; see the generated files in {} for details",
                platform_path(&root.join("baseline-failures"))
            )
        );
        let err = host.err();
        assert_eq!(err.matches("Wrote  :").count(), 5, "{err}");
        for path in [
            Documents::resolve(&args, &plan.root).diag,
            first.join("failure.json"),
            first.join("diags.json"),
            second.join("failure.json"),
            second.join("diags.json"),
        ] {
            assert!(err.contains(&platform_path(&path)), "{path} was not reported in:\n{err}");
        }
    }

    #[test]
    fn a_new_build_removes_stale_baseline_failure_artifacts() {
        let dir = workdir("run-clear-baseline-failures-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let stale = root.join("baseline-failures/old/target/tests/case/failure.json");
        fs::create_dir_all(stale.parent().expect("the stale record has a parent")).expect("stale directory");
        fs::write(&stale, "{}").expect("stale failure");

        clear_baseline_failures(&root).expect("stale baseline failures are removable");

        assert!(!root.join("baseline-failures").exists());
        clear_baseline_failures(&root).expect("an absent failure directory is already clean");
    }

    #[test]
    fn an_obstructed_baseline_failure_directory_stops_the_build() {
        let dir = workdir("run-blocked-baseline-failures-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::write(root.join("baseline-failures"), "not a directory").expect("obstruction");

        let failure = clear_baseline_failures(&root).expect_err("stale artifacts cannot be left ambiguous");

        assert!(
            failure.to_string().contains("could not remove stale baseline failures"),
            "{failure}"
        );
    }

    /// The promise the file is worth attaching on: nothing in it names the tree it came from.
    #[test]
    fn the_diagnostics_bundle_carries_neither_source_text_nor_a_path() {
        let dir = workdir("run-bundle-safety-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);
        args.artifact_dir = Some(root.clone());

        let plan = plan();
        let mut host = Sink::default();

        emit_diag(&mut host, &args, &plan, None, Instant::now(), Styler::new(false)).expect("diag");

        let text = fs::read_to_string(root.join("gamma-diagnostics.json")).expect("bundle file");

        assert!(!text.contains("a < b"), "the mutated source reached the bundle: {text}");
        assert!(!text.contains("src/lib.rs"), "a file path reached the bundle: {text}");
        assert!(!text.contains(plan.root.as_str()), "the workspace root reached the bundle: {text}");
        assert!(!text.contains("subject"), "a package name reached the bundle: {text}");
    }

    /// `--diag-names names` is for a tree whose names are already public, and it must actually
    /// change what is written or the flag is decoration.
    #[test]
    fn naming_is_opt_in_and_changes_what_is_written() {
        let dir = workdir("run-bundle-names-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);
        args.artifact_dir = Some(root.clone());
        args.diag_names = crate::diag::Redaction::Names;

        let plan = plan();
        let mut host = Sink::default();

        emit_diag(&mut host, &args, &plan, None, Instant::now(), Styler::new(false)).expect("diag");

        let text = fs::read_to_string(root.join("gamma-diagnostics.json")).expect("bundle file");

        assert!(text.contains("subject"), "{text}");
        assert!(!text.contains("a < b"), "even named, no source text: {text}");
    }

    /// A run that names no advice path still gets the document, beside the reports.
    #[test]
    fn a_run_that_asks_for_no_advice_path_still_writes_it_under_the_gamma_directory() {
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);

        let plan = plan();
        let session = session();
        let mut host = Sink::default();
        let base = plan.root.join("target/cargo-gamma");

        fs::create_dir_all(&base).expect("gamma base");
        emit_advice(&mut host, &args, &plan, &session, Duration::from_secs(20), Styler::new(false)).expect("advice");

        assert!(base.join("gamma-perf-advice.md").exists());
    }

    /// A leftover foreign config is called out, because the run does not honour it.
    #[test]
    fn a_foreign_config_is_called_out_before_anything_is_loaded() {
        let dir = workdir("run-foreign-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "examine_globs = [\"src/**\"]\n").expect("foreign");

        let mut args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                ..crate::commands::SelectArgs::default()
            },
            ..Default::default()
        };
        let mut host = Sink::default();

        configure(&mut host, &mut args, Styler::new(false)).expect("configure");

        assert!(host.err().contains("Hint"), "{}", host.err());
        assert!(host.err().contains("is not supported or read"), "{}", host.err());
        assert!(host.err().contains("configure gamma.toml explicitly"), "{}", host.err());
    }

    #[test]
    fn resumed_environment_is_checked_only_when_mutants_remain_pending() {
        for outcomes in [vec![], vec![Outcome::Killed, Outcome::Survived], vec![Outcome::NoCoverage]] {
            let mutants: Vec<_> = outcomes.into_iter().map(|outcome| mutant(0, outcome, 0)).collect();
            validate_resumed_environment(&mutants, || panic!("completed verdicts inherit no new environment")).unwrap();
        }
        for outcomes in [vec![Outcome::Pending], vec![Outcome::Killed, Outcome::Pending]] {
            let mutants: Vec<_> = outcomes.into_iter().map(|outcome| mutant(0, outcome, 0)).collect();
            let mut checked = false;
            validate_resumed_environment(&mutants, || {
                checked = true;
                Ok(())
            })
            .unwrap();
            assert!(checked);
            let failure = validate_resumed_environment(&mutants, || Err(error!("scripted environment mismatch"))).unwrap_err();
            assert_eq!(failure.to_string(), "scripted environment mismatch");
        }
    }

    #[test]
    fn unavailable_cache_context_still_records_environment_without_enabling_reuse() {
        let (record_context, reusable) = postprocessing_contexts(&RunArgs::default(), None).unwrap();
        assert!(record_context.states(crate::discover::Term::Environment));
        assert!(record_context.term(crate::discover::Term::Toolchain).is_empty());
        assert!(reusable.is_none());
        assert!(
            postprocessing_contexts(
                &RunArgs {
                    dry_run: true,
                    ..RunArgs::default()
                },
                None
            )
            .is_none()
        );
    }

    #[test]
    fn resume_configuration_loads_only_reporting_gates() {
        let dir = workdir("run-resume-gates-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        fs::write(
            root.join("gamma.toml"),
            "min-score = 80.0\nmax-flaky = 2\njobs = 12\nno-confirm = true\n",
        )
        .unwrap();
        for disabled in [false, true] {
            let mut args = RunArgs {
                resume_testing: true,
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..Default::default()
                },
                ..Default::default()
            };
            args.select.config.no_config = disabled;
            configure(&mut Sink::default(), &mut args, Styler::new(false)).unwrap();
            assert_eq!(args.min_score, if disabled { None } else { Some(80.0) });
            assert_eq!(args.max_flaky, if disabled { None } else { Some(2) });
            assert_eq!(args.measure.jobs, None);
            assert!(!args.no_confirm);
        }
    }

    /// Asking not to read config suppresses the note along with the loading.
    #[test]
    fn a_foreign_config_is_not_mentioned_when_config_is_disabled() {
        let dir = workdir("run-foreign-off-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "examine_globs = [\"src/**\"]\n").expect("foreign");

        let mut select = crate::commands::SelectArgs {
            dir: root,
            ..crate::commands::SelectArgs::default()
        };

        select.config.no_config = true;

        let mut args = RunArgs {
            select,
            ..Default::default()
        };
        let mut host = Sink::default();

        configure(&mut host, &mut args, Styler::new(false)).expect("configure");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    /// A directive that suppressed nothing must be named with enough to go and find it: the file,
    /// the line, what it claimed, and why it said it was needed.
    #[test]
    fn an_unused_skip_directive_is_named_with_its_place_and_its_reason() {
        let mut plan = plan();

        plan.idle = vec![crate::suppress::Idle {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 42,
            selectors: "arith".to_owned(),
            reason: Some("the compiler folds it".to_owned()),
        }];

        let mut host = Sink::default();

        report_idle(&mut host, &plan, Styler::new(false)).expect("note");

        let text = host.err();

        assert!(text.contains("1 skip directive"), "{text}");
        assert!(text.contains("src/lib.rs:42"), "{text}");
        assert!(text.contains("skip(arith)"), "{text}");
        assert!(text.contains("the compiler folds it"), "{text}");
    }

    /// The quiet case, and the one that makes the loud case worth trusting.
    #[test]
    fn a_run_whose_skips_all_still_apply_says_nothing_about_them() {
        let plan = plan();
        let mut host = Sink::default();

        report_idle(&mut host, &plan, Styler::new(false)).expect("note");

        assert!(host.err().is_empty(), "{}", host.err());
    }

    /// Every idle directive is named, not just the first: a tree that has drifted has drifted in
    /// several places, and a report that stops after one leaves the rest looking deliberate.
    #[test]
    fn every_unused_skip_directive_is_named() {
        let mut plan = plan();

        plan.idle = (1..=3)
            .map(|line| crate::suppress::Idle {
                file: Utf8PathBuf::from(format!("src/m{line}.rs")),
                line,
                selectors: "arith".to_owned(),
                reason: None,
            })
            .collect();

        let mut host = Sink::default();

        report_idle(&mut host, &plan, Styler::new(false)).expect("note");

        let text = host.err();

        assert!(text.contains("3 skip directives"), "{text}");

        for line in 1..=3 {
            assert!(text.contains(&format!("src/m{line}.rs:{line}")), "{text}");
        }
    }

    /// The build discovery is given is the build the run will actually perform.
    ///
    /// `measured` resolves the run configuration once and hands `config.cargo` to both
    /// `Survey::for_build` and `exec::run`, because discovery evaluates `#[cfg(...)]` against it. If
    /// the command line stopped reaching these options, a run under `--profile release --cargo-arg
    /// --target=…` would be surveyed as a different build and every item behind a gate those
    /// settings decide would be mutated wrongly or not at all — silently, and only in the
    /// configurations nobody tests locally.
    #[test]
    fn the_build_discovery_is_given_carries_the_command_lines_profile_and_arguments() {
        let args = RunArgs {
            measure: MeasureArgs {
                profile: Some("release".to_owned()),
                cargo_args: vec!["--target=x86_64-pc-solaris".to_owned()],
                ..MeasureArgs::default()
            },
            ..Default::default()
        };

        let cargo = run_config(&args, Styler::new(false)).cargo;
        let build = cargo.cfg_build(Utf8Path::new("."));

        assert_eq!(cargo.profile.as_deref(), Some("release"));
        assert_eq!(build.target.as_deref(), Some("x86_64-pc-solaris"));
    }

    #[test]
    fn every_run_setting_reaches_its_execution_config_field() {
        let mut args = RunArgs::default();
        args.select.features.features = vec!["feature-a".to_owned()];
        args.select.features.all_features = true;
        args.select.features.no_default_features = true;
        args.measure.jobs = Some(3);
        args.measure.test_timeout_multiplier = Some(2.25);
        args.measure.minimum_test_timeout = Some(13.5);
        args.measure.memory = Some(exec::MemoryControl::Measure);
        args.measure.memory_multiplier = Some(1.75);
        args.measure.memory_headroom = Some(123_456);
        args.measure.memory_limit = Some(654_321);
        args.measure.baseline_memory_limit = Some(765_432);
        args.measure.profile = Some("distinct-profile".to_owned());
        args.measure.cargo_args = vec!["--distinct-cargo".to_owned()];
        args.measure.cargo_test_args = vec!["--distinct-harness".to_owned()];
        args.measure.test_args = vec!["distinct-test".to_owned()];
        args.measure.cache_dir = Some(Utf8PathBuf::from("distinct-cache"));
        args.measure.copy_ignored = true;
        args.measure.test_packages = vec!["distinct-package".to_owned()];
        args.measure.include_tests = vec!["include-*".to_owned()];
        args.measure.exclude_tests = vec!["exclude-*".to_owned()];
        args.measure.test_workspace = true;
        args.measure.nextest = true;
        args.limits.build_timeout = Some(11.25);
        args.limits.build_timeout_multiplier = Some(3.5);
        args.limits.rollback_rounds = 7;
        args.leak_dirs = true;
        args.no_baseline = true;
        args.no_confirm = true;
        args.no_stall_detection = true;
        args.incremental = Some(exec::IncrementalMode::No);

        let expected_features = args.select.features.to_cargo_args();
        let config = run_config(&args, Styler::new(true));
        let defaults = exec::Config::default();

        assert_eq!(config.jobs, 3);
        assert_eq!(config.test_timeout_multiplier.to_bits(), 2.25_f64.to_bits());
        assert_eq!(config.timeout_floor, Duration::from_secs_f64(13.5));
        assert!(!config.baseline);
        assert!(!config.confirm);
        assert!(!config.stall);
        assert_eq!(config.stall_factor.to_bits(), defaults.stall_factor.to_bits());
        assert_eq!(config.stall_floor, defaults.stall_floor);
        assert_eq!(config.cargo.features, expected_features);
        assert_eq!(config.cargo.profile.as_deref(), Some("distinct-profile"));
        assert_eq!(config.cargo.extra, ["--distinct-cargo"]);
        assert_eq!(config.cargo.test_args, ["--distinct-harness", "distinct-test"]);
        assert!(config.cargo.color);
        assert_eq!(config.build.timeout, Some(Duration::from_secs_f64(11.25)));
        assert_eq!(config.build.multiplier.map(f64::to_bits), Some(3.5_f64.to_bits()));
        assert_eq!(config.build.rollback_rounds, 7);
        assert_eq!(config.memory.control, exec::MemoryControl::Measure);
        assert_eq!(config.memory.demand, exec::Demand::Stated);
        assert_eq!(config.memory.multiplier.to_bits(), 1.75_f64.to_bits());
        assert_eq!(config.memory.headroom, 123_456);
        assert_eq!(config.memory.limit, Some(654_321));
        assert_eq!(config.memory.baseline_limit, Some(765_432));
        assert!(config.leak_dirs);
        assert_eq!(config.cache_dir.as_deref(), Some(Utf8Path::new("distinct-cache")));
        assert!(config.copy_ignored);
        assert_eq!(config.test_packages, ["distinct-package"]);
        assert_eq!(config.include_tests, ["include-*"]);
        assert_eq!(config.exclude_tests, ["exclude-*"]);
        assert!(config.test_workspace);
        assert!(!config.whole_test_binaries);
        assert!(config.nextest);
        assert_eq!(config.incremental, exec::IncrementalMode::No);
    }

    /// With nothing on the command line, a run inherits the 50% default margin.
    ///
    /// The default is pinned to both the shared constant and its literal value, so raising or
    /// lowering it is a deliberate edit here rather than a silent change to every default run.
    #[test]
    fn a_run_with_no_multiplier_uses_the_default_margin() {
        let config = run_config(&RunArgs::default(), Styler::new(false));

        // Bit-exact equality on values that are exactly representable, which also keeps the pedantic
        // `float_cmp` lint quiet on a deliberate exact comparison.
        assert_eq!(config.test_timeout_multiplier.to_bits(), DEFAULT_TEST_TIMEOUT_MULTIPLIER.to_bits());
        assert_eq!(config.test_timeout_multiplier.to_bits(), 1.5_f64.to_bits());
    }

    /// An explicit multiplier wins outright over the default, whichever way the default moves.
    #[test]
    fn an_explicit_multiplier_overrides_the_default_margin() {
        let args = RunArgs {
            measure: MeasureArgs {
                test_timeout_multiplier: Some(2.0),
                ..MeasureArgs::default()
            },
            ..Default::default()
        };

        assert_eq!(
            run_config(&args, Styler::new(false)).test_timeout_multiplier.to_bits(),
            2.0_f64.to_bits()
        );
    }

    /// A survivor count past the SARIF ceiling is said out loud rather than silently trimmed.
    #[test]
    fn a_truncated_sarif_log_says_how_much_it_left_out() {
        let dir = workdir("run-sarif-truncated-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let args = RunArgs {
            artifact_dir: Some(root),
            ..Default::default()
        };

        let mut plan = plan();
        let survivor = plan.mutants[0].clone();

        plan.mutants = (0..=crate::ci::sarif::SARIF_LIMIT)
            .map(|index| {
                let mut mutant = survivor.clone();

                mutant.id = format!("m{index}").into();
                mutant
            })
            .collect();

        let mut host = Sink::default();

        emit_ci(&mut host, &args, &plan, None, Styler::new(false)).expect("ci");

        assert!(host.err().contains("warning"), "{}", host.err());
        assert!(host.err().contains("findings written"), "{}", host.err());
        assert!(host.err().contains("reject a larger log outright"), "{}", host.err());
    }

    /// A mutant whose `expect_survived` directive did not hold — because the suite actually caught
    /// it — fails the whole run and names the mutant that broke the promise; a gate that only
    /// checked the score would let a contradicted claim about the suite go unnoticed forever.
    #[test]
    fn a_contradicted_expectation_fails_the_run_and_names_the_mutant() {
        let dir = workdir("run-contradicted-");
        let root = subject_root(&dir);
        fs::write(
            root.join("src/lib.rs"),
            "// #[gamma::expect_survived(relational)]\n\
             pub fn less(a: i32, b: i32) -> bool { a < b }\n\n\
             #[test]\n\
             fn catches_it() {\n\
             \x20\x20\x20\x20assert!(less(1, 2));\n\
             \x20\x20\x20\x20assert!(!less(1, 1));\n\
             }\n",
        )
        .expect("lib");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(host.err().contains("expected survived, but this mutant was"), "{}", host.err());
        assert!(host.err().contains("1 expectation did not hold"), "{}", host.err());
    }

    /// A run whose score falls short of `--min-score` fails the gate even when every expectation
    /// held; the score threshold and the expectation directives are separate promises, and one
    /// holding does not excuse the other from being checked.
    #[test]
    fn a_run_below_the_minimum_score_fails_the_gate() {
        let dir = workdir("run-min-score-");
        let root = subject_root(&dir);
        // No test asserts anything about `less`, so the survivor is left uncontested and the
        // score comes in at 0%.
        fs::write(
            root.join("src/lib.rs"),
            "pub fn less(a: i32, b: i32) -> bool { a < b }\n\n\
             #[test]\n\
             fn calls_it() {\n\
             \x20\x20\x20\x20let _ = less(1, 2);\n\
             }\n",
        )
        .expect("lib");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            min_score: Some(50.0),
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(host.err().contains("is below the required 50.0%"), "{}", host.err());
    }

    /// A diff written with `diff.mnemonicPrefix` names its paths `i/` and `w/` rather than `a/` and
    /// `b/`. Every mutant in the change is still a mutant, so the gate still has a population to
    /// judge and still fails on a survivor — where before the paths resolved to nothing, the run
    /// tested nothing, and `--min-score 100` passed.
    #[test]
    fn a_diff_with_mnemonic_prefixes_is_still_gated() {
        let dir = workdir("run-in-diff-mnemonic-");
        let root = subject_root(&dir);
        // Nothing asserts anything about `less`, so the mutant on the changed line survives.
        fs::write(
            root.join("src/lib.rs"),
            "pub fn less(a: i32, b: i32) -> bool { a < b }\n\n\
             #[test]\n\
             fn calls_it() {\n\
             \x20\x20\x20\x20let _ = less(1, 2);\n\
             }\n",
        )
        .expect("lib");
        fs::write(
            root.join("change.patch"),
            "diff --git i/src/lib.rs w/src/lib.rs\n\
             --- i/src/lib.rs\n\
             +++ w/src/lib.rs\n\
             @@ -0,0 +1 @@\n\
             +pub fn less(a: i32, b: i32) -> bool { a < b }\n",
        )
        .expect("patch");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                in_diff: Some(root.join("change.patch")),
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            min_score: Some(100.0),
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(host.err().contains("is below the required 100.0%"), "{}", host.err());
    }

    /// A run whose every mutant is suppressed never builds anything, and the summary must still
    /// account for them all; if the summary were skipped along with the build, a fully suppressed
    /// package would look like a run that simply found nothing, hiding the fact that everything in
    /// it was deliberately excluded.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_run_with_every_mutant_suppressed_never_builds_and_still_summarizes() {
        let dir = workdir("run-all-suppressed-");
        let root = subject_root(&dir);
        fs::write(
            root.join("src/lib.rs"),
            "// #[gamma::skip(relational)]\n\
             pub fn less(a: i32, b: i32) -> bool { a < b }\n",
        )
        .expect("lib");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_OK);
        assert!(
            host.out().contains("none tested") || host.err().contains("none tested"),
            "{}{}",
            host.out(),
            host.err()
        );
    }

    /// `--leak-dirs` says where the scratch tree that would otherwise be deleted was left, so a
    /// caller who asked to inspect a build after the fact can find it; forgetting to print the path
    /// would make the flag indistinguishable from doing nothing.
    #[test]
    fn leak_dirs_reports_where_the_scratch_tree_was_kept() {
        let dir = workdir("run-leak-dirs-");
        let root = subject_root(&dir);
        fs::write(
            root.join("src/lib.rs"),
            "pub fn less(a: i32, b: i32) -> bool { a < b }\n\n\
             #[test]\n\
             fn catches_it() {\n\
             \x20\x20\x20\x20assert!(less(1, 2));\n\
             \x20\x20\x20\x20assert!(!less(1, 1));\n\
             }\n",
        )
        .expect("lib");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            leak_dirs: true,
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_OK);
        assert!(host.err().contains("Kept"), "{}", host.err());
    }

    /// An artifact directory that cannot be created is reported as an error rather than silently
    /// dropped, for the same reason a broken job-summary path is: the caller asked for a specific
    /// artifact, and a swallowed failure there is indistinguishable from a run that never analyzed
    /// anything.
    ///
    /// A missing parent directory is not that path — the writer creates one, as it does for the
    /// reports beside it — so the broken path here is one whose parent is a file, which no amount
    /// of creating directories can make writable.
    #[test]
    fn a_broken_artifact_directory_is_reported_as_an_error() {
        let dir = workdir("run-advice-broken-");
        let root = subject_root(&dir);
        fs::write(
            root.join("src/lib.rs"),
            "pub fn less(a: i32, b: i32) -> bool { a < b }\n\n\
             #[test]\n\
             fn catches_it() {\n\
             \x20\x20\x20\x20assert!(less(1, 2));\n\
             \x20\x20\x20\x20assert!(!less(1, 1));\n\
             }\n",
        )
        .expect("lib");

        fs::write(root.join("blocked"), "not a directory").expect("blocker");

        let args = RunArgs {
            artifact_dir: Some(root.join("blocked/artifacts")),
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            ..Default::default()
        };
        let mut host = Sink::default();

        let error = run_session(&mut host, &args, When::Never, Styler::new(false)).expect_err("broken artifact directory");

        assert!(error.to_string().contains("could not create artifact directory"), "{error}");
    }

    /// A gate that cannot fail is worse than no gate. A selection that leaves nothing to test used
    /// to exit zero, so `--min-score 100` passed on the strength of having tested nothing.
    #[test]
    fn every_gate_fails_when_nothing_was_available_to_test() {
        let dir = workdir("run-empty-gated-");
        let root = subject_root(&dir);
        fs::write(root.join("src/lib.rs"), "pub fn less(a: i32, b: i32) -> bool { a < b }\n").expect("lib");

        // Every file is excluded, so discovery finds nothing at all: the shape an over-eager
        // `--exclude-file`, an empty shard and a diff that named no code all arrive in.
        for (min_score, max_flaky, gate) in [(Some(100.0), None, "--min-score"), (None, Some(0), "--max-flaky")] {
            let args = RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    exclude_files: vec!["**/*.rs".to_owned()],
                    ..crate::commands::SelectArgs::default()
                },
                dry_run: true,
                min_score,
                max_flaky,
                ..Default::default()
            };
            let mut host = Sink::default();

            let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

            assert_eq!(code, EXIT_GATE_FAILED);
            assert!(host.err().contains("never evaluated"), "{}", host.err());
            assert!(host.err().contains(gate), "{}", host.err());
            assert!(host.err().contains("--exclude-file"), "{}", host.err());
            assert!(host.err().contains("--shard-count"), "{}", host.err());
        }
    }

    /// The same run without a gate made no claim about its score, so an empty population is an
    /// ordinary answer and must go on succeeding.
    #[test]
    fn an_ungated_run_with_nothing_to_test_still_succeeds() {
        let dir = workdir("run-empty-ungated-");
        let root = subject_root(&dir);
        fs::write(root.join("src/lib.rs"), "pub fn less(a: i32, b: i32) -> bool { a < b }\n").expect("lib");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                exclude_files: vec!["**/*.rs".to_owned()],
                ..crate::commands::SelectArgs::default()
            },
            dry_run: true,
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_OK);
    }

    /// A population that is not empty but holds nothing scorable — the shard that contained only
    /// suppressed mutants — is the same failure wearing a different hat: the score is a ratio with
    /// nothing in its denominator, and a threshold judged against it never ran.
    #[test]
    fn a_gated_run_whose_population_is_entirely_suppressed_fails() {
        let dir = workdir("run-suppressed-gated-");
        let root = subject_root(&dir);
        fs::write(
            root.join("src/lib.rs"),
            "// #[gamma::skip(relational)]\n\
             pub fn less(a: i32, b: i32) -> bool { a < b }\n",
        )
        .expect("lib");

        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            min_score: Some(100.0),
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(host.err().contains("never evaluated"), "{}", host.err());
    }

    /// And from the foreign-config note.
    #[test]
    fn a_closed_stream_is_reported_by_the_foreign_config_note() {
        let dir = workdir("run-foreign-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir_all(root.join(".cargo")).expect("cargo dir");
        fs::write(root.join(".cargo/mutants.toml"), "examine_globs = [\"src/**\"]\n").expect("foreign");

        // `configure` folds the file into `args`, so each attempt needs its own copy of them.
        fails_at_every_line(1, |host| {
            let mut args = RunArgs {
                select: crate::commands::SelectArgs {
                    dir: root.clone(),
                    ..crate::commands::SelectArgs::default()
                },
                ..Default::default()
            };

            configure(host, &mut args, Styler::new(false))
        });
    }

    /// A run whose build cannot be made to compile still reports, and still fails.
    ///
    /// This is the whole of the bargain: a workspace that gets stuck after an hour would otherwise
    /// end with an error and nothing else — no JSON, no HTML, no annotations, no score — so the
    /// time would be spent and nothing learned. The population that never ran is recorded as
    /// `notbuilt`, the report is written, the diagnostic that would have been the error is printed,
    /// and the exit code still says the run did not succeed.
    #[test]
    fn a_run_whose_build_cannot_converge_still_writes_a_report_and_still_fails() {
        let dir = workdir("run-unconverged-");
        let root = subject_root(&dir);

        // A call to a symbol that does not exist passes `cargo check --tests`, so the preflight
        // clears the tree, and then fails to link when the test targets are actually built. No
        // mutant can be blamed for a linker error, which is exactly the failure that would
        // otherwise end the run with nothing to show for it.
        fs::write(root.join("src/lib.rs"), fixtures::UNRESOLVED_LINK_SOURCE).expect("lib");

        let artifact_dir = root.join("artifacts");
        let report = artifact_dir.join("gamma-report.json");
        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                mutators: Some("relational.lt_to_le".to_owned()),
                ..crate::commands::SelectArgs::default()
            },
            artifact_dir: Some(artifact_dir),
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        // A build the tool could not converge is not a successful run, whatever was written.
        assert_ne!(code, EXIT_OK, "{}", host.err());
        assert_eq!(code, EXIT_CANNOT_PROCEED, "{}", host.err());

        let written = fs::read_to_string(&report).expect("the report has to exist even so");

        // The distinction the whole issue is about: these mutants never ran, so calling them
        // survivors would report a test-suite gap that does not exist. The schema has no status of
        // its own for a mutant nobody built, and `Ignored` is what `NotBuilt` exports as — what
        // matters is that it is not in the denominator and not called a survivor.
        assert!(written.contains("\"status\": \"Ignored\""), "{written}");
        assert!(!written.contains("Survived"), "{written}");

        // The diagnostic that would otherwise be the error, kept whole, and grouped so the reader can see
        // where the tool got stuck rather than merely that it did.
        let said = host.err();

        assert!(said.contains("could not be made to compile"), "{said}");
        assert!(said.contains("never ran"), "{said}");
        assert!(said.contains("Not run, by mutator: relational.lt_to_le"), "{said}");
        assert!(said.contains("Not run, by scope"), "{said}");
    }

    /// A near miss must never render as if it met the gate.
    ///
    /// The gate compares the full-precision score, so 66.666…% correctly fails a 66.7% bar — but
    /// printed at one decimal both read "66.7%", and the failure message would deny itself. The run
    /// gate builds its message through `distinguish`, which grows the precision until the two are
    /// visibly different: at two decimals the score reads "66.67" and the threshold "66.70", so the
    /// sentence no longer prints the same number twice.
    #[test]
    fn the_run_gate_shows_a_near_miss_as_below_the_threshold_not_equal_to_it() {
        let (score, minimum) = distinguish(200.0 / 3.0, 66.7);

        assert_ne!(score, minimum);
        assert_eq!(score, "66.67");
        assert_eq!(minimum, "66.70");
    }

    /// A clear miss stays at the one decimal place the report has always used.
    #[test]
    fn a_clear_miss_is_shown_at_a_single_decimal_place() {
        assert_eq!(distinguish(50.0, 80.0), ("50.0".to_owned(), "80.0".to_owned()));
    }
}
