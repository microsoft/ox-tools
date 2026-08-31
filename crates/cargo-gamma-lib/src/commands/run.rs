// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs;
use std::io::Write;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};

use super::cli::RunArgs;
use super::console_events::ConsoleEvents;
use super::dispatch::{DEFAULT_TEST_TIMEOUT_MULTIPLIER, EXIT_CANNOT_PROCEED, EXIT_GATE_FAILED, EXIT_OK};
use super::host::Host;
use super::verdict_log::VerdictLog;
use super::when::When;
use crate::config::Config;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::exec;
use crate::model::{Mutant, Outcome};
use crate::report::{Listings, Progress, Styler, quantity};

/// Which of the bulk outcome listings the caller asked for.
const fn listings(args: &RunArgs, announced: bool) -> Listings {
    Listings {
        killed: args.show_killed,
        unviable: args.show_unviable,
        announced,
    }
}

fn missing_hints(mode: exec::IncrementalMode, root: &Utf8Path) -> bool {
    mode.is_enabled() && crate::discover::Hints::is_missing(root)
}

/// Loads `gamma.toml` and folds it into `args`.
pub(super) fn configure<H: Host>(host: &mut H, args: &mut RunArgs, styler: Styler) -> crate::Result<()> {
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
    crate::elements::RunInfo {
        tests,
        started_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs()),
        merged: false,
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

/// Where a run's documents go, once the defaults and any overrides have been settled.
///
/// Every run writes all five — HTML, JSON and SARIF reports, Markdown advice and a diagnostics bundle.
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
    let documents = Documents::resolve(args, &plan.root);
    let report = crate::elements::build(plan, crate::elements::Thresholds::default(), Some(run_info(args, tests, dropped)))?;
    let mut stream = host.error();

    crate::elements::write_json(&report, &documents.json)?;
    writeln!(stream, "{} {}", styler.verb("Wrote"), documents.json)?;

    let source = if args.html_external {
        crate::html::Source::External
    } else {
        crate::html::Source::Inline
    };

    crate::html::write_page(&report, source, &documents.html)?;
    writeln!(stream, "{} {}", styler.verb("Wrote"), documents.html)?;
    drop(stream);

    emit_ci(host, args, plan, advice, styler)?;

    Ok(())
}

/// Writes the SARIF log and, when requested, the diff annotations and job summary.
fn emit_ci<H: Host>(host: &mut H, args: &RunArgs, plan: &Plan, advice: Option<&str>, styler: Styler) -> crate::Result<()> {
    let path = Documents::resolve(args, &plan.root).sarif;
    let (log, truncation) = crate::ci::sarif(&plan.mutants, &plan.root, args.sarif_level)?;

    crate::elements::write(&path, &log)?;

    let mut stream = host.error();

    writeln!(stream, "{} {path}", styler.verb("Wrote"))?;

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

    if !crate::ci::wanted(args.annotations, host.env("GITHUB_ACTIONS").is_some()) {
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
/// A gate that cannot fail is worse than no gate. Every route to an empty score — an exclude
/// pattern that matched everything, a shard that held nothing but suppressions, a diff that named
/// no code, an incremental run that had already settled the lot — would otherwise end in a
/// summary that said nothing was tested and an exit code that said everything was fine. A job that
/// asked for `--min-score 100` would then pass on the strength of having tested nothing at all,
/// indefinitely, which is the exact failure the flag exists to prevent.
///
/// A run that asked for no gate is left alone: an empty population is a perfectly ordinary answer
/// to a narrow selection, and turning it into a failure would break every run that never made a
/// claim about its score.
fn ungraded<H: Host>(host: &mut H, args: &RunArgs, styler: Styler, expectations: bool) -> i32 {
    let gate = if args.min_score.is_some() {
        "the `--min-score` gate"
    } else if expectations {
        "the expectations this run carried"
    } else {
        return EXIT_OK;
    };

    let _ = writeln!(
        host.error(),
        "{} no mutant counted toward the score, so {gate} was never evaluated; \
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

pub(super) fn run_session<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<i32> {
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
        return Ok(ungraded(host, args, styler, false));
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

    // Every mutant was suppressed, unviable, or never built, so the score is a ratio with nothing
    // in its denominator. That prints as 100%, which is the right answer to "how much of what ran
    // was caught" and the wrong one to hand a threshold.
    let Some(score) = summary.scored() else {
        return Ok(ungraded(
            host,
            args,
            styler,
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

/// Says which part of the build context cost this run the record's unviability.
///
/// Only the tier that was refused is reported. Unviability is a claim about what compiles, so it
/// requires every term of the context; the probes and the build order the same record holds require
/// none and are used regardless. Saying "the cache did not apply" would send the reader through
/// their whole configuration, and would also be wrong — most of the record still applied.
fn report_context<H: Host>(host: &mut H, moved: &[crate::discover::Term], styler: Styler) -> crate::Result<()> {
    let Some(first) = moved.first() else {
        return Ok(());
    };

    let named: Vec<&str> = moved.iter().map(|term| term.name()).collect();
    let axes = if moved.len() == 1 {
        first.name().to_owned()
    } else {
        named.join(", ")
    };

    writeln!(
        host.error(),
        "{} the record's unviability: {} differs from the run that wrote it. Its build order is still used, so the mutants that failed last time are compiled first",
        styler.note("Rebuilding"),
        axes
    )?;

    Ok(())
}

/// Digests the things other than the sources that decide whether a mutant compiles.
///
/// `None` means this run has no trustworthy key — the compiler could not be asked what it is — and
/// the cache is then neither read nor written. That costs the run the time it would have saved,
/// which is the right side to fail on for a cache whose entries are believed rather than re-checked.
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
        build_timeout: args.limits.build_timeout,
        build_timeout_multiplier: args.limits.build_timeout_multiplier,
        rollback_rounds: args.limits.rollback_rounds,
    })
}

struct IncrementalPreparation {
    base: Utf8PathBuf,
    context: crate::discover::ContextDigest,
    inputs: crate::discover::WorkspaceSnapshot,
}

impl IncrementalPreparation {
    fn for_run(args: &RunArgs, survey: &crate::discover::Survey, context: crate::discover::ContextDigest) -> Self {
        let base = exec::gamma_base(&survey.root, args.measure.cache_dir.as_deref());
        let inputs = crate::discover::RunRecord::snapshot_with_external(
            &survey.root,
            &base,
            survey.external_inputs(),
            survey.has_untracked_build_script_inputs(),
        );

        Self { base, context, inputs }
    }
}

fn incremental_context(args: &RunArgs) -> Option<crate::discover::ContextDigest> {
    let mode = args.incremental.unwrap_or(exec::IncrementalMode::Build);

    (!args.dry_run && mode.is_enabled()).then(|| cache_context(args)).flatten()
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
        include_tests: args.measure.include_tests.clone(),
        exclude_tests: args.measure.exclude_tests.clone(),
        test_workspace: args.measure.test_workspace,
        whole_test_binaries: args.measure.whole_test_binaries,
        nextest: args.measure.nextest,
        incremental: args.incremental.unwrap_or(exec::IncrementalMode::Build),
        timeout_floor: args
            .measure
            .minimum_test_timeout
            .map_or_else(|| exec::Config::default().timeout_floor, Duration::from_secs_f64),
        ..exec::Config::default()
    }
}

/// Discovers, runs and reports, returning everything the caller needs to judge the run.
///
/// Returns the whole [`Executed`], not just the plan it produced. `suppress` derives source edits
/// from these verdicts, and a run that could not build part of its population has verdicts for the
/// rest and none at all for that part — which is a decision only the caller can make, so the
/// stuck builds travel with the plan rather than being dropped on the way out. Split out of
/// [`run_session`] so `suppress` can act on the verdicts rather than re-deriving them from a
/// second run.
pub(super) fn execute<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<Executed> {
    measured(host, args, progress_when, styler)
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
            let record = crate::discover::RunRecord::load(base);

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
            let moved = if record.holds_unviability() {
                record.context().differences(&context.resolved_at(&survey.root))
            } else {
                Vec::new()
            };

            let (settled, declined) = record.settled_against(
                &survey.root,
                crate::discover::Trust::Free,
                &crate::discover::Killers::default(),
                context,
                inputs,
            );

            (settled, declined, moved)
        }
        _absent_or_disabled => (crate::HashMap::default(), 0, Vec::new()),
    };

    // What the record contributed for free, which is what `report_cache` announces. A run that
    // asked for more must not have the rest credited to the free tier.
    let free: crate::HashMap<crate::model::MutantId, Outcome> = recorded
        .iter()
        .filter(|(_id, outcome)| **outcome == Outcome::CompileError)
        .map(|(id, outcome)| (id.clone(), *outcome))
        .collect();

    if !recorded.is_empty() {
        survey.settle(recorded);
    }

    (free, declined, moved)
}

#[expect(
    clippy::too_many_lines,
    reason = "the command orchestrator keeps its ordered reporting and resource-cleanup paths together"
)]
fn measured<H: Host>(host: &mut H, args: &RunArgs, progress_when: When, styler: Styler) -> crate::Result<Executed> {
    let started = Instant::now();
    let selection = args.select.selection()?;
    let shard = args.select.shard()?;
    let visible = progress_when.resolve(host.is_terminal());
    let mut progress = Progress::new(visible, styler, host.terminal_width());

    progress.status(host, "Analyzing", "the workspace");

    // Settled before discovery rather than just before the run, because discovery evaluates
    // `#[cfg(...)]` against the build these options describe. Resolving it once and handing the
    // same value to both is what keeps the tree that is surveyed and the tree that is compiled from
    // describing different builds — a dry run included, since its listing is a claim about what a
    // real run would do.
    let config = run_config(args, styler);
    let context = incremental_context(args);
    let mut survey = crate::discover::Survey::for_build_with_cache_inputs(&args.select, shard, &config.cargo, context.is_some())?;
    let artifact_dir = Documents::directory(args, &survey.root);
    fs::create_dir_all(&artifact_dir).map_err(|cause| error!("could not create artifact directory `{artifact_dir}`").caused_by(cause))?;

    let incremental = context.map(|context| IncrementalPreparation::for_run(args, &survey, context));
    let cache_locks = if incremental.is_some() && !args.dry_run && args.measure.cache_dir.is_some() {
        Some(exec::claim_cache(&survey.root, args.measure.cache_dir.as_deref())?)
    } else {
        None
    };
    let (cached, _declined, moved) = incremental.as_ref().map_or_else(
        || (crate::HashMap::default(), 0, Vec::new()),
        |prepared| adopt(args, &mut survey, &prepared.base, Some(&prepared.context), &prepared.inputs),
    );

    // A dry run reports on the whole population and builds nothing, so there is no package-by-
    // package sequence to interleave the scan with; it is simply scanned.
    if args.dry_run {
        let mut ordinals = 0;
        let scanned = survey.scan(None, &selection, &mut ordinals)?;
        let plan = survey.into_plan(scanned);

        progress.finish(host);
        report_idle(host, &plan, styler)?;

        if plan.mutants.is_empty() {
            let _ = writeln!(host.error(), "no mutants were generated");

            return Ok(Executed {
                plan: None,
                stuck: Vec::new(),
            });
        }

        crate::report::summarize(host, &plan, styler, listings(args, false))?;
        emit_reports(host, args, &plan, None, None, &[], styler)?;
        emit_diag(host, args, &plan, None, started, styler)?;

        return Ok(Executed {
            plan: Some(plan),
            stuck: Vec::new(),
        });
    }

    let mut events = ConsoleEvents {
        host,
        progress,
        styler,
        estimate: args.estimate,
        show_build: args.measure.show_build,
        verdict_log: VerdictLog::default(),
    };

    let outcome = exec::run_with_locks(&survey, &selection, &config, &mut events, cache_locks);

    // A phase that failed never got to say what it found, so the line it opened is still waiting
    // for an ending. Close it before the error is printed, or the error arrives as the rest of
    // that sentence.
    if outcome.is_err() {
        events.abandon();
    }

    let log_result = events.finish_verdict_log();

    let exec::Measured {
        plan,
        built,
        stuck,
        dropped,
    } = outcome?;

    let log_failure = log_result.err();

    let mut progress = events.progress;

    // The live display named every survivor and timeout as it happened, so the summary must not
    // name them again.
    let announced = progress.is_enabled();

    progress.finish(host);

    // Written from the whole population so an adopted cache is preserved. Failing here is not failing the run:
    // every verdict has already been reached, and a scratch file that could not be written must
    // only ever cost the next run some time.
    let mode = args.incremental.unwrap_or(exec::IncrementalMode::Build);
    if let Some(IncrementalPreparation { base, context, inputs }) = incremental
        && let Some(record) = crate::discover::RunRecord::from_plan_snapshot(&plan, &context, inputs, &crate::discover::Killers::default())
    {
        let record_locks = if built.is_none() {
            match exec::claim_cache(&plan.root, args.measure.cache_dir.as_deref()) {
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
            record.store(&base, &plan.root);
        }
    }

    let adopted = adopted_from_cache(&plan, &cached);

    report_cache(host, adopted, styler)?;
    report_context(host, &moved, styler)?;
    report_idle(host, &plan, styler)?;

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
        crate::report::summarize(host, &plan, styler, listings(args, announced))?;

        emit_reports(host, args, &plan, stuck_panel(&stuck).as_deref(), None, &dropped, styler)?;
        emit_diag(host, args, &plan, None, started, styler)?;
        report_dropped(host, &dropped, styler)?;
        report_stuck(host, &stuck, styler)?;
        warn_auxiliary(host, log_failure.as_ref(), styler);

        return Ok(Executed { plan: Some(plan), stuck });
    };

    if args.leak_dirs {
        let tree = exec::scratch_tree(&plan.root, args.measure.cache_dir.as_deref());

        writeln!(host.error(), "{} {tree}", styler.verb("Kept"))?;
    }

    crate::report::summarize(host, &plan, styler, listings(args, announced))?;
    let has_suppressible_mutants = plan
        .mutants
        .iter()
        .any(|mutant| matches!(mutant.outcome, Outcome::Timeout | Outcome::OutOfMemory));
    crate::report::session_notes(
        host,
        &built.session,
        missing_hints(mode, &plan.root),
        has_suppressible_mutants,
        styler,
    )?;

    let wall = started.elapsed();
    let panel = summary_panel(args, &plan, &built.session, &stuck, &dropped, wall);

    emit_reports(host, args, &plan, Some(&panel), built.session.tests, &dropped, styler)?;
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

    emit_diag_bundle(host, args, plan, session, jobs, started, styler)
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
    jobs: usize,
    started: Instant,
    styler: Styler,
) -> crate::Result<()> {
    let path = Documents::resolve(args, &plan.root).diag;
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

    crate::elements::write(&path, &crate::diag::to_json(&bundle)?)
        .map_err(|cause| crate::error::error!("could not write the diagnostics bundle to `{path}`").caused_by(cause))?;

    writeln!(host.error(), "{} {}", styler.verb("Wrote"), path)?;

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

    writeln!(host.error(), "{} {path}", styler.verb("Wrote"))?;

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
#[cfg(not(miri))]
mod tests {
    use super::super::cli::MeasureArgs;
    use super::*;
    use crate::discover::TargetFile;
    use crate::exec::Session;
    use crate::fixtures;

    /// Lays out an empty single-package `subject` workspace in `dir` and returns its root.
    ///
    /// Only the manifest and the `src` directory: what the source under test is varies per test,
    /// so each caller writes its own `src/lib.rs`.
    fn subject_root(dir: &tempfile::TempDir) -> Utf8PathBuf {
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        fs::create_dir(root.join("src")).expect("src");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("manifest");
        root
    }

    use crate::testing::{Broken, Sink, fails_at_every_line, workdir};

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

        fn is_terminal(&self) -> bool {
            false
        }

        fn terminal_width(&self) -> Option<u16> {
            None
        }
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
    }

    #[test]
    fn discovery_resolves_external_inputs_only_for_cache_provenance() {
        let dir = workdir("run-cache-inputs-");
        let container = Utf8Path::from_path(dir.path()).expect("UTF-8 work directory");
        let root = container.join("workspace");
        let dependency = container.join("dependency");
        fs::create_dir_all(root.join("src")).expect("workspace source");
        fs::create_dir_all(dependency.join("src")).expect("dependency source");
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             [dependencies]\ndependency = { path = \"../dependency\" }\n\n[workspace]\n",
        )
        .expect("workspace manifest");
        fs::write(root.join("src/lib.rs"), "pub fn subject() {}\n").expect("workspace source");
        fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname = \"dependency\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("dependency manifest");
        fs::write(dependency.join("src/lib.rs"), "pub fn dependency() {}\n").expect("dependency source");
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
        let src = root.join("src");
        fs::create_dir(&src).expect("src");
        let source = "pub fn less(a: i32, b: i32) -> bool { a < b }\n";
        let absolute = src.join("lib.rs");
        fs::write(&absolute, source).expect("source");
        let start = source.find("a < b").expect("span");

        Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root,
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/lib.rs"),
                absolute,
                package: "subject".to_owned(),
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
                html_external: true,
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
            html_external: true,
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

        assert!(documents.json.exists());
        assert!(documents.html.exists());
        assert!(documents.sarif.exists());
        assert!(out.contains("::warning"), "{out}");
        assert!(err.contains("Wrote"), "{err}");
        assert!(summary_text.contains("embedded advice"), "{summary_text}");
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
        assert!(err.contains("diag"), "{err}");
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

        assert!(!args.diag, "the prose dump was not asked for and must not be the trigger");
        assert_eq!(parsed["schemaVersion"], "3");
        assert!(parsed["run"]["wallMs"].is_number(), "{text}");

        // The phase profile is what makes the census's cost and its sweep dividend readable from one
        // run. The copy, preflight and baseline always ran, so they are always present and named in
        // camelCase; this session had no census, so that phase is omitted rather than a lying zero.
        assert!(parsed["phases"]["copy"]["elapsedMs"].is_number(), "{text}");
        assert!(parsed["phases"]["preflight"]["elapsedMs"].is_number(), "{text}");
        assert!(parsed["phases"]["baseline"]["elapsedMs"].is_number(), "{text}");
        assert!(parsed["phases"]["census"].is_null(), "no census ran, so it is omitted: {text}");

        assert!(String::from_utf8(host.err).expect("utf-8").contains(path.as_str()));
    }

    /// A run that censused says so in its bundle: the phase carries its own elapsed time, the tests
    /// it walked, and the binaries it examined, so the census's cost is legible on its own rather
    /// than folded inside the build's total.
    #[test]
    fn a_censused_run_records_the_census_phase_in_its_bundle() {
        let mut args = RunArgs::default();

        args.measure.jobs = Some(0);

        let plan = plan();
        let mut session = session();

        session.phases.census = Some(exec::CensusCost {
            elapsed: Duration::from_secs(8),
            walked: 1_681,
            binaries: 30,
        });

        let mut host = Sink::default();

        emit_diag(&mut host, &args, &plan, Some(&session), Instant::now(), Styler::new(false)).expect("diag");

        let path = Documents::resolve(&args, &plan.root).diag;
        let text = fs::read_to_string(&path).expect("bundle file");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");

        assert_eq!(parsed["phases"]["census"]["elapsedMs"], 8_000);
        assert_eq!(parsed["phases"]["census"]["walked"], 1_681);
        assert_eq!(parsed["phases"]["census"]["binaries"], 30);
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
    fn a_gated_run_with_nothing_to_test_fails_rather_than_passing_silently() {
        let dir = workdir("run-empty-gated-");
        let root = subject_root(&dir);
        fs::write(root.join("src/lib.rs"), "pub fn less(a: i32, b: i32) -> bool { a < b }\n").expect("lib");

        // Every file is excluded, so discovery finds nothing at all: the shape an over-eager
        // `--exclude-file`, an empty shard and a diff that named no code all arrive in.
        let args = RunArgs {
            select: crate::commands::SelectArgs {
                dir: root,
                exclude_files: vec!["**/*.rs".to_owned()],
                ..crate::commands::SelectArgs::default()
            },
            dry_run: true,
            min_score: Some(100.0),
            ..Default::default()
        };
        let mut host = Sink::default();

        let code = run_session(&mut host, &args, When::Never, Styler::new(false)).expect("run session");

        assert_eq!(code, EXIT_GATE_FAILED);
        assert!(host.err().contains("never evaluated"), "{}", host.err());
        assert!(host.err().contains("--exclude-file"), "{}", host.err());
        assert!(host.err().contains("--shard-count"), "{}", host.err());
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
