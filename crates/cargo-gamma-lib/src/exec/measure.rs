// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Preparing the tree, building the schema, and driving a run to its verdicts.

use core::fmt::Write as _;
use core::time::Duration;
use std::fs;
use std::time::Instant;

use camino::Utf8PathBuf;
use cargo_gamma_process::MemoryRequest;

use super::baseline::{Baseline, measure_baseline};
use super::build::{Abandoned, Build, Converger};
use super::config::Config;
use super::events::Events;
use super::killers::Killers;
use super::memory;
use super::memory::MemoryPolicy;
use super::resources::Resources;
use super::session::{PackageSweepCost, Phases, Session, SweepCost};
use super::stall::Stall;
use super::sweep::{Sweep, test_all};
use super::test_binary::{
    Reachability, TestBinary, TestScope, admits_target, build_packages, calibrate, oracle_packages, reaches, reaching_packages, restrict,
    unmatched_test,
};
use super::workspace::{Workspace, campaign_base};
use crate::discover::{CompileFailTarget, LibraryTestTarget, Plan, Survey, compile_fail_advice};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::ops::registry::Selection;
use crate::{HashMap, Result};

/// How many groups a "not run, by …" line names before it starts counting the rest.
const GROUP_LIMIT: usize = 5;

#[cfg(test)]
thread_local! {
    static PREPARED_MEASUREMENT: std::cell::RefCell<Option<Measured>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_prepared_measurement<T>(measurement: Measured, body: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PREPARED_MEASUREMENT.with_borrow_mut(|slot| {
                let _ = slot.take();
            });
        }
    }
    PREPARED_MEASUREMENT.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "a thread cannot install two prepared measurements simultaneously");
        *slot = Some(measurement);
    });
    let _reset = Reset;
    body()
}

/// Runs every live mutant in the plan, writing verdicts back onto it.
///
/// # Errors
///
/// Returns an error if the test-oracle selection is invalid or empty, source-declared resources
/// cannot be discovered, the tree cannot be prepared, the build cannot be made to succeed, or the
/// baseline does not pass — a failing baseline means every comparison in the run has nothing to
/// compare against.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn run(survey: &Survey, selection: &Selection, config: &Config, events: &mut impl Events) -> Result<Measured> {
    let mut failed = FailedMeasurement::default();

    run_with_locks(survey, selection, config, events, None, &mut failed, None)
}

#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn run_with_locks(
    survey: &Survey,
    selection: &Selection,
    config: &Config,
    events: &mut impl Events,
    locks: Option<super::workspace::CacheLocks>,
    failed: &mut FailedMeasurement,
    record: Option<&super::testing_state::RecordPreparation>,
) -> Result<Measured> {
    let Measured {
        mut plan,
        built,
        stuck,
        dropped,
        ..
    } = measure_with_locks(survey, selection, config, events, locks, failed)?;

    // Nothing was live, so nothing was copied, built or measured. The plan still describes every
    // mutant that was found and why each one is not being run, which is what the caller reports.
    let Some(mut built) = built else {
        return Ok(Measured {
            plan,
            built: None,
            stuck,
            dropped,
            learning: None,
        });
    };

    let stall = Stall {
        budget: built.session.stall,
    };

    // Taken from the measured run rather than re-derived, so the sweep judges with exactly the
    // oracle the preflight cleared and the baseline timed.
    let oracle = built.oracle.clone();
    let scope = oracle.scope();

    // Loaded here rather than beside the verdicts the record also holds, because the hints are
    // neither read nor written by anything before the sweep: each is a guess about the test suite,
    // checked by running a test, and there is nothing to check before there are binaries to check
    // it with.
    let base = campaign_base(&survey.root, &survey.target, config.cache_dir.as_deref());
    let incremental = config.incremental.is_enabled();
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    let narrowed = !config.whole_test_binaries;

    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    let mut killers = if incremental {
        Killers::load(&base, &survey.root)
    } else {
        Killers::default()
    };
    built.session.phases.initial_hints = Some(super::session::InitialHints::capture(&plan, &killers));

    // The package/binary shape stays fixed throughout the sweep.
    let reach = Reachability::build(&plan, &built.session.binaries, &scope);

    if let Some(record) = record {
        let state = super::testing_state::TestingState::start(
            &plan,
            &built,
            killers,
            super::testing_state::TestingSettings {
                jobs: config.jobs,
                mutators: Some(selection.sorted().join(",")),
                timeout_floor: config.timeout_floor,
                confirm: config.confirm,
                narrowed,
                incremental,
            },
            record,
            (&stuck, &dropped),
        )?;
        killers = state.killers;
        built.work.arm_testing_journal()?;
    }
    built.work.retain_initial_hints(built.session.phases.initial_hints.clone());

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    announce_measurement(events, &plan, &built.session);

    let sweep = Sweep {
        timeout_floor: config.timeout_floor,
        stall,
        jobs: config.jobs,
        meter: built.session.metered,
        confirm: config.confirm,
    };

    let sweep_started = Instant::now();
    let swept = test_all(&built.work, &mut plan, &reach, sweep, narrowed, &mut killers, events);
    let sweep_elapsed = sweep_started.elapsed();

    let spent = match swept {
        Ok(spent) => spent,
        Err(failure) => {
            // An incomplete campaign has no completed record to carry its partial learning.

            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
            store_incomplete_learning(incremental, &killers, &base, &plan.mutants);
            failed.plan = Some(plan);

            return Err(failure);
        }
    };

    // `None` when nothing was swept: the phase is recorded as absent rather than as a real phase
    // that happened to cost nothing, which is what [`Phases::sweep`] documents its `Option` to mean.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    built.session.phases.sweep = built.work.with_sweep_diagnostics(sweep_cost(spent, sweep_elapsed));
    built.work.complete_testing()?;

    Ok(Measured {
        plan,
        built: Some(built),
        stuck,
        dropped,
        learning: Some(killers),
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn resume_testing(
    mut state: super::testing_state::TestingState,
    mut workspace: Workspace,
    events: &mut impl Events,
) -> Result<Measured> {
    let scope = state.oracle.scope();
    workspace.retain_sweep_cost(state.session.phases.sweep.clone());
    workspace.retain_initial_hints(state.session.phases.initial_hints.clone());
    events.sweep_planning(&state.plan, state.session.binaries.len(), state.settings.jobs);
    let reach = Reachability::build(&state.plan, &state.session.binaries, &scope);

    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
    announce_measurement(events, &state.plan, &state.session);
    let started = Instant::now();
    let sweep = Sweep {
        timeout_floor: state.settings.timeout_floor,
        stall: Stall {
            budget: state.session.stall,
        },
        jobs: state.settings.jobs,
        meter: state.session.metered,
        confirm: state.settings.confirm,
    };
    let swept = test_all(
        &workspace,
        &mut state.plan,
        &reach,
        sweep,
        state.settings.narrowed,
        &mut state.killers,
        events,
    );
    let spent = match swept {
        Ok(spent) => spent,
        Err(failure) => {
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
            store_incomplete_learning(state.settings.incremental, &state.killers, &state.record.base, &state.plan.mutants);
            return Err(failure);
        }
    };
    state.session.phases.sweep =
        workspace.with_sweep_diagnostics(SweepCost::combine(state.session.phases.sweep, sweep_cost(spent, started.elapsed())));
    workspace.complete_testing()?;
    Ok(Measured {
        plan: state.plan,
        built: Some(Built {
            work: workspace,
            session: state.session,
            oracle: state.oracle,
        }),
        learning: Some(state.killers),
        stuck: state.stuck,
        dropped: state.dropped,
    })
}

pub(super) fn sweep_cost(spent: Option<super::sweep::Spent>, elapsed: Duration) -> Option<SweepCost> {
    let spent = spent?;

    Some(SweepCost {
        confirmed_initial_killers: spent.confirmed_initial_killers,
        accounting: spent.accounting,
        elapsed,
        launches: spent.launches,
        probes: spent.probes,
        exact_probes: spent.exact_probes,
        exact_hits: spent.exact_hits,
        inferred_probes: spent.inferred_probes,
        inferred_hits: spent.inferred_hits,
        inferred_rejected: spent.inferred_rejected,
        exact_candidates: spent.exact_candidates,
        inferred_candidates: spent.inferred_candidates,
        item_candidates: spent.item_candidates,
        reach_candidates: spent.reach_candidates,
        file_candidates: spent.file_candidates,
        item_probes: spent.item_probes,
        item_hits: spent.item_hits,
        reach_probes: spent.reach_probes,
        reach_hits: spent.reach_hits,
        file_probes: spent.file_probes,
        file_hits: spent.file_hits,
        item_rejected: spent.item_rejected,
        reach_rejected: spent.reach_rejected,
        file_rejected: spent.file_rejected,
        whole_selections: spent.whole_selections,
        uncovered_selections: spent.uncovered_selections,
        packages: spent
            .packages
            .into_iter()
            .map(|package| PackageSweepCost {
                package: package.package,
                mutants: package.mutants,
                first_started_ms: package.first_started_ms,
                settled_ms: package.settled_ms,
            })
            .collect(),
        launches_saved: spent.saved,
        reach_launches_saved: spent.reach_saved,
    })
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn announce_measurement(events: &mut impl Events, plan: &Plan, session: &Session) {
    events.measured(plan, session);
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn store_learning(killers: &Killers, base: &camino::Utf8Path, mutants: &[crate::model::Mutant]) {
    killers.store(base, mutants);
}

fn store_incomplete_learning(incremental: bool, killers: &Killers, base: &camino::Utf8Path, mutants: &[crate::model::Mutant]) {
    if incremental {
        // #[gamma::skip(call_result.default, reason = "store_learning returns unit and this mutator retains its cache publication effects before replacing unit with unit")]
        store_learning(killers, base, mutants);
    }
}

/// What a run worked out before testing a single mutant.
#[derive(Debug)]
pub struct Measured {
    /// Every mutant that was found, whether or not it will be run.
    pub plan: Plan,

    /// The tree and the measurements, absent when there was nothing live to build for.
    pub built: Option<Built>,

    /// The builds this run could not make compile, in the order it gave up on them.
    ///
    /// Empty for a run that built everything it was asked to, which is the ordinary case. A run
    /// that could not converge a build would otherwise end here with an error and nothing else — no
    /// report, no annotations, no score, after an hour of work on a large workspace. The population
    /// it gave up on is recorded as [`Outcome::NotBuilt`] instead, and this is what the caller says
    /// out loud and fails the run over.
    pub stuck: Vec<String>,

    /// Test packages the preflight had to drop before the tree would check at all.
    ///
    /// Empty for the ordinary run. When it is not, some package the caller never asked to mutate
    /// does not compile, and the run went ahead over the packages that do rather than refusing to
    /// start. Their test targets were neither built nor run, so a mutant one of them would have
    /// killed is reported as surviving — which is a gap in this run's oracle and not a gap in the
    /// suite. Unlike [`Self::stuck`] this does not fail the run; it qualifies it, so it is carried
    /// into the report rather than only printed.
    pub dropped: Vec<String>,

    pub(crate) learning: Option<Killers>,
}

/// A built tree and what measuring it revealed.
#[derive(Debug)]
pub struct Built {
    /// The scratch tree. It owns the built binaries, so dropping it deletes them.
    pub work: Workspace,

    /// The timings and test binaries the run works from.
    pub session: Session,

    /// The oracle the measured run settled on.
    ///
    /// Carried rather than recomputed because settling it needs things the caller does not have:
    /// the run's package selection, and whatever the preflight had to give up on to make the tree
    /// compile at all. Deriving it a second time from the configuration would let the sweep consult
    /// binaries the measured run never cleared.
    pub oracle: Oracle,
}

/// State retained when measurement fails after discovering and building the population.
#[derive(Debug, Default)]
pub(crate) struct FailedMeasurement {
    pub(crate) plan: Option<Plan>,
    pub(crate) session: Option<Session>,
    pub(crate) work: Option<Workspace>,
}

struct SessionContext {
    build: Duration,
    metered: bool,
    unbounded: Option<String>,
    filtered: usize,
    copy: Duration,
    preflight: Duration,
}

fn session_before_sweep(baseline: Baseline, build: Build, work: &Workspace, context: SessionContext, stall: Option<Duration>) -> Session {
    Session {
        baseline: baseline.elapsed,
        baseline_wall: baseline.wall,
        tests: baseline.tests,
        quiet: baseline.quiet,
        stall,
        build: context.build,
        peak: baseline.peak,
        metered: context.metered,
        unbounded: context.unbounded,
        withdrawn: build.withdrawn,
        census: build.census,
        rounds: build.rounds,
        rounds_taken: build.history,
        binaries: build.binaries,
        scratch: work.base().to_owned(),
        filtered: context.filtered,
        widened: build.widened,
        ordering: build.ordering,
        phases: Phases {
            initial_hints: None,
            copy: context.copy,
            preflight: context.preflight,
            sweep: None,
        },
    }
}

/// The packages whose tests decide verdicts, owned so a later phase can borrow it back.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Oracle {
    /// The `--test-package` restriction, empty when nothing is restricted.
    pub packages: Vec<String>,

    /// Whether each mutant is judged only by tests from its own package.
    pub package_local: bool,

    /// Whether `--test-workspace` lifted the cap, so every package's tests may decide verdicts.
    ///
    /// It lifts the cap only. A binary still does not decide a verdict on code it cannot link, so a
    /// mutant no binary reaches is uncovered here exactly as it is under the cap.
    pub whole_workspace: bool,
}

impl Oracle {
    fn new(packages: Vec<String>, package_local: bool, whole_workspace: bool) -> Self {
        Self {
            packages,
            package_local,
            whole_workspace,
        }
    }

    /// Borrows this as the scope the reachability filters take.
    fn scope(&self) -> TestScope<'_> {
        TestScope {
            packages: &self.packages,
            package_local: self.package_local,
            whole_workspace: self.whole_workspace,
        }
    }
}

/// Checks that the tree compiles before a single mutant is applied to it.
///
/// This is what lets every later compiler error be absorbed. The instrumented build compiles this
/// same tree with guards written into it, so once this passes, an error that appears afterwards was
/// introduced by a mutant and the rollback loop can withdraw it without troubling anyone. Without
/// it, a tree whose own test targets do not compile fails in the middle of a run and reports a
/// mutant it cannot name, which sends the reader hunting through their own source for a fault that
/// was there before the tool arrived.
///
/// The packages named are the ones the final build will compile, but computed from the packages
/// this run intends to mutate rather than from the ones that turn out to hold live mutants. That
/// is a superset, and a superset is the safe direction: a target left unchecked here would have
/// its genuine errors absorbed later as though a mutant had caused them.
/// Returns the packages the check had to give up on to succeed at all, empty in the ordinary case.
/// They are the caller's problem as much as the tool's: see [`Converger::preflight`].
#[cfg_attr(coverage_nightly, coverage(off))]
fn preflight(
    survey: &Survey,
    plan: &Plan,
    work: &Workspace,
    config: &Config,
    converger: &mut Converger,
    events: &mut impl Events,
) -> Result<Cleared> {
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    begin_preflight(events);

    let requested: Vec<String> = oracle_packages(&survey.selected, config);
    let package_local = inferred_package_local(config);
    let scope = TestScope {
        packages: &requested,
        package_local,
        whole_workspace: config.test_workspace,
    };

    let intended = survey.packages();
    let intending: crate::HashSet<&str> = intended.iter().map(String::as_str).collect();
    let checking = reaching_packages(&survey.reach, &intending, &scope);
    converger.freeze_context(checking.as_deref());
    let cleared = Converger::preflight(work, plan, checking.as_deref(), &intended, config.test_lib, config.build, events)?;
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    end_preflight(events);

    // A check that only passed after widening requires the instrumented build to stay wide too;
    // narrowing there would reproduce a failure already shown to belong to the scope rather than
    // to any mutant.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    settle_preflight_scope(converger, cleared.whole_workspace);

    // The check only passed because it stopped asking about those packages, so the rest of the run
    // stops asking too. Building test targets this run has decided cannot convict anything would be
    // paying again for the compile that just failed, and running them would mean running binaries
    // the preflight never cleared — the one thing the preflight exists to prevent.
    finish_preflight(requested, intended, config.test_workspace, cleared.dropped)
}

fn begin_preflight(events: &mut impl Events) {
    const ACTIVE: &str = "Validating";
    const COMPLETE: &str = "Validated";
    const DETAIL: &str = "the workspace";
    events.begin(ACTIVE, COMPLETE, DETAIL);
}

fn end_preflight(events: &mut impl Events) {
    events.end(Default::default());
}

fn inferred_package_local(config: &Config) -> bool {
    config.test_packages.is_empty() && !config.test_workspace
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn settle_preflight_scope(converger: &mut Converger, whole_workspace: bool) {
    if whole_workspace {
        Converger::require_whole_workspace(converger);
    }
}

fn finish_preflight(requested: Vec<String>, intended: Vec<String>, whole_workspace: bool, dropped: Vec<String>) -> Result<Cleared> {
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    let packages = if dropped.is_empty() {
        requested
    } else {
        narrowed_oracle(requested, intended, &dropped)?
    };
    Ok(Cleared {
        packages,
        whole_workspace,
        dropped,
    })
}

/// The oracle a preflight retreat leaves behind: what was asked for, less what would not compile.
///
/// Subtraction, never substitution. An explicit `--test-package` can name packages disjoint from the
/// mutated ones, and replacing the list with the mutated packages would hand the run the very test
/// binaries the caller opted out of while withdrawing every one they asked for — an oracle nobody
/// chose, reported as though they had.
///
/// An unrestricted oracle is the one case a subtraction cannot be spelled, since an empty list means
/// "everything" rather than "everything so far". There the checked set is named outright, which is a
/// narrowing of "everything" and so is still only ever a subtraction.
///
/// # Errors
///
/// When nothing survives the subtraction. Continuing would judge every mutant against no tests at
/// all and report the lot as uncovered, which reads as a fact about the code rather than about a
/// build that did not happen.
fn narrowed_oracle(requested: Vec<String>, intended: Vec<String>, dropped: &[String]) -> Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(intended);
    }

    let kept: Vec<String> = requested.into_iter().filter(|package| !dropped.contains(package)).collect();

    if kept.is_empty() {
        let named = dropped.join("`, `");

        return Err(error!(
            "none of the packages this run's tests would come from could be compiled: `{named}`.\n\
             Fix the build there, or name a package that does build with --test-package."
        ));
    }

    Ok(kept)
}

/// What the preflight settled: the scope the rest of the run works in, and what it cost to get it.
struct Cleared {
    /// The `--test-package` restriction, narrowed if the check had to retreat to succeed.
    packages: Vec<String>,

    /// Whether every admitted binary still runs for every mutant.
    whole_workspace: bool,

    /// The packages given up on, empty in the ordinary run. See [`Measured::dropped`].
    dropped: Vec<String>,
}

/// Scans, instruments, builds and measures the baseline, without testing a single mutant.
///
/// Packages are scanned in an order where each one follows what it depends on, then the complete
/// selected population is instrumented and converged together. That order is forced anyway — a mutant
/// cannot produce a diagnostic until everything its own package depends on compiles clean — and
/// following it deliberately lets a run say which package it is working on, and say it once, before
/// the wait rather than after it.
///
/// The tree is copied on demand rather than up front. A run whose every mutant turns out to be
/// suppressed has nothing to build, and it should not pay for a copy to discover that.
///
/// # Errors
///
/// Returns an error if a file cannot be parsed, the test-oracle selection is invalid or empty,
/// source-declared resources cannot be discovered, the tree cannot be prepared, the build cannot
/// be made to succeed, or the baseline does not pass — a failing baseline means every comparison
/// in the run has nothing to compare against.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn measure(survey: &Survey, selection: &Selection, config: &Config, events: &mut impl Events) -> Result<Measured> {
    let mut failed = FailedMeasurement::default();

    measure_with_locks(survey, selection, config, events, None, &mut failed)
}

fn validate_test_oracle(tests: &[String], libraries: &[LibraryTestTarget], selected: &[String], config: &Config) -> Result<()> {
    let library_tests;
    let declared_tests = if config.test_lib {
        library_tests = admitted_library_tests(libraries, selected, config);
        library_tests.as_slice()
    } else {
        tests
    };
    if let Some(pattern) = unmatched_test(declared_tests, &config.include_tests, &config.exclude_tests) {
        return Err(error!("no test target matches `{pattern}`; patterns match cargo target names, not test function names").usage());
    }

    if config.test_lib && declared_tests.is_empty() {
        return Err(error!(
            "`--test-lib` selected no runnable library unit-test harness; ensure an oracle package \
             has a library target with `test = true`"
        )
        .usage());
    }

    Ok(())
}

fn admitted_library_tests(tests: &[LibraryTestTarget], selected: &[String], config: &Config) -> Vec<String> {
    let requested = oracle_packages(selected, config);
    let mut targets: Vec<String> = tests
        .iter()
        .filter(|test| config.test_workspace || requested.contains(&test.package))
        .map(|test| test.target.clone())
        .collect();
    targets.sort();
    targets.dedup();
    targets
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[expect(
    clippy::too_many_lines,
    reason = "the linear preparation sequence keeps validation, staging, measurement, and cleanup ordering visible"
)]
fn measure_with_locks(
    survey: &Survey,
    selection: &Selection,
    config: &Config,
    events: &mut impl Events,
    locks: Option<super::workspace::CacheLocks>,
    failed: &mut FailedMeasurement,
) -> Result<Measured> {
    #[cfg(test)]
    if let Some(measured) = PREPARED_MEASUREMENT.with_borrow_mut(Option::take) {
        return Ok(measured);
    }
    let started = Instant::now();

    let support = runner_memory_support(
        config.nextest,
        cfg!(target_os = "macos"),
        memory::support().map_err(|reason| reason.to_string()),
    );
    let (memory, unbounded) = settle_memory_control(config, support)?;

    // Resource discovery also uses the baseline limit, even with --no-baseline, so those
    // launches still need the soft-bound warning. Enforcement reaches here only with a
    // baseline or an explicit ceiling: settling above disables it otherwise, or refuses
    // the run when the user insisted.
    #[cfg(target_os = "macos")]
    if memory.enforcing() || memory.baseline_limit.is_some() {
        events.warn(&format!(
            "memory enforcement on macOS samples the process group's physical footprint every {} ms: \
             the bound is soft, allocation can overshoot between samples, descendants that leave \
             the process group are neither counted nor stopped, and members owned by another user \
             cannot be read and are not counted",
            cargo_gamma_process::SAMPLE_INTERVAL.as_millis(),
        ));
    }

    // Checked against what the workspace declares, before anything is copied or compiled. A typo
    // here changes which tests get to convict a mutant, so it should cost a second rather than a
    // full instrumented build.
    validate_test_oracle(&survey.tests, &survey.library_tests, &survey.selected, config)?;

    // Said before the tree is copied, let alone built. A compile-fail target is expensive per
    // mutant rather than once, so by the time its cost is visible in the progress display the run
    // has already committed hours to it — and what it looks like from outside is a run that hung.
    // Checked against the oracle and its filters so that unrelated workspace packages and targets
    // the caller has already excluded are not presented as costs of this run.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    announce_compile_fail_costs(survey, config, events);

    let mut plan = survey.skeleton();
    let mut converger = configured_converger(config);

    // The scratch-tree copy, timed on its own: it is a component of the build's total, but a large
    // workspace can spend as much duplicating itself as compiling, and the aggregate cannot say
    // which.
    let copy_started = Instant::now();
    let mut work = Workspace::prepare_with_locks(&plan.root, &survey.target, config, events, locks)?;
    let mut copy = Duration::ZERO;
    let exclusions =
    // #[gamma::skip(try.propagate_to_unwrap, reason = "This exact measurement-orchestration propagation follows real workspace preparation, compiler inventory, and runtime linking. Forcing its failure at this caller requires filesystem/process inputs; owned inventory-order/error tests exercise the injected policy separately.", tag = "native-boundary")]
    prepare_runtime_inventory(
        &work,
        survey,
        &plan,
        prepare_compiler_exclusions,
        |work| {

            // #[gamma::skip(stmt.delete_assign, reason = "This assignment samples native elapsed workspace-copy time; distinguishing its omission requires a real-clock observation in the preparation pipeline, which deterministic tests must not depend on.", tag = "native-boundary")]
            copy = copy_started.elapsed();
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            work.begin_build_sequence();
            // Settled once, before anything is spawned, so the baseline and the sweep cannot disagree about
            // how wide the workload they measure and judge is.
            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            calibrate_harness(work, config.jobs);
        },
        link_package_runtimes,
    )?;

    let preflight_started = Instant::now();
    let Cleared {
        packages,
        whole_workspace,
        dropped,
    } = preflight(survey, &plan, &work, config, &mut converger, events)?;
    let preflight_elapsed = preflight_started.elapsed();
    let oracle = Oracle::new(packages, inferred_package_local(config), whole_workspace);
    let scope = oracle.scope();

    let anything_live = instrument_compiler_population(survey, selection, &mut plan, &work, events, &mut converger, exclusions)?;
    let mut stuck = Vec::new();

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    reorder_plan(&mut plan, &mut converger);

    // Nothing was live anywhere, or everything live was in a build that could not be made to
    // compile. Either way there is nothing left to build, measure or run, and the verdicts the
    // abandoned population carries are already written onto the plan.
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    if !anything_live {
        converger.settle(&mut plan);

        return Ok(Measured {
            plan,
            built: None,
            stuck,
            dropped,
            learning: None,
        });
    }

    // Check the complete schema to convergence, then generate its test binaries. The successful
    // build's artifact stream is retained for the baseline, so convergence does not end with a
    // redundant cache-hit Cargo invocation.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    begin_schema_build(events, converger.historical_exclusions);

    // Build only packages whose tests can actually be selected: the rest would be compiled,
    // baselined and never consulted. All target packages have already been instrumented, so this
    // one convergence sees every mutation that can affect those test targets.
    let select = build_packages(&plan, &scope);
    let mut build = converger.finish(&work, &mut plan, select.as_deref(), config.build, events)?;
    // From this point onward compiler convergence has produced durable score-neutral learning.
    // If a later preparation step fails, move the plan into `failed` on that error path so the
    // command layer can publish the learning without retaining a second full population.

    // The build that decides the run could not be made to compile, so there is no test binary to
    // judge anything with and nothing left to measure. Every mutant still live carries
    // `NotBuilt`, which is what tells the reader that these are mutants nobody ran rather than
    // mutants the suite let through, and the run reports that rather than exiting empty-handed.
    if let Some(abandoned) = build.stuck {
        let count = abandoned.ordinals.len();

        stuck.push(describe_stuck(&plan, select.as_deref().unwrap_or_default(), &abandoned));
        let detail = format!(
            "the build could not be made to compile, {} not run",
            crate::report::quantity(count, "mutant")
        );
        events.complete(&detail);

        return Ok(Measured {
            plan,
            built: None,
            stuck,
            dropped,
            learning: None,
        });
    }

    let (unviable, viable, not_built) = schema_build_counts(&plan.mutants);
    // #[gamma::skip(call_result.default, reason = "finish_schema_build returns unit and this mutator retains the completion event before replacing unit with unit")]
    finish_schema_build(events, unviable, viable, not_built);

    // Before the baseline, so the shares `apportion` computes describe the suite that will actually
    // run. A run with nothing left to run it cannot decide anything: every mutant would survive
    // unopposed and the report would read as a total failure of the test suite rather than as the
    // filter having eaten it.
    if config.test_lib && build.binaries.is_empty() {
        let error = error!(
            "`--test-lib` selected no runnable library unit-test harness; ensure a selected package \
             has a library target with `test = true`"
        )
        .usage();
        failed.plan = Some(plan);

        return Err(error);
    }

    let filtered = match restrict_binaries(&mut build.binaries, config) {
        Ok(filtered) => filtered,
        Err(error) => {
            failed.plan = Some(plan);

            return Err(error);
        }
    };

    let build_time = started.elapsed();

    // The build above already produced the binaries. This phase now describes only execution, so a
    // test counter cannot sit at its completed value while Cargo is still compiling.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    begin_baseline(events);

    // Armed after the build and before the baseline. The metadata nextest is handed describes
    // binaries that do not exist until the build has run, and the baseline has to be measured
    // through the same runner that will judge every mutant — a baseline taken one way and compared
    // against verdicts reached the other measures nothing.
    if let Err(error) = arm_selected_runner(&mut work, &build.binaries, &build.artifacts, config.nextest) {
        failed.plan = Some(plan);

        return Err(error);
    }
    // Marker enumeration requires that finalized launch environment. Install the resulting policy
    // before baseline measurement so baseline, mutant, and confirmation launches all use
    // the same admission rules.
    let resources = match Resources::discover(
        &work,
        &build.binaries,
        &config.resources,
        MemoryRequest {
            calibrating: false,
            meter: memory.measuring(),
            limit: memory.baseline_limit,
        },
    ) {
        Ok(resources) => resources,
        Err(error) => {
            failed.plan = Some(plan);

            return Err(error);
        }
    };
    work.set_resources(resources);

    let session_context = SessionContext {
        build: build_time,
        metered: memory.measuring(),
        unbounded,
        filtered,
        copy,
        preflight: preflight_elapsed,
    };
    let mut failed_baseline = None;
    let baseline = match take_baseline_retaining(&work, &mut build.binaries, config, &memory, Some(&mut failed_baseline), events) {
        Ok(baseline) => baseline,
        Err(error) => {
            let baseline = failed_baseline.expect("baseline aggregation retains its completed measurements before returning a failure");
            failed.session = Some(session_before_sweep(baseline, build, &work, session_context, None));
            failed.plan = Some(plan);
            failed.work = Some(work);

            return Err(error);
        }
    };

    events.sweep_planning(&plan, build.binaries.len(), config.jobs);

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    warn_if_oracle_is_empty(&plan, &build.binaries, &scope, config, &dropped, events);

    let stall = calibrate_stall(&baseline, config);

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    calibrate_binaries(&mut build.binaries, config, &memory);

    let session = session_before_sweep(baseline, build, &work, session_context, stall.budget);

    // Everything that could have failed has. What was built is now worth keeping, so that the next
    // run in this workspace is incremental rather than starting cold.
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    settle_workspace(&work);

    Ok(Measured {
        plan,
        built: Some(Built { work, session, oracle }),
        stuck,
        dropped,
        learning: None,
    })
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn announce_compile_fail_costs(survey: &Survey, config: &Config, events: &mut dyn Events) {
    let targets = survey.compile_fail.as_slice();
    warn_about_compile_fail_targets(targets, &survey.selected, config, events);
}

fn validate_synchronized_sources(work: &Workspace, digests: &HashMap<Utf8PathBuf, String>) -> Result<()> {
    for (path, expected) in digests {
        let synchronized = work.root.join(path);
        let source = fs::read_to_string(synchronized.as_std_path())
            .map_err(|cause| error!("could not validate synchronized source `{path}`").caused_by(cause))?;

        if crate::discover::digest(crate::parse::strip_bom(&source).as_bytes()) != *expected {
            return Err(error!(
                "source `{}` changed between discovery and workspace synchronization; rerun cargo gamma after edits have settled",
                path
            ));
        }
    }

    Ok(())
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn calibrate_harness(work: &Workspace, jobs: usize) {
    work.calibrate_harness(jobs);
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn reorder_plan(plan: &mut Plan, converger: &mut Converger) {
    plan.sort();
    converger.plan_reordered();
}

fn begin_baseline(events: &mut impl Events) {
    const ACTIVE: &str = "Baselining";
    const COMPLETE: &str = "Baseline";
    const DETAIL: &str = "running the unmutated suite";
    events.begin(ACTIVE, COMPLETE, DETAIL);
}

fn restrict_binaries(binaries: &mut Vec<TestBinary>, config: &Config) -> Result<usize> {
    let present = binaries.len();
    restrict(binaries, &config.include_tests, &config.exclude_tests);
    let filtered = present.saturating_sub(binaries.len());
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    if binaries.is_empty() && present > 0 {
        return Err(error!("`--include-test` and `--exclude-test` left no test target to decide a verdict").usage());
    }
    Ok(filtered)
}

// #[gamma::skip(all, reason = "the mutation affects internal orchestration state with no safely deterministic observation at this layer")]
fn arm_selected_runner(work: &mut Workspace, binaries: &[TestBinary], artifacts: &str, nextest: bool) -> Result<()> {
    work.arm_test_environment(binaries, artifacts)?;

    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    if nextest {
        work.arm_nextest(binaries)?;
    }
    Ok(())
}

fn warn_if_oracle_is_empty(
    plan: &Plan,
    binaries: &[TestBinary],
    scope: &TestScope<'_>,
    config: &Config,
    dropped: &[String],
    events: &mut dyn Events,
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
) {
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    warn_about_an_empty_oracle(plan, binaries, scope, config.test_packages.is_empty(), dropped, events);
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn calibrate_binaries(binaries: &mut [TestBinary], config: &Config, memory: &MemoryPolicy) {
    calibrate(binaries, config, memory);
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn settle_workspace(work: &Workspace) {
    Workspace::settle(work);
}

/// Keeps a failed run's workspace alive until the command layer has persisted its diagnostics.
/// Renders what a build that could not be made to compile cost, and where it got stuck.
///
/// The reason is repeated verbatim: the rollback-limit failure's withdrawal series and its
/// falling-or-flat advice, and the unattributed failure's excerpt of what cargo actually said, are
/// the only text that tells a reader whether to raise a limit, fix a build script, or look
/// somewhere else entirely. Losing it would leave a run that says it got stuck without saying on
/// what.
///
/// Grouped by mutator and by enclosing item, because "we could not build 900 mutants" is not
/// actionable and "every one of them was the `stmt.delete` mutator, all in `parser::Lexer`" is: it
/// names one operator to exclude, or one module to look at, and either of those turns a dead run
/// into a run that finishes.
fn describe_stuck(plan: &Plan, packages: &[String], abandoned: &Abandoned) -> String {
    let ordinals: crate::HashSet<u32> = abandoned.ordinals.iter().copied().collect();
    let affected: Vec<&crate::model::Mutant> = plan.mutants.iter().filter(|mutant| ordinals.contains(&mutant.ordinal)).collect();

    let where_ = if packages.is_empty() {
        "the workspace".to_owned()
    } else {
        format!("{} {}", crate::report::quantity(packages.len(), "package"), packages.join(", "))
    };

    let mut described = format!(
        "the build for {where_} could not be made to compile, so {} never ran and {} reported as `notbuilt` rather than as survivors.",
        crate::report::quantity(abandoned.ordinals.len(), "mutant"),
        if abandoned.ordinals.len() == 1 { "is" } else { "are" }
    );

    for (label, counted) in [
        ("mutator", tally(&affected, |mutant| mutant.mutator.to_string())),
        ("scope", tally(&affected, |mutant| mutant.item_path.to_string())),
    ] {
        if !counted.is_empty() {
            let _ = write!(described, "\nNot run, by {label}: {}.", counted.join(", "));
        }
    }

    described.push('\n');
    described.push_str(&abandoned.reason);
    described
}

/// How many mutants fall under each key, commonest first, capped so one line stays one line.
fn tally(mutants: &[&crate::model::Mutant], key: impl Fn(&crate::model::Mutant) -> String) -> Vec<String> {
    let mut counts: crate::HashMap<String, usize> = crate::HashMap::default();

    for mutant in mutants {
        *counts.entry(key(mutant)).or_default() += 1;
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();

    // Ties broken by name so that two runs over the same tree read the same way; a hash map's order
    // is not something a report should inherit.
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    let rest = ranked.len().saturating_sub(GROUP_LIMIT);
    let mut rendered: Vec<String> = ranked
        .iter()
        .take(GROUP_LIMIT)
        .map(|(name, count)| format!("{name} ({count})"))
        .collect();

    if rest > 0 {
        rendered.push(format!("and {rest} more"));
    }

    rendered
}

/// Scans and instruments the complete target population before asking Cargo to build it.
///
/// Packages are still scanned in dependency order for deterministic ordinals, but compilation is
/// deliberately deferred until every target package is present. One workspace-wide convergence
/// can then expose independent compiler failures together and its successful final round produces
/// the exact test binaries consumed by the baseline.
#[cfg_attr(coverage_nightly, coverage(off))]
fn instrument_population(
    survey: &Survey,
    selection: &Selection,
    plan: &mut Plan,
    work: &Workspace,
    events: &mut impl Events,
    exclusions: Option<&crate::discover::CompilerExclusions>,
) -> Result<bool> {
    let mut ordinals = u32::default();
    let source_indices = plan.source_indices();
    let packages = survey.packages();
    let mut live = usize::default();

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    begin_population(events, packages.len());
    for stage in &crate::discover::stages(&packages, &survey.reach) {
        for package in stage {
            let scanned = survey.scan_excluding(Some(package), selection, &mut ordinals, exclusions)?;

            live = live.saturating_add(live_mutants(&scanned.mutants));
            validate_synchronized_sources(work, &scanned.digests)?;
            plan.absorb_indexed(scanned, &source_indices);
        }
    }

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    finish_population(events, &plan.mutants);
    Ok(live > usize::default())
}

fn configured_converger(config: &Config) -> Converger {
    let mut converger = Converger::default();
    converger.select_test_lib(config.test_lib);
    converger
}

/// Prepares retained exclusions before build setup and runtime linking; the first failure stops setup.
fn prepare_runtime_inventory(
    work: &Workspace,
    survey: &Survey,
    plan: &Plan,
    prepare: impl FnOnce(&Workspace, &Plan) -> Result<crate::discover::CompilerExclusions>,
    after_prepare: impl FnOnce(&Workspace),
    link: impl FnOnce(&Workspace, &Survey, &Plan) -> Result<()>,
) -> Result<crate::discover::CompilerExclusions> {
    let exclusions = prepare(work, plan)?;
    after_prepare(work);
    link(work, survey, plan)?;
    Ok(exclusions)
}

fn link_package_runtimes(work: &Workspace, survey: &Survey, plan: &Plan) -> Result<()> {
    link_package_runtimes_with(survey, |package| work.link_runtime(package, &plan.files))
}

fn link_package_runtimes_with(survey: &Survey, mut link: impl FnMut(&str) -> Result<()>) -> Result<()> {
    for package in survey.packages() {
        link(&package)?;
    }
    Ok(())
}

/// Instruments against retained compiler evidence, then transfers it into convergence state.
fn instrument_compiler_population(
    survey: &Survey,
    selection: &Selection,
    plan: &mut Plan,
    work: &Workspace,
    events: &mut impl Events,
    converger: &mut Converger,
    exclusions: crate::discover::CompilerExclusions,
) -> Result<bool> {
    let anything_live = instrument_population(survey, selection, plan, work, events, Some(&exclusions))?;

    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
    retain_compiler_exclusions(converger, plan, exclusions);
    Ok(anything_live)
}

/// Recounts historically excluded mutants in the new plan and retains their reusable evidence.
fn retain_compiler_exclusions(converger: &mut Converger, plan: &Plan, exclusions: crate::discover::CompilerExclusions) {
    converger.historical_exclusions = plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::CompileError).count();
    converger.compiler_exclusions = Some(exclusions);
}

/// Loads and persists retained compiler evidence before instrumenting a new population.
fn prepare_compiler_exclusions(work: &Workspace, plan: &Plan) -> Result<crate::discover::CompilerExclusions> {
    let exclusions = crate::discover::CompilerExclusions::load(work.base(), &plan.root)?;
    exclusions.store(work.base())?;
    Ok(exclusions)
}

fn begin_population(events: &mut impl Events, packages: usize) {
    let detail = crate::report::quantity(packages, "target package");
    // #[gamma::skip(all, reason = "this literal belongs to a Cargo process protocol or diagnostic boundary that cannot be isolated deterministically without replacing process-global integration state")]
    events.begin("Mutating", "Mutated", &detail);
}

fn live_mutants(mutants: &[crate::model::Mutant]) -> usize {
    mutants.iter().filter(|mutant| mutant.ordinal > 0).count()
}

fn finish_population(events: &mut impl Events, mutants: &[Mutant]) {
    let population = mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 || mutant.outcome == Outcome::CompileError)
        .count();
    let detail = format!(", {}", crate::report::quantity(population, "mutant"));
    events.end(&detail);
}

fn schema_build_counts(mutants: &[Mutant]) -> (usize, usize, usize) {
    let unviable = mutants.iter().filter(|mutant| mutant.outcome == Outcome::CompileError).count();
    let viable = mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .count();
    let not_built = mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::NotBuilt)
        .count();
    (unviable, viable, not_built)
}

fn begin_schema_build(events: &mut impl Events, historical_exclusions: usize) {
    events.begin("Excluding", "Excluded", "unviable mutants");
    events.convergence_progress(historical_exclusions);
}

fn finish_schema_build(events: &mut impl Events, unviable: usize, viable: usize, not_built: usize) {
    let mut detail = crate::report::quantity(unviable, "unviable mutant");
    if not_built > 0 {
        let unavailable = if not_built == 1 {
            "1 mutant could not be built".to_owned()
        } else {
            format!("{not_built} mutants could not be built")
        };
        let _ = write!(detail, "; {unavailable}; {viable} are viable");
    } else {
        let _ = write!(detail, ", leaving {}", crate::report::quantity(viable, "viable mutant"));
    }
    events.complete(&detail);
}

/// Says so when a test target is going to run the compiler once per mutant.
///
/// Named and never removed. `trybuild` asserts exact compiler output, so on a proc-macro crate such
/// a target is often the *primary* oracle — a mutant that corrupts a diagnostic message is caught
/// there and nowhere else. Excluding it automatically would gut the oracle for the code the
/// technique suits best, and the mutants would come back as survivors rather than as anything
/// visibly wrong. Whether the catch rate justifies the price is the caller's to weigh, so the run
/// states the price and the flag and leaves the decision where it belongs.
fn warn_about_compile_fail_targets(targets: &[CompileFailTarget], selected: &[String], config: &Config, events: &mut dyn Events) {
    let oracle = oracle_packages(selected, config);
    let admitted: Vec<_> = targets
        .iter()
        .filter(|target| {
            (config.test_workspace || oracle.contains(&target.package))
                && admits_target(&target.target, &config.include_tests, &config.exclude_tests)
        })
        .cloned()
        .collect();

    if let Some(warning) = compile_fail_advice(&admitted) {
        events.warn(&warning);
    }
}

/// How many packages a warning names before it starts counting them instead.
const NAMED_HELPERS: usize = 5;

/// Says so when the oracle cap has left the mutated code with no tests at all.
///
/// Called after the baseline, because the emptiness it reports is a count of tests and nothing
/// before the baseline has counted any.
///
/// The default oracle is the tests `cargo test` would run here, which is the right cap for the
/// ordinary workspace and the wrong one for a workspace that keeps its tests somewhere else — a
/// package of integration tests, or a parent crate that exercises a private implementation crate
/// through its own suite. There the cap withdraws the only tests that could convict anything, and
/// the run is honest about it: every mutant comes back uncovered. Honest is not the same as
/// useful, though, and "nothing tests this" reads like a verdict on the code rather than on the
/// scope the run was given.
///
/// Only said when the cap was inferred rather than asked for, and only when some package outside it
/// could actually have reached the mutated code. A caller who named `--test-package` has already
/// answered this question, and a workspace where nothing else links the code has nothing to offer.
///
/// A preflight retreat is the exception, and it displaces the cap as the explanation. It withdrew
/// packages the caller never asked to lose, so it is worth reporting however the oracle was chosen,
/// and naming the cap instead would send the reader to widen a scope that was never the problem.
fn warn_about_an_empty_oracle(
    plan: &Plan,
    binaries: &[TestBinary],
    scope: &TestScope<'_>,
    inferred: bool,
    dropped: &[String],
    events: &mut dyn Events,
) {
    // A retreat is worth saying whoever chose the oracle, because it withdrew packages the caller
    // never asked to lose. The cap is only worth saying when the run inferred it.
    if dropped.is_empty() && (!inferred || scope.whole_workspace) {
        return;
    }

    let mutated: crate::HashSet<&str> = plan
        .mutants
        .iter()
        .filter(|mutant| mutant.ordinal != 0)
        .filter(|mutant| mutant.outcome == Outcome::Pending)
        .map(|mutant| &*mutant.package)
        .collect();

    let Some(_first_mutated) = mutated.iter().next() else {
        return;
    };

    // A binary that announced tests it never counted is not evidence of an empty suite, so `None`
    // is read as "there may well be tests here" and stops the warning. Saying this over a suite
    // that does convict things would be worse than saying nothing.
    let judged = mutated.iter().any(|package| package_is_judged(binaries, package, plan, scope));

    if judged {
        return;
    }

    // The retreat is the cause whenever there was one: the packages it gave up on are exactly the
    // ones whose tests are no longer there to convict, so blaming the cap would send the reader to
    // widen a scope that was never the problem — and the advice the cap gives, `--test-package <p>`,
    // would name a package that has just failed to compile.
    if !dropped.is_empty() {
        let named = dropped.join("`, `");

        events.warn(&format!(
            "no test in this run's scope reaches the code being mutated, so every mutant will be reported as uncovered.              The preflight could not compile `{named}`, so their tests were withdrawn from the oracle.              Fixing the build there restores them"
        ));

        return;
    }

    let mut helpers: Vec<&str> = plan
        .reach
        .iter()
        .filter(|(package, reachable)| !scope.admits(package) && reachable.iter().any(|name| mutated.contains(name.as_str())))
        .map(|(package, _reachable)| package.as_str())
        .collect();

    if helpers.is_empty() {
        return;
    }

    helpers.sort_unstable();

    // Named rather than counted, and the flag spelled out with a real package in it, because the
    // whole point of the warning is that the next command is one edit away.
    let shown = helpers.len().min(NAMED_HELPERS);
    let named = helpers[..shown].join("`, `");
    let rest = helpers.len() - shown;
    let others = if rest == 0 { String::new() } else { format!(" and {rest} more") };

    events.warn(&format!(
        "no test in this run's scope reaches the code being mutated, so every mutant will be reported as uncovered. By default each mutant is judged only by its own package, and `{named}`{others} also links this code. Add `--test-package {}` to let its tests decide a verdict, or `--test-workspace` for every package's",
        helpers[0]
    ));
}

fn package_is_judged(binaries: &[TestBinary], package: &str, plan: &Plan, scope: &TestScope<'_>) -> bool {
    let available = binaries;
    available
        .iter()
        // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
        .any(|binary| (binary.tests != Some(0)) && reaches(binary, package, plan, scope))
}

/// Runs the suite once with no mutant active, or reports that nothing was measured.
///
/// The measurement is taken at the sweep's own concurrency, because calibrating on an idle machine
/// and spending on a loaded one makes every budget derived from it too tight — and a mutant that
/// loses that race is recorded as a timeout, which lowers the score and can fail the run.
#[cfg(test)]
fn take_baseline(
    work: &Workspace,
    binaries: &mut [TestBinary],
    config: &Config,
    memory: &MemoryPolicy,
    events: &mut impl Events,
) -> Result<Baseline> {
    take_baseline_retaining(work, binaries, config, memory, None, events)
}

fn take_baseline_retaining(
    work: &Workspace,
    binaries: &mut [TestBinary],
    config: &Config,
    memory: &MemoryPolicy,
    failed: Option<&mut Option<Baseline>>,
    events: &mut impl Events,
) -> Result<Baseline> {
    if !config.baseline {
        events.complete("no baseline was measured");

        return Ok(Baseline {
            elapsed: Duration::ZERO,
            wall: Duration::ZERO,
            quiet: Duration::ZERO,
            tests: None,
            peak: None,
        });
    }

    let request = MemoryRequest {
        calibrating: true,
        meter: memory.measuring(),
        limit: memory.baseline_limit,
    };

    let total = binaries.len();
    let unit = if total == 1 { "test binary" } else { "test binaries" };
    let mut completed = 0_usize;

    events.phase_progress(completed, total, unit);

    let measured = measure_baseline(work, binaries, request, config.jobs, failed, || {
        completed += 1;
        events.phase_progress(completed, total, unit);
    })?;

    // Reported after the fact rather than before, because the figures everything downstream is
    // derived from — each test binary's timeout is scaled from its baseline by `--test-timeout-multiplier`, and
    // the stall budget is calibrated from the longest silence within the baseline — only exist once the suite
    // has actually run.
    events.complete(&describe(&measured, total));

    Ok(measured)
}

/// Settles what memory control this run can actually deliver, refusing or degrading as appropriate.
///
/// Two things can make the configured policy impossible: a host with no way to account for a whole
/// process tree, and a run with no baseline to calibrate a ceiling from. What happens then depends
/// on who asked. Someone who passed `--memory` did so because an unbounded mutant would cost them
/// something, and a run that quietly gave them nothing would be discovered only by the thing they
/// were trying to prevent — so that is an error. Someone who passed nothing has the default, and
/// refusing to produce a mutation score because this machine has no cgroup delegation would be an
/// obstruction rather than a safeguard — so that degrades to no memory control, out loud.
///
/// It is said out loud rather than silently because the protection is the kind whose absence is
/// invisible until it matters. A user who believes their machine is protected and finds out
/// otherwise mid-run is worse off than one who was told plainly at the start.
///
/// Host support is passed in rather than probed here so that every branch is decided by its
/// arguments alone: whether a machine happens to have a delegated cgroup then has no bearing on
/// which of these paths a test exercises.
fn settle_memory_control(config: &Config, support: Result<(), String>) -> Result<(MemoryPolicy, Option<String>)> {
    let policy = config.memory;

    if !policy.measuring() {
        return Ok((policy.disabled(), None));
    }

    if let Err(reason) = support {
        if policy.insisted() {
            return Err(error!(
                "memory control was asked for, but it is not available here: {reason}.\n\
                 Run with `--memory off` to continue without it."
            ));
        }

        return Ok((policy.disabled(), Some(reason)));
    }

    // A ceiling is calibrated from the baseline. Without one there is no measurement to calibrate
    // from, and a number invented here would be presented with exactly the confidence of a
    // measured one.
    if policy.enforcing() {
        if config.baseline || policy.limit.is_some() {
            return Ok((policy, None));
        }
        if policy.insisted() {
            return Err(error!(
                "`--memory enforce` derives each test binary's ceiling from what it used during the \
                 baseline, and `--no-baseline` means there is no such measurement.\n\
                 Pass `--memory-limit` to state a ceiling outright, or drop `--no-baseline`."
            ));
        }

        return Ok((
            policy.disabled(),
            Some("`--no-baseline` leaves no measurement to derive a ceiling from".to_owned()),
        ));
    }

    Ok((policy, None))
}

fn runner_memory_support(nextest: bool, macos: bool, support: Result<(), String>) -> Result<(), String> {
    if macos && nextest {
        Err(
            "nextest puts each test in its own process group, outside macOS sampled memory control; drop `--nextest` to use memory control"
                .to_owned(),
        )
    } else {
        support
    }
}

/// Describes what the baseline measured, for the line that replaces its announcement.
///
/// The test count is omitted rather than guessed when no harness announced one, which is what a
/// target built with `harness = false` does.
fn describe(baseline: &Baseline, binaries: usize) -> String {
    let duration = format!("{:.1?}", baseline.wall);
    let binaries = if binaries == 1 {
        "1 test binary".to_owned()
    } else {
        format!("{binaries} test binaries")
    };
    let ran = baseline.tests.map_or_else(
        || format!("the suite across {binaries} passed in {duration}"),
        |tests| format!("{} ran across {binaries} in {duration}", crate::report::quantity(tests, "test")),
    );

    // Reported whenever it was measured, whether or not anything is being enforced. A project
    // deciding whether a ceiling is worth turning on needs to know what its suite actually uses,
    // and this line is where that number is cheapest to notice.
    match baseline.peak {
        Some(peak) => format!("{ran} with a peak of {}", crate::report::bytes(peak)),
        None => ran,
    }
}

/// Derives a mutant's stall budget from what the baseline measured.
///
/// Extracted from [`measure`] because this number decides every stall verdict the
/// run reaches, and it is the only place the tuning constants in [`Config::default`] are read.
/// Inline in `measure` it could only be exercised by a full run, which means the values a user
/// actually runs with were derived by code no assertion could reach; here it is one call with
/// no I/O.
fn calibrate_stall(baseline: &Baseline, config: &Config) -> Stall {
    // Without a baseline there is no calibration, so a stall cannot be detected and every mutant
    // waits out its whole budget.
    if config.stall && config.baseline {
        Stall::calibrated(baseline.quiet, config.stall_factor, config.stall_floor)
    } else {
        Stall::NONE
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::iter::once;
    use std::cell::RefCell;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::rc::Rc;

    use camino::Utf8Path;
    use serde_json::{Value, from_slice, from_value, json, to_value, to_vec};

    use super::*;
    use crate::discover::{Killer, TargetFile};
    use crate::exec::memory::{Demand, MemoryControl};
    use crate::exec::verdict::Only;
    use crate::fixtures;
    use crate::testing::Recorder;

    #[test]
    fn oct25_prepared_runtime_setup_preserves_each_error_before_advancing_the_pipeline() {
        let output = tempfile::tempdir_in(".").unwrap();
        let base = Utf8Path::from_path(output.path()).unwrap();
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery");
        let survey = Survey::from_test_root(root.clone());
        let plan = survey.skeleton();
        let work = Workspace::adopt(root, base.join("target"));
        let failure = prepare_runtime_inventory(
            &work,
            &survey,
            &plan,
            |seen, selected| {
                assert!(std::ptr::eq(seen, &raw const work));
                assert!(std::ptr::eq(selected, &raw const plan));
                Err(error!("owned inventory preparation refusal"))
            },
            |_| panic!("preparation failure must precede build setup"),
            |_, _, _| panic!("preparation failure must precede runtime linking"),
        )
        .unwrap_err();
        assert_eq!(failure.message(), "owned inventory preparation refusal");
        let prepared = std::cell::Cell::new(false);
        let failure = prepare_runtime_inventory(
            &work,
            &survey,
            &plan,
            prepare_compiler_exclusions,
            |seen| {
                assert!(std::ptr::eq(seen, &raw const work));
                prepared.set(true);
            },
            |seen, selected, population| {
                assert!(prepared.get());
                assert!(std::ptr::eq(seen, &raw const work));
                assert!(std::ptr::eq(selected, &raw const survey));
                assert!(std::ptr::eq(population, &raw const plan));
                Err(error!("owned runtime link refusal"))
            },
        )
        .unwrap_err();
        assert_eq!(failure.message(), "owned runtime link refusal");
        assert_eq!(
            from_slice::<Value>(&std::fs::read(base.join("compiler-exclusions.json")).unwrap()).unwrap(),
            json!({"version": 1, "mutants": {}})
        );
    }

    #[test]
    fn recovered_runtime_linking_returns_the_package_error_without_touching_native_manifests() {
        let survey = Survey::from_checked_fixture();
        let mut calls = Vec::new();
        let failure = link_package_runtimes_with(&survey, |package| {
            calls.push(package.to_owned());
            Err(error!("injected runtime manifest read refusal"))
        })
        .unwrap_err();
        assert_eq!(calls, ["alpha"]);
        assert_eq!(failure.message(), "injected runtime manifest read refusal");
    }

    fn recovered_measured_plan() -> (super::super::testing_state::TestingState, Survey, Selection) {
        let state = super::super::testing_state::TestingState::from_test_plan(fixtures::plan(vec![Mutant {
            outcome: Outcome::Pending,
            ..fixtures::mutant()
        }]));
        let survey = Survey::from_test_root(state.plan.root.clone());
        let selection = Selection::parse("literal.bool_flip,arith.add_to_sub").unwrap();
        (state, survey, selection)
    }

    fn recovered_memory_work() -> (Workspace, super::super::testing_state::JournalEntries) {
        let mut work = Workspace::adopt("invalid\0measurement".into(), "invalid\0measurement/target".into());
        let (journal, entries) = super::super::testing_state::TestingJournal::memory_failing_after_for_test(Some(0));
        work.set_journal_for_test(journal);
        (work, entries)
    }

    fn recovered_first_accounting(entries: &super::super::testing_state::JournalEntries) -> Value {
        let entries = entries
            .lock()
            .expect("journal capture only appends or copies owned bytes under this lock")
            .clone();
        assert_eq!(entries.len(), 1);
        from_slice(&entries[0]).unwrap()
    }

    #[test]
    fn recovered_native_run_captures_and_retains_initial_hints_before_its_first_in_memory_checkpoint() {
        let (state, survey, selection) = recovered_measured_plan();
        let (work, entries) = recovered_memory_work();
        let measured = Measured {
            plan: state.plan,
            built: Some(Built {
                work,
                session: state.session,
                oracle: state.oracle,
            }),
            stuck: Vec::new(),
            dropped: Vec::new(),
            learning: None,
        };
        let config = Config {
            incremental: super::super::incremental_mode::IncrementalMode::No,
            ..Config::default()
        };
        let result = with_prepared_measurement(measured, || {
            run_with_locks(
                &survey,
                &selection,
                &config,
                &mut Recorder::default(),
                None,
                &mut FailedMeasurement::default(),
                None,
            )
        });
        let Err(failure) = result else {
            panic!("the first durable checkpoint is deliberately refused");
        };
        assert_eq!(failure.message(), "injected durable append error");
        let entry = recovered_first_accounting(&entries);
        assert_eq!(entry["sweep"]["accounting"]["initialExactHints"], 0);
        assert_eq!(entry["sweep"]["accounting"]["initialKillerConfirmed"], 0);
    }

    #[test]
    fn recovered_resumed_run_retains_prior_cost_and_hint_coverage_before_its_first_in_memory_checkpoint() {
        for retain_prior in [false, true] {
            let (mut state, _survey, _selection) = recovered_measured_plan();
            let (work, entries) = recovered_memory_work();
            state.session.phases.initial_hints = Some(super::super::session::InitialHints {
                population: 1,
                killers: HashMap::default(),
            });
            state.session.phases.sweep = retain_prior.then(|| SweepCost {
                elapsed: Duration::from_millis(37),
                confirmed_initial_killers: std::collections::BTreeSet::from(["retained-confirmation".into()]),
                accounting: Some(super::super::session::SweepAccounting::segment(
                    state.session.phases.initial_hints.as_ref(),
                    true,
                )),
                ..SweepCost::default()
            });
            let result = resume_testing(state, work, &mut Recorder::default());
            let Err(failure) = result else {
                panic!("the first durable checkpoint is deliberately refused");
            };
            assert_eq!(failure.message(), "injected durable append error");
            let entry = recovered_first_accounting(&entries);
            assert_eq!(
                entry["sweep"]["elapsed"],
                to_value(Duration::from_millis(if retain_prior { 37 } else { 0 })).unwrap()
            );
            assert_eq!(entry["sweep"]["accounting"]["initialExactHints"], 0);
            assert_eq!(entry["sweep"]["accounting"]["initialKillerConfirmed"], usize::from(retain_prior));
        }
    }

    #[test]
    fn recovered_native_run_passes_sorted_mutator_settings_and_propagates_checkpoint_start_failure() {
        let (state, survey, selection) = recovered_measured_plan();
        let record = state.record.clone();
        let work = Workspace::adopt("invalid\0measurement".into(), "invalid\0measurement/target".into());
        let measured = Measured {
            plan: state.plan,
            built: Some(Built {
                work,
                session: state.session,
                oracle: state.oracle,
            }),
            stuck: Vec::new(),
            dropped: Vec::new(),
            learning: None,
        };
        let config = Config {
            incremental: super::super::incremental_mode::IncrementalMode::No,
            ..Config::default()
        };
        let result = super::super::testing_state::with_start_observer(
            |settings| {
                assert_eq!(settings.mutators.as_deref(), Some("arith.add_to_sub,literal.bool_flip"));
                Err(error!("the in-memory testing-state start is deliberately refused"))
            },
            || {
                with_prepared_measurement(measured, || {
                    run_with_locks(
                        &survey,
                        &selection,
                        &config,
                        &mut Recorder::default(),
                        None,
                        &mut FailedMeasurement::default(),
                        Some(&record),
                    )
                })
            },
        );
        let Err(failure) = result else {
            panic!("the testing-state observer returned a named error");
        };
        assert_eq!(failure.message(), "the in-memory testing-state start is deliberately refused");
    }

    #[test]
    fn recovered_materialization_preserves_exact_historical_exclusions_and_counts_only_compile_error_outcomes() {
        let survey = Survey::from_checked_fixture();
        let selection = Selection::parse("relational.lt_to_le").unwrap();
        let scanned = survey.scan_excluding(Some("alpha"), &selection, &mut 0, None).unwrap();
        assert_eq!(scanned.mutants.len(), 1);
        let id = scanned.mutants[0].id.clone();
        let exclusions: crate::discover::CompilerExclusions = from_value(json!({
            "version": 1, "mutants": {id.as_str(): "retained exact compiler rejection"}
        }))
        .unwrap();
        let work = Workspace::adopt(survey.root.clone(), "never-written-materialization-target".into());
        let mut plan = survey.skeleton();
        let mut converger = configured_converger(&Config {
            test_lib: true,
            ..Config::default()
        });
        assert!(converger.test_lib);
        assert!(
            !instrument_compiler_population(
                &survey,
                &selection,
                &mut plan,
                &work,
                &mut Recorder::default(),
                &mut converger,
                exclusions,
            )
            .unwrap()
        );
        assert_eq!(converger.historical_exclusions, 1);
        assert_eq!(plan.mutants.len(), 1);
        assert_eq!(plan.mutants[0].id, id);
        assert_eq!(plan.mutants[0].outcome, Outcome::CompileError);
        assert_eq!(plan.mutants[0].ordinal, 0);
        assert_eq!(
            plan.mutants[0].note.as_deref(),
            Some("historical compiler exclusion: retained exact compiler rejection")
        );
        plan.mutants.extend([
            Mutant {
                outcome: Outcome::Pending,
                ..fixtures::mutant()
            },
            Mutant {
                outcome: Outcome::CompileError,
                ..fixtures::mutant()
            },
        ]);
        let exclusions = converger.compiler_exclusions.take().unwrap();
        retain_compiler_exclusions(&mut converger, &plan, exclusions);
        assert_eq!(converger.historical_exclusions, 2);
        assert_eq!(
            converger.compiler_exclusions.as_ref().unwrap().reason(&plan.mutants[0]),
            Some("retained exact compiler rejection")
        );
    }

    #[test]
    fn recovered_schema_build_announces_the_exact_historical_count_after_its_phase_start() {
        #[derive(Default)]
        struct Announcements(Vec<(String, usize)>);
        impl Events for Announcements {
            fn phase(&mut self, verb: &str, _detail: &str) {
                self.0.push((verb.to_owned(), 0));
            }
            fn convergence_progress(&mut self, excluded: usize) {
                self.0.push(("historical".into(), excluded));
            }
            fn mutant(&mut self, _mutant: &Mutant) {
                panic!("announcing the schema build does not settle any mutant");
            }
        }
        let mut events = Announcements::default();
        begin_schema_build(&mut events, 7);
        assert_eq!(events.0, [("Excluding".into(), 0), ("historical".into(), 7)]);
    }

    #[test]
    fn recovered_compiler_inventory_preparation_returns_native_load_and_store_failures_without_creating_an_inventory() {
        let survey = Survey::from_checked_fixture();
        let plan = survey.skeleton();
        let invalid_base = Utf8Path::new("invalid\0compiler-inventory");
        let invalid_work = Workspace::adopt(plan.root.clone(), invalid_base.join("target"));
        let expected = crate::discover::CompilerExclusions::load(invalid_base, &plan.root)
            .unwrap_err()
            .message()
            .to_owned();
        assert_eq!(prepare_compiler_exclusions(&invalid_work, &plan).unwrap_err().message(), expected);

        #[cfg(windows)]
        {
            let base_file = survey.files[0].absolute.clone();
            let work = Workspace::adopt(survey.root, base_file.join("target"));
            let inventory = crate::discover::CompilerExclusions::load(&base_file, &plan.root).unwrap();
            let expected = inventory.store(&base_file).unwrap_err().message().to_owned();
            assert_eq!(prepare_compiler_exclusions(&work, &plan).unwrap_err().message(), expected);
            assert_eq!(
                std::fs::read(&base_file).unwrap(),
                include_bytes!("../../tests/fixtures/completed-recovery/lib.rs")
            );
        }
    }

    #[test]
    fn oct25_preflight_distinguishes_implicit_local_policy_from_a_selected_nonmutating_dependent() {
        for (whole_workspace, explicit_package, expected_selection) in [
            (false, None, vec!["--package", "alpha"]),
            (true, None, vec!["--workspace"]),
            (false, Some("beta"), vec!["--package", "alpha", "--package", "beta"]),
        ] {
            let mut survey = Survey::from_test_root("never-read-preflight-root".into());
            survey.selected = vec!["alpha".into(), "beta".into()];
            survey.files.push(TargetFile {
                path: "lib.rs".into(),
                absolute: "never-read-preflight-root/lib.rs".into(),
                package: "alpha".into(),
                source: None,
            });
            survey.reach = [
                ("alpha".into(), once("alpha".into()).collect()),
                ("beta".into(), once("alpha".into()).collect()),
                ("gamma".into(), once("gamma".into()).collect()),
            ]
            .into_iter()
            .collect();
            let plan = survey.skeleton();
            let config = Config {
                test_workspace: whole_workspace,
                test_packages: explicit_package.into_iter().map(str::to_owned).collect(),
                ..Config::default()
            };
            let work = Workspace::adopt("never-read-preflight-root".into(), "never-written-preflight-target".into());
            let calls = Rc::new(RefCell::new(0));
            let result = super::super::build::with_compiler(
                {
                    let calls = Rc::clone(&calls);
                    move |_work, arguments, _timeout, _events| {
                        *calls.borrow_mut() += 1;
                        let mut expected = vec!["check", "--tests", "--keep-going", "--message-format=json"];
                        expected.extend_from_slice(&expected_selection);
                        assert_eq!(arguments, expected);
                        Err(error!("the in-process preflight observer stops before execution"))
                    }
                },
                || preflight(&survey, &plan, &work, &config, &mut Converger::default(), &mut Recorder::default()),
            );
            let failure = result.err().expect("the injected compiler returned an error");
            assert_eq!(failure.message(), "the in-process preflight observer stops before execution");
            assert_eq!(*calls.borrow(), 1);
        }
    }

    #[derive(Default)]
    struct MeasurementEvents {
        recorded: Recorder,
        measurements: usize,
        planning: Vec<(usize, usize)>,
        before_baseline: Option<Box<dyn FnOnce()>>,
    }

    impl Events for MeasurementEvents {
        fn phase(&mut self, verb: &str, detail: &str) {
            self.recorded.phase(verb, detail);
            if verb == "Baselining"
                && let Some(before) = self.before_baseline.take()
            {
                before();
            }
        }

        fn mutant(&mut self, mutant: &Mutant) {
            self.recorded.mutant(mutant);
        }

        fn measured(&mut self, _plan: &Plan, _session: &Session) {
            self.measurements += 1;
        }

        fn sweep_planning(&mut self, _plan: &Plan, binaries: usize, jobs: usize) {
            self.planning.push((binaries, jobs));
        }
    }

    /// Surveys a real tiny crate with isolated cache/output; compilation is scripted by callers,
    /// while workspace preparation, metadata, baseline execution and publication remain native.
    fn orchestration_fixture(source: &str) -> (tempfile::TempDir, Survey, Selection, Config) {
        let (directory, root) = fixtures::private_crate_dir("measure-orchestration-", source);
        let arguments = crate::commands::SelectArgs {
            dir: root.clone(),
            mutators: Some("literal.bool_flip".to_owned()),
            ..crate::commands::SelectArgs::default()
        };
        let selection = arguments.selection().unwrap();
        let survey = Survey::new(&arguments, None).unwrap();
        let cache_dir = root.join("target").join("isolated-campaign-cache");
        crate::exec::create_private_dir_all_for_test(&cache_dir);
        let config = Config {
            jobs: 3,
            test_lib: true,
            nextest: false,
            cache_dir: Some(cache_dir),
            memory: MemoryPolicy {
                control: MemoryControl::Off,
                ..MemoryPolicy::default()
            },
            ..Config::default()
        };
        (directory, survey, selection, config)
    }

    /// Uses real package identity but reports the supplied executable instead of compiling source.
    fn successful_compiler_output(work: &Workspace, executable: &Utf8Path, emit_binary: bool) -> std::process::Output {
        let metadata = std::process::Command::new("cargo")
            .args(["metadata", "--format-version=1", "--no-deps", "--manifest-path"])
            .arg(work.root.join("Cargo.toml"))
            .output()
            .unwrap();
        assert!(metadata.status.success(), "{}", String::from_utf8_lossy(&metadata.stderr));
        let metadata: Value = from_slice(&metadata.stdout).unwrap();
        let package = &metadata["packages"][0];
        let stdout = if emit_binary {
            json!({
                "reason": "compiler-artifact",
                "profile": {"test": true},
                "package_id": package["id"],
                "manifest_path": work.root.join("Cargo.toml"),
                "target": {"name": package["targets"][0]["name"], "kind": ["lib"], "test": true},
                "executable": executable,
                "filenames": [executable],
            })
            .to_string()
            .into_bytes()
        } else {
            Vec::new()
        };
        #[cfg(unix)]
        let status = { std::process::ExitStatus::from_raw(0) };
        #[cfg(windows)]
        let status = { std::process::ExitStatus::from_raw(0) };
        std::process::Output {
            status,
            stdout,
            stderr: Vec::new(),
        }
    }

    #[test]
    fn measurement_drives_library_build_baseline_scope_and_phase_contracts() {
        for baseline in [false, true] {
            let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
            let config = Config { baseline, ..config };
            let executable = crate::testing::helper().path;
            let mut events = MeasurementEvents::default();
            let calls = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&calls);
            let result = super::super::build::with_compiler(
                move |work, arguments, _timeout, _events| {
                    observed.borrow_mut().push(arguments.to_vec());
                    Ok(Some(successful_compiler_output(work, &executable, true)))
                },
                || measure(&survey, &selection, &config, &mut events),
            )
            .unwrap();
            assert_eq!(result.plan.mutants.len(), 1);
            assert!(result.learning.is_none());
            assert!(result.stuck.is_empty());
            assert!(result.dropped.is_empty());
            let built = result
                .built
                .expect("one viable mutation and its test artifact must retain a measured workspace");
            assert_eq!(built.session.binaries.len(), 1);
            assert!(built.work.runner().is_none());
            assert!(!built.session.metered);
            assert_eq!(built.oracle.packages, survey.selected);
            assert!(built.oracle.package_local);
            assert!(!built.oracle.whole_workspace);
            assert_eq!(built.session.stall.is_some(), baseline);
            let calls = calls.borrow();
            assert!(
                calls.len() >= 2,
                "preflight and instrumented schema are separate compiler invocations"
            );
            for test_build in calls
                .iter()
                .filter(|arguments| arguments.first().is_some_and(|argument| argument == "test"))
            {
                assert!(test_build.iter().any(|argument| argument == "--lib"), "{test_build:?}");
            }
            assert_eq!(events.planning, [(1, 3)]);
            assert!(
                events
                    .recorded
                    .phases
                    .iter()
                    .any(|(_, detail)| detail == "0 unviable mutants, leaving 1 viable mutant")
            );
            if baseline {
                assert!(
                    events
                        .recorded
                        .phases
                        .iter()
                        .any(|(_, detail)| detail.contains("1 test binary passed"))
                );
            } else {
                assert!(
                    events
                        .recorded
                        .phases
                        .iter()
                        .any(|(_, detail)| detail == "no baseline was measured")
                );
            }
        }
    }

    #[test]
    fn empty_populations_do_not_build_or_invent_learning_or_a_prepared_session() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> u8 { 7 }\n");
        let executable = crate::testing::helper().path;
        let mut events = Recorder::default();
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(successful_compiler_output(work, &executable, false))),
            || run(&survey, &selection, &config, &mut events),
        )
        .unwrap();
        assert!(result.plan.mutants.is_empty());
        assert!(result.built.is_none());
        assert!(result.learning.is_none());
        assert!(result.stuck.is_empty());
        assert!(!events.phases.iter().any(|(verb, _)| verb == "Excluding"));
        assert!(events.phases.iter().any(|(_, detail)| detail == ", 0 mutants"));
    }

    #[test]
    fn preflight_whole_workspace_requests_reach_the_actual_compiler_arguments() {
        let (_directory, survey, selection, mut config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        config.test_workspace = true;
        let executable = crate::testing::helper().path;
        let calls = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&calls);
        let measured = super::super::build::with_compiler(
            move |work, arguments, _timeout, _events| {
                observed.borrow_mut().push(arguments.to_vec());
                Ok(Some(successful_compiler_output(work, &executable, true)))
            },
            || measure(&survey, &selection, &config, &mut Recorder::default()),
        )
        .unwrap();
        let calls = calls.borrow();
        assert!(calls[0].iter().any(|argument| argument == "--workspace"), "{calls:?}");
        let built = measured.built.unwrap();
        assert!(built.oracle.whole_workspace);
        assert!(!built.oracle.package_local);
    }

    #[test]
    fn early_measurement_validation_returns_memory_oracle_and_native_cache_errors() {
        for refusal in ["memory", "oracle", "cache"] {
            let (_directory, survey, selection, mut config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
            match refusal {
                "memory" => {
                    config.baseline = false;
                    config.memory = MemoryPolicy {
                        control: MemoryControl::Enforce,
                        demand: Demand::Stated,
                        ..MemoryPolicy::default()
                    };
                }
                "oracle" => config.include_tests.push("missing-target".to_owned()),
                "cache" => {
                    let blocker = survey.root.join("cache-is-a-file");
                    fs::write(&blocker, "not a directory").unwrap();
                    config.cache_dir = Some(blocker);
                }
                _ => unreachable!(),
            }
            let mut failed = FailedMeasurement::default();
            let result = super::super::build::with_compiler(
                |_work, _arguments, _timeout, _events| panic!("early validation must not invoke a compiler"),
                || run_with_locks(&survey, &selection, &config, &mut Recorder::default(), None, &mut failed, None),
            );
            let error = result.expect_err("invalid preparation input must be returned without panic");
            match refusal {
                "memory" => assert!(error.to_string().contains("--memory"), "{error}"),
                "oracle" => assert!(error.to_string().contains("missing-target"), "{error}"),
                "cache" => assert!(error.to_string().contains("cache-is-a-file"), "{error}"),
                _ => unreachable!(),
            }
            assert!(failed.plan.is_none());
            assert!(failed.session.is_none());
            assert!(failed.work.is_none());
        }
    }

    #[test]
    fn schema_compiler_errors_are_returned_instead_of_unwrapped() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let executable = crate::testing::helper().path;
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| {
                if fs::read_to_string(work.root.join("src/lib.rs")).unwrap().contains("gamma_rt") {
                    return Err(error!("the controlled schema compiler refused its input"));
                }
                Ok(Some(successful_compiler_output(work, &executable, false)))
            },
            || measure(&survey, &selection, &config, &mut Recorder::default()),
        );
        let error = result.expect_err("a refused schema compiler must propagate its diagnostic");
        assert!(
            error.to_string().contains("the controlled schema compiler refused its input"),
            "{error}"
        );
    }

    #[test]
    fn nonlibrary_measurement_can_report_an_empty_oracle_without_a_library_usage_error() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let config = Config { test_lib: false, ..config };
        let executable = crate::testing::helper().path;
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(successful_compiler_output(work, &executable, false))),
            || measure(&survey, &selection, &config, &mut Recorder::default()),
        )
        .unwrap();
        let built = result.built.expect("a viable nonlibrary population still has a measured run");
        assert!(built.session.binaries.is_empty());
        assert!(result.learning.is_none());
    }

    #[test]
    fn library_measurement_rejects_absent_artifacts_without_losing_its_discovered_plan() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let executable = crate::testing::helper().path;
        let mut failed = FailedMeasurement::default();
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(successful_compiler_output(work, &executable, false))),
            || measure_with_locks(&survey, &selection, &config, &mut Recorder::default(), None, &mut failed),
        );
        let error = result.expect_err("a library oracle cannot proceed without a runnable library artifact");
        assert!(error.is_usage());
        assert!(
            error.to_string().contains("selected no runnable library unit-test harness"),
            "{error}"
        );
        let plan = failed
            .plan
            .expect("post-convergence preparation errors must retain their discovered population");
        assert_eq!(plan.mutants.len(), 1);
        assert!(failed.session.is_none());
    }

    #[test]
    fn failed_measured_baselines_retain_the_session_population_and_workspace() {
        let (_directory, survey, selection, mut config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        config.cargo.test_args = vec!["--gamma-step=when-arg:--list|exit:0".to_owned(), "--gamma-step=exit:1".to_owned()];
        let executable = crate::testing::helper().path;
        let mut failed = FailedMeasurement::default();
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(successful_compiler_output(work, &executable, true))),
            || measure_with_locks(&survey, &selection, &config, &mut Recorder::default(), None, &mut failed),
        );
        assert!(
            result.is_err(),
            "the native helper must fail its baseline after successful marker enumeration"
        );
        let session = failed
            .session
            .expect("a failed baseline must retain all its measured session diagnostics");
        assert_eq!(session.binaries.len(), 1);
        assert!(!session.metered);
        assert!(session.stall.is_none());
        assert_eq!(
            failed
                .plan
                .expect("a failed baseline retains its selected population")
                .mutants
                .len(),
            1
        );
        assert!(
            failed
                .work
                .expect("a failed baseline retains its workspace until publication")
                .root
                .is_dir()
        );
    }

    #[test]
    fn postbuild_filter_and_resource_errors_preserve_the_discovered_population() {
        for wrong_target in [false, true] {
            let (_directory, survey, selection, mut config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
            if wrong_target {
                config.include_tests = vec![survey.library_tests[0].target.clone()];
            }
            let executable = if wrong_target {
                crate::testing::helper().path
            } else {
                survey.root.join("missing-runnable-library")
            };
            let mut failed = FailedMeasurement::default();
            let result = super::super::build::with_compiler(
                move |work, _arguments, _timeout, _events| {
                    let mut output = successful_compiler_output(work, &executable, true);
                    if wrong_target {
                        let mut artifact: Value = from_slice(&output.stdout).unwrap();
                        artifact["target"]["name"] = "not-the-selected-library".into();
                        output.stdout = artifact.to_string().into_bytes();
                    }
                    Ok(Some(output))
                },
                || measure_with_locks(&survey, &selection, &config, &mut Recorder::default(), None, &mut failed),
            );
            let error = result.expect_err("an excluded or unlaunchable test artifact must not form a measured oracle");
            if wrong_target {
                assert!(error.to_string().contains("no test target"), "{error}");
            } else {
                assert!(error.to_string().contains("missing-runnable-library"), "{error}");
            }
            assert_eq!(
                failed
                    .plan
                    .expect("postbuild preparation errors must preserve the population")
                    .mutants
                    .len(),
                1
            );
            assert!(failed.session.is_none());
        }
    }

    fn prepared_record(survey: &Survey, config: &Config) -> super::super::testing_state::RecordPreparation {
        let base = campaign_base(&survey.root, &survey.target, config.cache_dir.as_deref());
        super::super::testing_state::RecordPreparation {
            report_info: None,
            name_filtered: false,
            inputs: crate::discover::RunRecord::snapshot_with_external(
                &survey.root,
                &base,
                survey.external_inputs(),
                survey.has_untracked_build_script_inputs(),
            ),
            base,
            context: crate::discover::ContextDigest::default(),
            killers: survey.killers(),
        }
    }

    #[test]
    fn a_published_testing_session_returns_a_native_journal_arming_failure_without_panicking() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let record = prepared_record(&survey, &config);
        let executable = crate::testing::helper().path;
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let triggered = std::rc::Rc::clone(&observed);
        let result = super::super::workspace::with_journal_arm_hook(
            move |base| {
                assert!(base.join("testing-session.json").is_file());
                let journal = base.join("testing-verdicts.jsonl");
                assert!(journal.is_file(), "the session must have published its initial journal first");
                fs::remove_file(&journal).unwrap();
                fs::create_dir(&journal).unwrap();
                triggered.set(true);
            },
            || {
                super::super::build::with_compiler(
                    move |work, _arguments, _timeout, _events| Ok(Some(retained_compiler_output(work, &executable))),
                    || {
                        run_with_locks(
                            &survey,
                            &selection,
                            &config,
                            &mut Recorder::default(),
                            None,
                            &mut FailedMeasurement::default(),
                            Some(&record),
                        )
                    },
                )
            },
        );
        assert!(
            observed.get(),
            "the failure must occur after state publication, not during initial preparation"
        );
        let failure = result.expect_err("a directory replacing the published journal cannot be armed");
        assert!(failure.message().contains("testing-verdicts.jsonl"), "{failure}");
        assert!(record.base.join("testing-session.json").is_file());
        assert!(record.base.join("testing-verdicts.jsonl").is_dir());
    }

    fn retained_compiler_output(work: &Workspace, executable: &Utf8Path) -> std::process::Output {
        fs::create_dir_all(&work.target).unwrap();
        let retained = work.target.join(executable.file_name().unwrap());
        fs::copy(executable, &retained).unwrap();
        successful_compiler_output(work, &retained, true)
    }

    /// Publishes testing state for resume, retaining its executable and arming the durable journal.
    /// Compilation is scripted; publication, native baseline and subsequent resume remain real.
    fn prepared_testing_fixture(
        survey: &Survey,
        selection: &Selection,
        config: &Config,
        killers: Killers,
    ) -> (super::super::testing_state::TestingState, Workspace) {
        let record = prepared_record(survey, config);
        let executable = crate::testing::helper().path;
        let measured = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(retained_compiler_output(work, &executable))),
            || measure(survey, selection, config, &mut Recorder::default()),
        )
        .unwrap();
        let mut built = measured.built.unwrap();
        let state = super::super::testing_state::TestingState::start(
            &measured.plan,
            &built,
            killers,
            super::super::testing_state::TestingSettings {
                jobs: config.jobs,
                mutators: Some(selection.sorted().join(",")),
                timeout_floor: config.timeout_floor,
                confirm: config.confirm,
                narrowed: true,
                incremental: config.incremental.is_enabled(),
            },
            &record,
            (&measured.stuck, &measured.dropped),
        )
        .unwrap();
        built.work.arm_testing_journal().unwrap();
        (state, built.work)
    }

    #[test]
    fn resumed_prepared_testing_returns_measurements_learning_and_a_new_sweep_cost() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let (state, work) = prepared_testing_fixture(&survey, &selection, &config, Killers::default());
        let mut events = MeasurementEvents::default();
        let resumed = resume_testing(state, work, &mut events).unwrap();
        assert_eq!(events.measurements, 1);
        assert_eq!(events.planning, [(1, 3)]);
        assert!(resumed.learning.is_some());
        assert!(resumed.plan.mutants.iter().all(|mutant| mutant.outcome != Outcome::Pending));
        assert!(
            resumed
                .built
                .expect("a resumed run must retain its measured workspace")
                .session
                .phases
                .sweep
                .is_some()
        );
    }

    #[test]
    fn run_with_a_record_publishes_the_prepared_state_before_sweeping() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let record = prepared_record(&survey, &config);
        let executable = crate::testing::helper().path;
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(retained_compiler_output(work, &executable))),
            || {
                run_with_locks(
                    &survey,
                    &selection,
                    &config,
                    &mut Recorder::default(),
                    None,
                    &mut FailedMeasurement::default(),
                    Some(&record),
                )
            },
        )
        .unwrap();
        let (saved, _journal_bytes) = super::super::testing_state::TestingState::load(&record.base).unwrap();
        assert_eq!(saved.settings.mutators.as_deref(), Some("literal.bool_flip"));
        assert_eq!(saved.plan.mutants.len(), result.plan.mutants.len());
        assert!(result.learning.is_some());
    }

    struct PublicationCollision {
        base: Utf8PathBuf,
        fired: bool,
    }

    impl Events for PublicationCollision {
        fn phase(&mut self, _verb: &str, _detail: &str) {}

        fn mutant(&mut self, _mutant: &Mutant) {}

        fn measured(&mut self, _plan: &Plan, _session: &Session) {
            assert!(!self.fired);
            self.fired = true;
            let phase = self.base.join("testing-phase.json");
            fs::create_dir(&phase).unwrap();
        }
    }

    #[test]
    fn completed_and_resumed_sweeps_propagate_native_phase_publication_errors() {
        for resume in [false, true] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let base = Utf8Path::from_path(directory.path()).unwrap().to_owned();
            let (mut state, survey, selection) = recovered_measured_plan();
            state.record.base.clone_from(&base);
            fs::write(base.join("testing-session.json"), to_vec(&state).unwrap()).unwrap();
            fs::write(base.join("testing-verdicts.jsonl"), "").unwrap();
            let mut work = Workspace::adopt(survey.root.clone(), base.join("target"));
            work.arm_testing_journal().unwrap();
            let mut events = PublicationCollision {
                base: base.clone(),
                fired: false,
            };
            let result = if resume {
                resume_testing(state, work, &mut events)
            } else {
                let measured = Measured {
                    plan: state.plan,
                    built: Some(Built {
                        work,
                        session: state.session,
                        oracle: state.oracle,
                    }),
                    stuck: Vec::new(),
                    dropped: Vec::new(),
                    learning: None,
                };
                let config = Config {
                    incremental: super::super::incremental_mode::IncrementalMode::No,
                    ..Config::default()
                };
                with_prepared_measurement(measured, || {
                    run_with_locks(
                        &survey,
                        &selection,
                        &config,
                        &mut events,
                        None,
                        &mut FailedMeasurement::default(),
                        None,
                    )
                })
            };
            let error = result.expect_err("a directory at the phase filename must refuse completed-state publication");
            assert!(events.fired, "resume={resume}: failed before publication: {error}");
            assert!(error.to_string().contains("testing-phase.json"), "{error}");
            assert!(base.join("testing-phase.json").is_dir());
        }
    }

    fn startup_failure_arguments() -> Vec<String> {
        let marker = core::str::from_utf8(gamma_rt::ENVIRONMENT_ERROR_MARKER).unwrap();
        vec![
            "--gamma-step=when-arg:--list|print:covered: test".to_owned(),
            "--gamma-step=when-arg:--list|exit:0".to_owned(),
            format!("--gamma-step=when-env:{}|print:{marker}", gamma_rt::ACTIVE_VAR),
            format!("--gamma-step=when-env:{}|exit:1", gamma_rt::ACTIVE_VAR),
            "--gamma-step=print:running 1 test".to_owned(),
            "--gamma-step=print:test covered ... ok".to_owned(),
            "--gamma-step=exit:0".to_owned(),
        ]
    }

    #[test]
    fn failed_full_and_resumed_sweeps_publish_partial_learning_and_retain_the_failed_population() {
        for resume in [false, true] {
            let (_directory, survey, selection, mut config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
            config.cargo.test_args = startup_failure_arguments();
            let scanned = survey.scan(None, &selection, &mut 0).unwrap();
            let mutant = scanned.mutants[0].clone();
            let killer = Killer {
                package: mutant.package.to_string(),
                target: mutant.package.replace('-', "_"),
                test: "covered".to_owned(),
            };
            let base = campaign_base(&survey.root, &survey.target, config.cache_dir.as_deref());
            let mut failed = FailedMeasurement::default();
            let result = if resume {
                let mut killers = Killers::default();
                killers.record(mutant.id.clone(), killer.clone());
                let (state, work) = prepared_testing_fixture(&survey, &selection, &config, killers);
                resume_testing(state, work, &mut Recorder::default())
            } else {
                let executable = crate::testing::helper().path;
                let probes = core::iter::once((mutant.id.clone(), killer.clone())).collect();
                super::super::build::with_compiler(
                    move |work, _arguments, _timeout, _events| {
                        let output = successful_compiler_output(work, &executable, true);
                        crate::discover::RunRecord::store_probes(work.base(), &probes);
                        Ok(Some(output))
                    },
                    || run_with_locks(&survey, &selection, &config, &mut Recorder::default(), None, &mut failed, None),
                )
            };
            let error = result.expect_err("a runtime startup marker must abort the sweep after its valid baseline");
            assert!(error.to_string().contains("startup"), "{error}");
            assert_eq!(
                crate::discover::RunRecord::load_incomplete(&base).probes().get(&mutant.id),
                Some(&killer)
            );
            if !resume {
                let retained = failed
                    .plan
                    .expect("a failed sweep must retain its population for command-level diagnostics");
                assert_eq!(retained.mutants.len(), 1);
                assert_eq!(retained.mutants[0].id, mutant.id);
            }
        }
    }

    #[test]
    fn measurement_installs_discovered_resources_before_returning_its_workspace() {
        let (directory, survey, selection, mut config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        config.resources = vec![super::super::resources::ResourceLimit::new("db", 1).unwrap()];
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("resource_helper.rs");
        let executable = root.join(format!("resource_helper{}", std::env::consts::EXE_SUFFIX));
        fs::write(
            &source,
            "#[test] #[ignore] fn __cargo_gamma_resource_6462_binary() {}\n#[test] fn covered() {}\n",
        )
        .unwrap();
        let compiler = std::process::Command::new("rustc")
            .args(["--test", "--edition=2024"])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(compiler.status.success(), "{}", String::from_utf8_lossy(&compiler.stderr));
        let measured = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| Ok(Some(successful_compiler_output(work, &executable, true))),
            || measure(&survey, &selection, &config, &mut Recorder::default()),
        )
        .unwrap();
        let built = measured.built.unwrap();
        assert!(!built.work.required_resources(&built.session.binaries[0], Only::All).is_empty());
    }

    #[test]
    fn postbuild_environment_arming_errors_keep_the_converged_population() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let executable = crate::testing::helper().path;
        let manifest = Rc::new(RefCell::new(None));
        let observed = Rc::clone(&manifest);
        let mut events = MeasurementEvents {
            before_baseline: Some(Box::new(move || {
                fs::write(
                    manifest
                        .borrow()
                        .as_ref()
                        .expect("a successful compiler must supply its copied manifest"),
                    "not = [valid TOML",
                )
                .unwrap();
            })),
            ..MeasurementEvents::default()
        };
        let mut failed = FailedMeasurement::default();
        let result = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| {
                *observed.borrow_mut() = Some(work.root.join("Cargo.toml"));
                Ok(Some(successful_compiler_output(work, &executable, true)))
            },
            || measure_with_locks(&survey, &selection, &config, &mut events, None, &mut failed),
        );
        let error = result.expect_err("the finalized environment must not be armed from a malformed manifest");
        assert!(error.to_string().contains("metadata"), "{error}");
        assert_eq!(failed.plan.unwrap().mutants.len(), 1);
        assert!(failed.session.is_none());
    }

    #[test]
    fn an_unattributed_schema_refusal_reports_stuck_mutants_without_claiming_learning() {
        let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
        let executable = crate::testing::helper().path;
        let mut events = Recorder::default();
        let measured = super::super::build::with_compiler(
            move |work, _arguments, _timeout, _events| {
                let mut output = successful_compiler_output(work, &executable, false);
                if fs::read_to_string(work.root.join("src/lib.rs")).unwrap().contains("gamma_rt") {
                    output.status = std::process::Command::new(&executable).arg("--gamma-step=exit:1").status().unwrap();
                    output.stderr = b"controlled unattributed compiler refusal".to_vec();
                }
                Ok(Some(output))
            },
            || measure(&survey, &selection, &config, &mut events),
        )
        .unwrap();
        assert!(measured.built.is_none());
        assert!(measured.learning.is_none());
        assert_eq!(measured.stuck.len(), 1);
        assert!(
            events
                .phases
                .iter()
                .any(|(_, detail)| detail == "the build could not be made to compile, 1 mutant not run")
        );
    }

    #[test]
    fn population_scan_and_runtime_link_errors_propagate_without_panicking() {
        for scan in [false, true] {
            let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
            let original = survey.root.join("src/lib.rs");
            let executable = crate::testing::helper().path;
            if !scan {
                fs::write(survey.root.join("Cargo.toml"), "not = [valid TOML").unwrap();
            }
            let result = super::super::build::with_compiler(
                move |work, _arguments, _timeout, _events| {
                    let output = successful_compiler_output(work, &executable, false);
                    if scan {
                        fs::write(&original, "pub fn subject( {").unwrap();
                    }
                    Ok(Some(output))
                },
                || measure(&survey, &selection, &config, &mut Recorder::default()),
            );
            let error = result.expect_err("invalid source or a malformed copied manifest must be returned as an error");
            let error = error.to_string();
            assert!(error.contains(if scan { "lib.rs" } else { "Cargo.toml" }), "{error}");
        }
    }

    #[test]
    fn measurement_propagates_preflight_errors_and_synchronized_source_drift() {
        for corrupt in [false, true] {
            let (_directory, survey, selection, config) = orchestration_fixture("pub fn subject() -> bool { true }\n");
            let executable = crate::testing::helper().path;
            let mut failed = FailedMeasurement::default();
            let result = super::super::build::with_compiler(
                move |work, _arguments, _timeout, _events| {
                    if !corrupt {
                        return Err(error!("the controlled preflight compiler refused its input"));
                    }
                    let output = successful_compiler_output(work, &executable, true);
                    fs::write(work.root.join("src/lib.rs"), "pub fn subject() -> bool { false }\n").unwrap();
                    Ok(Some(output))
                },
                || run_with_locks(&survey, &selection, &config, &mut Recorder::default(), None, &mut failed, None),
            );
            let error = result.expect_err("a failed preflight or changed synchronized source must be returned");
            let error = error.to_string();
            if corrupt {
                assert!(error.contains("changed between discovery and workspace synchronization"), "{error}");
            } else {
                assert!(error.contains("the controlled preflight compiler refused its input"), "{error}");
            }
        }
    }

    #[test]
    fn oracle_scope_and_library_admission_truth_tables_preserve_package_ownership() {
        let tests = [
            LibraryTestTarget {
                package: "alpha".to_owned(),
                target: "zeta".to_owned(),
            },
            LibraryTestTarget {
                package: "alpha".to_owned(),
                target: "alpha".to_owned(),
            },
            LibraryTestTarget {
                package: "beta".to_owned(),
                target: "beta".to_owned(),
            },
            LibraryTestTarget {
                package: "beta".to_owned(),
                target: "alpha".to_owned(),
            },
        ];
        for package_local in [false, true] {
            for whole_workspace in [false, true] {
                let oracle = Oracle::new(vec!["selected".to_owned()], package_local, whole_workspace);
                let scope = oracle.scope();
                assert_eq!(scope.packages, ["selected"]);
                assert_eq!(scope.package_local, package_local);
                assert_eq!(scope.whole_workspace, whole_workspace);
            }
        }
        for workspace in [false, true] {
            for requested in [Vec::new(), vec!["beta".to_owned()]] {
                let config = Config {
                    test_workspace: workspace,
                    test_packages: requested.clone(),
                    ..Config::default()
                };
                let targets = admitted_library_tests(&tests, &["alpha".to_owned()], &config);
                assert_eq!(
                    targets,
                    if workspace {
                        vec!["alpha".to_owned(), "beta".to_owned(), "zeta".to_owned()]
                    } else if requested.is_empty() {
                        vec!["alpha".to_owned(), "zeta".to_owned()]
                    } else {
                        vec!["alpha".to_owned(), "beta".to_owned()]
                    }
                );
            }
        }
    }

    #[test]
    fn oracle_validation_matrix_distinguishes_library_targets_patterns_and_empty_oracles() {
        let tests = vec!["integration".to_owned()];
        let libraries = [LibraryTestTarget {
            package: "selected".to_owned(),
            target: "library".to_owned(),
        }];
        let selected = ["selected".to_owned()];
        for test_lib in [false, true] {
            let valid = if test_lib { "library" } else { "integration" };
            let invalid = if test_lib { "integration" } else { "library" };
            let config = Config {
                test_lib,
                include_tests: vec![valid.to_owned()],
                ..Config::default()
            };
            validate_test_oracle(&tests, &libraries, &selected, &config).unwrap();
            let config = Config {
                test_lib,
                include_tests: vec![invalid.to_owned()],
                ..Config::default()
            };
            let failure =
                validate_test_oracle(&tests, &libraries, &selected, &config).expect_err("pattern names a target outside this oracle");
            assert_eq!(
                failure.to_string(),
                format!("no test target matches `{invalid}`; patterns match cargo target names, not test function names")
            );
            assert!(failure.is_usage());
        }
        validate_test_oracle(&[], &[], &selected, &Config::default()).unwrap();
        let config = Config {
            test_lib: true,
            ..Config::default()
        };
        let failure = validate_test_oracle(&tests, &[], &selected, &config).expect_err("library oracle has no targets");
        assert_eq!(
            failure.to_string(),
            "`--test-lib` selected no runnable library unit-test harness; ensure an oracle package has a library target with `test = true`"
        );
        assert!(failure.is_usage());
        validate_test_oracle(&tests, &libraries, &selected, &config).unwrap();
    }

    #[test]
    fn preflight_retreat_uses_checked_packages_only_for_unrestricted_requests() {
        let checked = vec!["checked".to_owned(), "helper".to_owned()];
        for whole_workspace in [false, true] {
            let cleared = finish_preflight(Vec::new(), checked.clone(), whole_workspace, vec!["broken".to_owned()])
                .expect("unrestricted retreat names checked packages");
            assert_eq!(cleared.packages, checked);
            assert_eq!(cleared.whole_workspace, whole_workspace);
            assert_eq!(cleared.dropped, ["broken"]);
            let cleared = finish_preflight(
                vec!["requested".to_owned(), "broken".to_owned()],
                checked.clone(),
                whole_workspace,
                vec!["broken".to_owned()],
            )
            .expect("restricted retreat subtracts only");
            assert_eq!(cleared.packages, ["requested"]);
        }
        let failure = narrowed_oracle(vec!["broken".to_owned()], checked, &["broken".to_owned(), "other".to_owned()])
            .expect_err("no requested oracle remains");
        assert_eq!(
            failure.to_string(),
            "none of the packages this run's tests would come from could be compiled: `broken`, `other`.\nFix the build there, or name a package that does build with --test-package."
        );
    }

    #[test]
    fn schema_progress_reports_zero_singular_and_plural_unbuilt_populations_exactly() {
        for (not_built, expected) in [
            (0, "2 unviable mutants, leaving 7 viable mutants"),
            (1, "2 unviable mutants; 1 mutant could not be built; 7 are viable"),
            (2, "2 unviable mutants; 2 mutants could not be built; 7 are viable"),
        ] {
            let mut events = Recorder::default();
            begin_schema_build(&mut events, 0);
            finish_schema_build(&mut events, 2, 7, not_built);
            assert_eq!(
                events.phases,
                [
                    ("Excluding".to_owned(), "unviable mutants".to_owned()),
                    (String::new(), expected.to_owned()),
                ]
            );
        }
    }

    #[test]
    fn schema_population_counts_require_live_ordinals_and_exact_outcome_classes() {
        let fixture = oracle_plan("core").mutants.remove(0);
        let mut population = Vec::new();
        for outcome in Outcome::ALL {
            for ordinal in 0..=2 {
                let mutant = Mutant {
                    ordinal,
                    outcome,
                    ..fixture.clone()
                };
                let expected = if outcome == Outcome::CompileError {
                    (1, 0, 0)
                } else if ordinal == 0 {
                    (0, 0, 0)
                } else {
                    match outcome {
                        Outcome::CompileError => (1, 0, 0),
                        Outcome::Pending => (0, 1, 0),
                        Outcome::NotBuilt => (0, 0, 1),
                        _ => (0, 0, 0),
                    }
                };
                assert_eq!(
                    schema_build_counts(core::slice::from_ref(&mutant)),
                    expected,
                    "{outcome:?}/{ordinal}"
                );
                population.push(mutant);
            }
        }
        assert_eq!(schema_build_counts(&population), (3, 2, 2));
        assert_eq!(schema_build_counts(&[]), (0, 0, 0));
    }

    #[test]
    fn session_construction_carries_stall_and_metering_without_rederiving_measurements() {
        let (_dir, work) = crate::testing::helper_workspace("measure-session-contract-", &["exit:0"]);
        for metered in [false, true] {
            for stall in [None, Some(Duration::from_secs(23))] {
                let baseline = Baseline {
                    elapsed: Duration::from_secs(2),
                    wall: Duration::from_secs(3),
                    quiet: Duration::from_secs(4),
                    tests: Some(5),
                    peak: Some(6),
                };
                let build = Build {
                    history: Vec::new(),
                    binaries: vec![crate::testing::test_binary("subject")],
                    artifacts: String::new(),
                    withdrawn: 7,
                    rounds: 8,
                    widened: true,
                    census: Vec::new(),
                    stuck: None,
                    ordering: super::super::build::OrderingHints::default(),
                };
                let context = SessionContext {
                    build: Duration::from_secs(9),
                    metered,
                    unbounded: Some("fixture policy".to_owned()),
                    filtered: 10,
                    copy: Duration::from_secs(11),
                    preflight: Duration::from_secs(12),
                };
                let session = session_before_sweep(baseline, build, &work, context, stall);
                assert_eq!(session.stall, stall);
                assert_eq!(session.metered, metered);
                assert_eq!(session.baseline, Duration::from_secs(2));
                assert_eq!(session.baseline_wall, Duration::from_secs(3));
                assert_eq!(session.quiet, Duration::from_secs(4));
                assert_eq!(session.tests, Some(5));
                assert_eq!(session.peak, Some(6));
                assert_eq!(session.withdrawn, 7);
                assert_eq!(session.rounds, 8);
                assert!(session.widened);
                assert_eq!(session.build, Duration::from_secs(9));
                assert_eq!(session.filtered, 10);
                assert_eq!(session.unbounded.as_deref(), Some("fixture policy"));
                assert_eq!(session.scratch.as_path(), work.base());
                assert_eq!(session.binaries.len(), 1);
                assert_eq!(session.binaries[0].path.as_str(), "subject");
                assert_eq!(session.phases.copy, Duration::from_secs(11));
                assert_eq!(session.phases.preflight, Duration::from_secs(12));
                assert!(session.phases.sweep.is_none());
            }
        }
    }

    #[test]
    fn synchronized_source_read_failures_remain_errors_instead_of_panics() {
        let (_dir, work) = crate::testing::helper_workspace("measure-source-errors-", &["exit:0"]);
        for path in ["missing.rs", "directory"] {
            if path == "directory" {
                fs::create_dir(work.root.join(path)).expect("directory read failure fixture");
            }
            let digests = std::iter::once((Utf8PathBuf::from(path), "unused".to_owned())).collect();
            let failure = validate_synchronized_sources(&work, &digests).expect_err("unreadable synchronized source");
            assert!(
                failure
                    .to_string()
                    .contains(&format!("could not validate synchronized source `{path}`"))
            );
        }
    }

    #[test]
    fn library_oracle_validation_keeps_package_and_target_associated() {
        let tests = [
            LibraryTestTarget {
                package: "alpha".to_owned(),
                target: "alpha-lib".to_owned(),
            },
            LibraryTestTarget {
                package: "beta".to_owned(),
                target: "beta-lib".to_owned(),
            },
        ];
        let config = Config {
            test_lib: true,
            ..Config::default()
        };

        assert_eq!(
            admitted_library_tests(&tests, &["alpha".to_owned()], &config),
            ["alpha-lib"],
            "a target owned only by beta must not validate alpha's oracle"
        );
    }

    #[test]
    fn synchronized_workspace_must_match_the_discovered_source_generation() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 workspace");
        fs::create_dir_all(root.join("src").as_std_path()).expect("source directory");
        fs::write(root.join("src/lib.rs").as_std_path(), "pub fn changed() {}\n").expect("changed source");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = stuck_plan(Vec::new());
        plan.root.clone_from(&root);
        plan.files.push(TargetFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            absolute: root.join("src/lib.rs"),
            package: "subject".to_owned(),
            source: Some("pub fn original() {}\n".to_owned()),
        });
        let _previous = plan
            .digests
            .insert(Utf8PathBuf::from("src/lib.rs"), crate::discover::digest(b"pub fn original() {}\n"));

        let error = validate_synchronized_sources(&work, &plan.digests).expect_err("changed copied source must stop the run");

        assert!(
            error
                .to_string()
                .contains("source `src/lib.rs` changed between discovery and workspace synchronization"),
            "{error}"
        );
    }

    #[test]
    fn synchronized_workspace_generation_ignores_the_parsed_byte_order_mark() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 workspace");
        fs::create_dir_all(root.join("src").as_std_path()).expect("source directory");
        fs::write(root.join("src/lib.rs").as_std_path(), "\u{feff}pub fn original() {}\n").expect("source with BOM");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = stuck_plan(Vec::new());
        plan.root.clone_from(&root);
        plan.files.push(TargetFile {
            path: Utf8PathBuf::from("src/lib.rs"),
            absolute: root.join("src/lib.rs"),
            package: "subject".to_owned(),
            source: Some("pub fn original() {}\n".to_owned()),
        });
        let _previous = plan
            .digests
            .insert(Utf8PathBuf::from("src/lib.rs"), crate::discover::digest(b"pub fn original() {}\n"));

        validate_synchronized_sources(&work, &plan.digests).expect("BOM-normalized source must match discovery");
    }

    #[test]
    fn incomplete_non_incremental_campaign_preserves_existing_learning() {
        let directory = tempfile::tempdir().expect("temporary campaign state");
        let base = Utf8Path::from_path(directory.path()).expect("UTF-8 campaign state");
        let mutant = fixtures::mutant();
        let killer = Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: "tests::caught".to_owned(),
        };
        crate::discover::RunRecord::store_probes(base, &core::iter::once((mutant.id.clone(), killer.clone())).collect());
        let mut pending = Killers::default();
        pending.record(
            mutant.id.clone(),
            Killer {
                test: "tests::newly_caught".to_owned(),
                ..killer.clone()
            },
        );

        store_incomplete_learning(false, &pending, base, std::slice::from_ref(&mutant));

        assert_eq!(crate::discover::RunRecord::load(base).probes().get(&mutant.id), Some(&killer));
        assert!(!base.join("incomplete-gamma-learning.json").exists());
    }

    #[test]
    fn incomplete_incremental_campaign_keeps_learning_out_of_the_completed_record() {
        let directory = tempfile::tempdir().expect("temporary campaign state");
        let base = Utf8Path::from_path(directory.path()).expect("UTF-8 campaign state");
        let mutant = fixtures::mutant();
        let killer = Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: "tests::caught".to_owned(),
        };
        let mut killers = Killers::default();
        killers.record(mutant.id.clone(), killer.clone());

        store_incomplete_learning(true, &killers, base, std::slice::from_ref(&mutant));

        assert!(crate::discover::RunRecord::load(base).probes().is_empty());
        assert_eq!(
            crate::discover::RunRecord::load_incomplete(base).probes().get(&mutant.id),
            Some(&killer)
        );
    }

    #[test]
    fn preflight_policy_reports_its_phases_and_preserves_scope() {
        let mut events = Recorder::default();
        begin_preflight(&mut events);
        end_preflight(&mut events);
        begin_baseline(&mut events);
        assert_eq!(
            events.phases,
            vec![
                ("Validating".to_owned(), "the workspace".to_owned()),
                (String::new(), String::new()),
                ("Baselining".to_owned(), "running the unmutated suite".to_owned()),
            ]
        );

        assert!(inferred_package_local(&Config::default()));
        assert!(!inferred_package_local(&Config {
            test_workspace: true,
            ..Config::default()
        }));
        assert!(!inferred_package_local(&Config {
            test_packages: vec!["tests".to_owned()],
            ..Config::default()
        }));
        let ordinary = finish_preflight(vec!["a".to_owned()], vec!["a".to_owned()], false, Vec::new())
            .expect("an ordinary preflight keeps its request");
        assert_eq!(ordinary.packages, ["a"]);
        assert!(!ordinary.whole_workspace);
        assert!(ordinary.dropped.is_empty());
        let retreated = finish_preflight(
            vec!["a".to_owned(), "b".to_owned()],
            vec!["a".to_owned(), "b".to_owned()],
            true,
            vec!["a".to_owned()],
        )
        .expect("a partial retreat keeps the compiling package");
        assert_eq!(retreated.packages, ["b"]);
        assert!(retreated.whole_workspace);
        assert_eq!(retreated.dropped, ["a"]);

        let mut none = Vec::new();
        assert_eq!(restrict_binaries(&mut none, &Config::default()).expect("empty stays empty"), 0);
        let mut binaries = [oracle_binary("core", Some(1)), oracle_binary("core", Some(1))].to_vec();
        binaries[0].target = "keep".to_owned();
        binaries[1].target = "drop".to_owned();
        assert_eq!(
            restrict_binaries(
                &mut binaries,
                &Config {
                    exclude_tests: vec!["drop".to_owned()],
                    ..Config::default()
                },
            )
            .expect("one target remains"),
            1
        );
        assert_eq!(binaries.len(), 1);
        assert_eq!(binaries[0].target, "keep");
    }

    /// A baseline with the given elapsed time and quiet period, and nothing else measured.
    ///
    /// The two fields the calibration reads, so a test can state exactly what it is calibrating
    /// from without standing up a run.
    const fn measured(elapsed: Duration, quiet: Duration) -> Baseline {
        Baseline {
            elapsed,
            wall: Duration::ZERO,
            quiet,
            tests: None,
            peak: None,
        }
    }

    #[test]
    fn the_default_stall_budget_is_ten_times_the_measured_quiet_period() {
        let config = Config::default();
        let baseline = measured(Duration::from_mins(10), Duration::from_secs(3));

        let stall = calibrate_stall(&baseline, &config);

        assert_eq!(
            stall.budget,
            Some(Duration::from_secs(30)),
            "3s quiet x {} factor should be 30s; the default factor is now {}",
            config.stall_factor,
            config.stall_factor
        );
    }

    #[test]
    fn a_suite_that_never_goes_quiet_gets_the_stall_floor_rather_than_zero() {
        let config = Config::default();
        let baseline = measured(Duration::from_mins(10), Duration::ZERO);

        let stall = calibrate_stall(&baseline, &config);

        assert_eq!(
            stall.budget,
            Some(Duration::from_secs(5)),
            "a zero quiet period scaled by any factor is still zero, so the floor must win; \
             the default floor is now {:?}",
            config.stall_floor
        );
    }

    #[test]
    fn stall_detection_stands_down_when_there_is_no_baseline_to_calibrate_from() {
        let config = Config {
            baseline: false,
            ..Config::default()
        };

        let stall = calibrate_stall(&measured(Duration::from_secs(100), Duration::from_secs(1)), &config);

        assert_eq!(stall.budget, None, "a budget derived from a baseline that never ran is a guess");
    }

    #[test]
    fn stall_detection_stands_down_when_it_is_switched_off() {
        let config = Config {
            stall: false,
            ..Config::default()
        };

        let stall = calibrate_stall(&measured(Duration::from_secs(100), Duration::from_secs(1)), &config);

        assert_eq!(stall.budget, None, "--no-stall must disable the detector outright");
    }

    /// One live mutant of the given mutator, in the given enclosing item.
    fn stuck_mutant(ordinal: u32, mutator: &str, item_path: &str) -> Mutant {
        Mutant {
            id: format!("m{ordinal}").into(),
            ordinal,
            mutator: (mutator.to_owned()).into(),
            item_path: (item_path.to_owned()).into(),
            original: "a < b".to_owned().into(),
            replacement: "a <= b".to_owned().into(),
            outcome: Outcome::NotBuilt,
            ..fixtures::mutant()
        }
    }

    /// A plan holding the given mutants and nothing else.
    fn stuck_plan(mutants: Vec<Mutant>) -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: Utf8PathBuf::from("/workspace"),
            files: Vec::new(),
            mutants,
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    #[test]
    fn stuck_build_diagnostic_pins_group_order_and_omits_empty_group_lines() {
        let plan = stuck_plan(vec![stuck_mutant(1, "operator", "scope")]);
        let abandoned = Abandoned {
            reason: "compiler detail".into(),
            ordinals: vec![1],
        };
        assert_eq!(
            describe_stuck(&plan, &["subject".into()], &abandoned),
            "the build for 1 package subject could not be made to compile, so 1 mutant never ran and is reported as `notbuilt` rather than as survivors.\nNot run, by mutator: operator (1).\nNot run, by scope: scope (1).\ncompiler detail"
        );
        assert_eq!(
            describe_stuck(
                &stuck_plan(Vec::new()),
                &[],
                &Abandoned {
                    reason: "compiler detail".into(),
                    ordinals: Vec::new()
                }
            ),
            "the build for the workspace could not be made to compile, so 0 mutants never ran and are reported as `notbuilt` rather than as survivors.\ncompiler detail"
        );
    }

    /// A build that could not be made to compile is described by where it got stuck, not merely by
    /// the fact that it did.
    ///
    /// "We could not build 900 mutants" is not something anyone can act on. "All of them were the
    /// same mutator, all in the same module" names one operator to exclude or one module to look
    /// at, and either turns a dead run into one that finishes.
    #[test]
    fn a_stuck_build_is_described_by_mutator_and_by_scope() {
        let plan = stuck_plan(vec![
            stuck_mutant(1, "relational.lt_to_le", "subject::less"),
            stuck_mutant(2, "relational.lt_to_le", "subject::less"),
            stuck_mutant(3, "arith.add_to_sub", "subject::sum"),
            stuck_mutant(4, "relational.lt_to_le", "subject::sum"),
        ]);
        let abandoned = Abandoned {
            reason: "Mutants blamed in the last rounds of this build: 9, 9, 9.".to_owned(),
            ordinals: vec![1, 2, 3, 4],
        };

        let described = describe_stuck(&plan, &["subject".to_owned()], &abandoned);

        assert!(described.contains("4 mutants"), "{described}");
        assert!(described.contains("subject"), "{described}");
        assert!(
            described.contains("Not run, by mutator: relational.lt_to_le (3), arith.add_to_sub (1)."),
            "{described}"
        );
        assert!(
            described.contains("Not run, by scope: subject::less (2), subject::sum (2)."),
            "{described}"
        );

        // The diagnostic that would have been the run's error message, kept word for word: the
        // per-round series is the only thing that says whether raising the limit would help.
        assert!(
            described.contains("Mutants blamed in the last rounds of this build: 9, 9, 9."),
            "{described}"
        );
    }

    /// A whole-workspace build names the workspace rather than an empty package list.
    #[test]
    fn a_stuck_whole_workspace_build_says_so() {
        let plan = stuck_plan(vec![stuck_mutant(1, "relational.lt_to_le", "subject::less")]);
        let abandoned = Abandoned {
            reason: "the instrumented tree does not compile".to_owned(),
            ordinals: vec![1],
        };

        let described = describe_stuck(&plan, &[], &abandoned);

        assert!(described.contains("the build for the workspace"), "{described}");
        assert!(described.contains("1 mutant never ran"), "{described}");
        assert!(
            described.ends_with("\nthe instrumented tree does not compile"),
            "the diagnostic is separated by exactly one newline: {described:?}"
        );
    }

    /// Only the mutants the build actually gave up on are counted, not every mutant in the plan.
    #[test]
    fn a_stuck_build_counts_only_what_it_gave_up_on() {
        let plan = stuck_plan(vec![
            stuck_mutant(1, "relational.lt_to_le", "subject::less"),
            stuck_mutant(2, "arith.add_to_sub", "subject::sum"),
        ]);
        let abandoned = Abandoned {
            reason: "stuck".to_owned(),
            ordinals: vec![2],
        };

        let described = describe_stuck(&plan, &["subject".to_owned()], &abandoned);

        assert!(described.contains("Not run, by mutator: arith.add_to_sub (1)."), "{described}");
        assert!(!described.contains("relational.lt_to_le"), "{described}");
    }

    #[test]
    fn population_and_build_progress_name_the_complete_work() {
        let mut mutants = [stuck_mutant(0, "a", "f"), stuck_mutant(1, "b", "f"), stuck_mutant(2, "c", "f")];
        mutants[0].outcome = Outcome::CompileError;
        assert_eq!(live_mutants(&mutants), 2);

        let mut events = Recorder::default();
        begin_population(&mut events, 12);
        finish_population(&mut events, &mutants);
        begin_schema_build(&mut events, 0);
        finish_schema_build(&mut events, 1, 2, 0);
        assert_eq!(
            events.phases,
            [
                ("Mutating".to_owned(), "12 target packages".to_owned()),
                (String::new(), ", 3 mutants".to_owned()),
                ("Excluding".to_owned(), "unviable mutants".to_owned()),
                (String::new(), "1 unviable mutant, leaving 2 viable mutants".to_owned()),
            ]
        );
    }

    #[test]
    fn population_progress_counts_live_and_unviable_mutants_once_but_not_other_inactive_results() {
        let mut mutants = [
            stuck_mutant(0, "historical", "f"),
            stuck_mutant(1, "live", "f"),
            stuck_mutant(2, "new_failure", "f"),
            stuck_mutant(0, "ignored", "f"),
            stuck_mutant(0, "settled", "f"),
        ];
        mutants[0].outcome = Outcome::CompileError;
        mutants[1].outcome = Outcome::Pending;
        mutants[2].outcome = Outcome::CompileError;
        mutants[3].outcome = Outcome::Ignored;
        mutants[4].outcome = Outcome::Killed;
        let mut events = Recorder::default();

        finish_population(&mut events, &mutants);
        let (unviable, viable, not_built) = schema_build_counts(&mutants);
        finish_schema_build(&mut events, unviable, viable, not_built);

        assert_eq!(
            events.phases,
            [
                (String::new(), ", 3 mutants".to_owned()),
                (String::new(), "2 unviable mutants, leaving 1 viable mutant".to_owned()),
            ]
        );
    }

    #[test]
    fn phase_opening_labels_preserve_the_completed_verbs() {
        #[derive(Default)]
        struct Labels(Vec<(String, String, String)>);

        impl Events for Labels {
            fn phase(&mut self, _verb: &str, _detail: &str) {
                panic!("phase openings must pass both display verbs through begin");
            }

            fn begin(&mut self, active: &str, completed: &str, detail: &str) {
                self.0.push((active.to_owned(), completed.to_owned(), detail.to_owned()));
            }

            fn mutant(&mut self, _mutant: &Mutant) {}
        }

        let mut events = Labels::default();
        begin_schema_build(&mut events, 0);
        begin_preflight(&mut events);
        begin_population(&mut events, 2);
        assert_eq!(
            events.0,
            [
                ("Excluding".to_owned(), "Excluded".to_owned(), "unviable mutants".to_owned()),
                ("Validating".to_owned(), "Validated".to_owned(), "the workspace".to_owned()),
                ("Mutating".to_owned(), "Mutated".to_owned(), "2 target packages".to_owned()),
            ]
        );
    }

    #[test]
    fn failed_baselines_return_errors_without_discarding_completed_measurements() {
        let (_directory, work) = crate::testing::helper_workspace(
            "retained-failed-baseline-",
            &[
                "print:running 2 tests",
                "print:test alpha ... FAILED",
                "print:test beta ... ok",
                "exit:1",
            ],
        );
        let mut binaries = [crate::testing::helper()];
        let mut retained = None;
        let result = take_baseline_retaining(
            &work,
            &mut binaries,
            &Config {
                jobs: 1,
                ..Config::default()
            },
            &MemoryPolicy {
                control: MemoryControl::Off,
                ..MemoryPolicy::default()
            },
            Some(&mut retained),
            &mut Recorder::default(),
        );
        assert!(
            result.is_err(),
            "a failing native baseline must not become a successful measurement"
        );
        let retained = retained.expect("completed measurements must survive the baseline error");
        assert_eq!(retained.tests, Some(2));
        assert_eq!(binaries[0].tests, Some(2));
        assert_eq!(retained.peak, None);
    }

    /// A long tail is counted rather than printed, so one line stays one line.
    #[test]
    fn a_long_list_of_groups_is_capped_and_the_rest_counted() {
        let mutants: Vec<Mutant> = (1..=8)
            .map(|ordinal| stuck_mutant(ordinal, &format!("op.{ordinal}"), "subject::f"))
            .collect();
        let plan = stuck_plan(mutants);
        let abandoned = Abandoned {
            reason: "stuck".to_owned(),
            ordinals: (1..=8).collect(),
        };

        let described = describe_stuck(&plan, &["subject".to_owned()], &abandoned);

        assert!(described.contains("and 3 more"), "{described}");
        assert!(!described.contains("op.6"), "{described}");
        assert!(!described.contains("op.8"), "{described}");

        let six: Vec<Mutant> = (1..=6)
            .map(|ordinal| stuck_mutant(ordinal, &format!("boundary.{ordinal}"), "subject::f"))
            .collect();
        let references: Vec<&Mutant> = six.iter().collect();
        assert_eq!(
            tally(&references, |mutant| mutant.mutator.to_string()),
            vec![
                "boundary.1 (1)",
                "boundary.2 (1)",
                "boundary.3 (1)",
                "boundary.4 (1)",
                "boundary.5 (1)",
                "and 1 more",
            ]
        );
    }

    #[test]
    fn a_baseline_without_a_harness_count_is_described_by_elapsed_time_only() {
        let baseline = Baseline {
            elapsed: Duration::from_millis(1500),
            wall: Duration::from_millis(500),
            quiet: Duration::ZERO,
            tests: None,
            peak: None,
        };

        // Custom test harnesses may never announce a count; reporting the elapsed fixed cost is
        // still useful, but inventing a count would be misleading.
        assert_eq!(describe(&baseline, 2), "the suite across 2 test binaries passed in 500.0ms");
    }

    /// A baseline whose peak was measured is described with it, whether or not a ceiling is
    /// actually enforced.
    ///
    /// A project deciding whether a ceiling is worth turning on needs to know what its suite
    /// actually uses, and this line is where that number is cheapest to notice; omitting it whenever
    /// enforcement happens to be off would hide the one number that answers that question.
    #[test]
    fn a_baseline_with_a_measured_peak_is_described_with_it() {
        let baseline = Baseline {
            elapsed: Duration::from_millis(500),
            wall: Duration::from_millis(500),
            quiet: Duration::ZERO,
            tests: Some(4),
            peak: Some(1024 * 1024),
        };

        assert_eq!(
            describe(&baseline, 3),
            "4 tests ran across 3 test binaries in 500.0ms with a peak of 1.0 MB"
        );
    }

    #[test]
    fn a_baseline_summary_uses_the_singular_test_binary_unit() {
        let baseline = Baseline {
            elapsed: Duration::from_millis(500),
            wall: Duration::from_millis(500),
            quiet: Duration::ZERO,
            tests: Some(1),
            peak: None,
        };

        assert_eq!(describe(&baseline, 1), "1 test ran across 1 test binary in 500.0ms");
    }

    #[test]
    fn baseline_progress_distinguishes_disabled_and_empty_suites() {
        #[derive(Default)]
        struct BaselineEvents {
            phases: Vec<(String, String)>,
            progress: Vec<(usize, usize, String)>,
        }

        impl Events for BaselineEvents {
            fn phase(&mut self, verb: &str, detail: &str) {
                self.phases.push((verb.to_owned(), detail.to_owned()));
            }

            fn phase_progress(&mut self, completed: usize, total: usize, unit: &str) {
                self.progress.push((completed, total, unit.to_owned()));
            }

            #[cfg_attr(coverage_nightly, coverage(off))]
            fn mutant(&mut self, _mutant: &Mutant) {}
        }

        let directory = crate::testing::workdir("measure-empty-baseline-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8 root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut disabled = BaselineEvents::default();
        let without = take_baseline(
            &work,
            &mut [],
            &Config {
                baseline: false,
                ..Config::default()
            },
            &MemoryPolicy::default(),
            &mut disabled,
        )
        .expect("a disabled baseline is an empty measurement");

        assert_eq!(without.elapsed, Duration::ZERO);
        assert_eq!(without.wall, Duration::ZERO);
        assert_eq!(without.quiet, Duration::ZERO);
        assert_eq!(without.tests, None);
        assert_eq!(without.peak, None);
        assert_eq!(disabled.phases, vec![(String::new(), "no baseline was measured".to_owned())]);
        assert!(disabled.progress.is_empty());

        let mut empty = BaselineEvents::default();
        let measured = take_baseline(
            &work,
            &mut [],
            &Config {
                jobs: 1,
                ..Config::default()
            },
            &MemoryPolicy::default(),
            &mut empty,
        )
        .expect("an empty suite still completes its baseline");

        assert_eq!(measured.tests, None);
        assert_eq!(empty.progress, vec![(0, 0, "test binaries".to_owned())]);
        assert_eq!(empty.phases.len(), 1);
        assert!(
            empty.phases[0].1.contains("the suite across 0 test binaries passed"),
            "{:?}",
            empty.phases
        );

        let (_directory, work) = crate::testing::helper_workspace("measure-one-baseline-", &["exit:0"]);
        let mut binaries = [crate::testing::helper()];
        binaries[0].budget = Some(Duration::from_secs(1));
        let mut one = BaselineEvents::default();
        let measured = take_baseline(
            &work,
            &mut binaries,
            &Config {
                jobs: 1,
                ..Config::default()
            },
            &MemoryPolicy {
                control: MemoryControl::Off,
                ..MemoryPolicy::default()
            },
            &mut one,
        )
        .expect("one passing binary is measured");

        assert_eq!(measured.tests, None);
        assert_eq!(
            one.progress,
            vec![(0, 1, "test binary".to_owned()), (1, 1, "test binary".to_owned())]
        );
        assert_eq!(one.phases.len(), 1);
        assert!(
            one.phases[0].1.contains("the suite across 1 test binary passed"),
            "{:?}",
            one.phases
        );
    }

    /// Memory control that was never asked for needs nothing from the host, and is returned
    /// exactly as configured.
    ///
    /// Checking host support before even looking at whether measurement was requested would turn a
    /// run that never asked for memory control into one that fails, or degrades with a note, on a
    /// machine that has never had anything to do with the feature at all.
    #[test]
    fn memory_control_that_was_never_asked_for_needs_no_host_support() {
        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Off,
                ..MemoryPolicy::default()
            },
            ..Config::default()
        };

        let (settled, note) =
            settle_memory_control(&config, Err("no cgroup here".to_owned())).expect("a policy that asks for nothing cannot fail");

        assert!(!settled.measuring());
        assert_eq!(note, None);
    }

    #[test]
    fn explicitly_disabled_memory_control_clears_both_limits_without_a_note() {
        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Off,
                demand: Demand::Stated,
                limit: Some(100),
                baseline_limit: Some(200),
                ..MemoryPolicy::default()
            },
            ..Config::default()
        };
        let (settled, note) = settle_memory_control(&config, Ok(())).unwrap();
        assert_eq!(
            settled,
            MemoryPolicy {
                limit: None,
                baseline_limit: None,
                ..config.memory
            }
        );
        assert_eq!(note, None);
    }

    /// A stated memory policy this host cannot deliver is an error, not a silent degradation.
    ///
    /// Someone who passed `--memory` did so because an unbounded mutant would cost them something,
    /// and a run that quietly gave them nothing would be discovered only by the thing they were
    /// trying to prevent.
    #[test]
    fn a_stated_memory_policy_this_host_cannot_deliver_is_an_error() {
        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Measure,
                demand: Demand::Stated,
                ..MemoryPolicy::default()
            },
            ..Config::default()
        };

        let failure = settle_memory_control(&config, Err("no cgroup here".to_owned()))
            .expect_err("a stated policy this host cannot deliver must error");

        assert!(failure.to_string().contains("--memory off"), "{failure}");
        assert!(
            failure.to_string().contains("no cgroup here"),
            "the error must repeat what the host said, {failure}"
        );
    }

    /// A defaulted memory policy this host cannot deliver degrades to no memory control, and says
    /// why rather than merely stating that it degraded.
    ///
    /// Nobody asked for this by name, so refusing to produce a mutation score at all because this
    /// machine has no cgroup delegation would be an obstruction rather than a safeguard.
    #[test]
    fn a_defaulted_memory_policy_this_host_cannot_deliver_degrades_with_a_note() {
        let (settled, note) = settle_memory_control(&Config::default(), Err("no cgroup here".to_owned()))
            .expect("a defaulted policy degrades rather than errors");

        assert!(!settled.measuring());
        assert_eq!(
            note.as_deref(),
            Some("no cgroup here"),
            "a degraded policy must say why, not merely that it degraded"
        );
    }

    #[test]
    fn macos_nextest_disables_inherited_memory_control_with_a_reason() {
        let config = Config {
            nextest: true,
            ..Config::default()
        };
        let support = runner_memory_support(config.nextest, true, Ok(()));
        let (settled, note) = settle_memory_control(&config, support).unwrap();
        assert_eq!(settled, config.memory.disabled());
        assert!(!settled.enforcing());
        assert_eq!(settled.baseline_limit, None);
        assert_eq!(
            note.as_deref(),
            Some(
                "nextest puts each test in its own process group, outside macOS sampled memory control; drop `--nextest` to use memory control"
            )
        );
    }

    #[test]
    fn macos_nextest_refuses_stated_memory_control_and_limits() {
        for memory in [
            MemoryPolicy {
                control: MemoryControl::Measure,
                demand: Demand::Stated,
                ..MemoryPolicy::default()
            },
            MemoryPolicy {
                demand: Demand::Stated,
                ..MemoryPolicy::default()
            },
            MemoryPolicy {
                demand: Demand::Stated,
                limit: Some(100),
                ..MemoryPolicy::default()
            },
            MemoryPolicy {
                control: MemoryControl::Measure,
                demand: Demand::Stated,
                baseline_limit: Some(100),
                ..MemoryPolicy::default()
            },
        ] {
            let config = Config {
                nextest: true,
                memory,
                ..Config::default()
            };
            let support = runner_memory_support(config.nextest, true, Ok(()));
            let error = settle_memory_control(&config, support).unwrap_err();
            assert_eq!(
                error.to_string(),
                "memory control was asked for, but it is not available here: nextest puts each test in its own process group, outside macOS sampled memory control; drop `--nextest` to use memory control.\nRun with `--memory off` to continue without it."
            );
        }
    }

    #[test]
    fn runner_memory_support_preserves_supported_combinations_and_host_failures() {
        for (macos, nextest) in [(true, false), (false, true), (false, false)] {
            let config = Config {
                nextest,
                ..Config::default()
            };
            let support = runner_memory_support(config.nextest, macos, Ok(()));
            let (settled, note) = settle_memory_control(&config, support).unwrap();
            assert_eq!(settled, config.memory);
            assert_eq!(note, None);
            assert_eq!(
                runner_memory_support(nextest, macos, Err("host unavailable".to_owned())),
                Err("host unavailable".to_owned())
            );
        }
    }

    #[test]
    fn macos_nextest_allows_explicitly_disabled_memory_control() {
        let config = Config {
            nextest: true,
            memory: MemoryPolicy {
                control: MemoryControl::Off,
                demand: Demand::Stated,
                limit: Some(100),
                baseline_limit: Some(200),
                ..MemoryPolicy::default()
            },
            ..Config::default()
        };
        let support = runner_memory_support(config.nextest, true, Ok(()));
        let (settled, note) = settle_memory_control(&config, support).unwrap();
        assert_eq!(
            settled,
            MemoryPolicy {
                limit: None,
                baseline_limit: None,
                ..config.memory
            }
        );
        assert_eq!(note, None);
    }

    /// A stated policy this host *can* deliver still errors when there is no baseline to derive a
    /// ceiling from, because a ceiling invented with no measurement behind it would be presented
    /// with exactly the confidence of a measured one.
    #[test]
    fn a_stated_policy_this_host_can_deliver_still_errors_with_no_baseline_to_derive_a_ceiling_from() {
        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Enforce,
                demand: Demand::Stated,
                ..MemoryPolicy::default()
            },
            baseline: false,
            ..Config::default()
        };

        let failure =
            settle_memory_control(&config, Ok(())).expect_err("a stated policy with no baseline to derive a ceiling from must error");

        assert!(failure.to_string().contains("--memory-limit"), "{failure}");
    }

    /// A defaulted policy this host *can* deliver degrades, without a baseline to derive a ceiling
    /// from, instead of erroring — nobody asked for enforcement by name, so there is a note rather
    /// than a hard stop.
    #[test]
    fn a_defaulted_policy_this_host_can_deliver_degrades_with_no_baseline_to_derive_a_ceiling_from() {
        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Enforce,
                ..MemoryPolicy::default()
            },
            baseline: false,
            ..Config::default()
        };

        let (settled, note) = settle_memory_control(&config, Ok(())).expect("a defaulted policy degrades rather than errors");

        assert!(!settled.measuring());
        assert!(note.is_some_and(|note| note.contains("--no-baseline")), "the note must say why");
    }

    /// A stated ceiling with no baseline is admitted: the ceiling was measured by whoever passed
    /// it, so there is nothing left to derive.
    #[test]
    fn a_stated_ceiling_needs_no_baseline_to_derive_one_from() {
        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Enforce,
                limit: Some(1 << 30),
                ..MemoryPolicy::default()
            },
            baseline: false,
            ..Config::default()
        };

        let (settled, note) = settle_memory_control(&config, Ok(())).expect("a stated ceiling needs no baseline");

        assert!(settled.measuring());
        assert_eq!(note, None);
    }

    #[test]
    fn baseline_derivation_guard_requires_all_three_missing_inputs() {
        let (settled, note) = settle_memory_control(&Config::default(), Ok(())).expect("the default baseline supports enforcement");
        assert!(settled.enforcing());
        assert_eq!(note, None);

        let config = Config {
            memory: MemoryPolicy {
                control: MemoryControl::Measure,
                demand: Demand::Stated,
                ..MemoryPolicy::default()
            },
            baseline: false,
            ..Config::default()
        };
        let (settled, note) = settle_memory_control(&config, Ok(())).expect("measurement needs no baseline-derived ceiling");
        assert!(settled.measuring());
        assert!(!settled.enforcing());
        assert_eq!(note, None);
    }

    fn target_in(package: &str, name: &str) -> CompileFailTarget {
        CompileFailTarget {
            package: package.to_owned(),
            target: name.to_owned(),
            harness: "trybuild".to_owned(),
        }
    }

    fn target(name: &str) -> CompileFailTarget {
        target_in("routerama", name)
    }

    fn config_with(exclude: &[&str]) -> Config {
        Config {
            exclude_tests: exclude.iter().map(|pattern| (*pattern).to_owned()).collect(),
            ..Config::default()
        }
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_compile_fail_target_is_named_before_anything_is_built() {
        let mut events = Recorder::default();

        warn_about_compile_fail_targets(
            &[target("router_compile_fail")],
            &["routerama".to_owned()],
            &config_with(&[]),
            &mut events,
        );

        let [warning] = events.warnings.as_slice() else {
            panic!("one admitted target is one warning, got {:?}", events.warnings);
        };

        assert!(warning.contains("router_compile_fail"), "{warning}");
        assert!(warning.contains("--exclude-test router_compile_fail"), "{warning}");
    }

    /// A caller who has already acted on the advice must not be given it again, or the warning
    /// becomes noise that is scrolled past on the run where it matters.
    #[test]
    fn a_target_already_excluded_is_not_warned_about() {
        let mut events = Recorder::default();

        warn_about_compile_fail_targets(
            &[target("router_compile_fail")],
            &["routerama".to_owned()],
            &config_with(&["router_compile_fail"]),
            &mut events,
        );

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// A glob is how these are excluded in practice, and it has to count as having acted.
    #[test]
    fn a_target_excluded_by_a_glob_is_not_warned_about() {
        let mut events = Recorder::default();

        warn_about_compile_fail_targets(
            &[target("router_compile_fail")],
            &["routerama".to_owned()],
            &config_with(&["*_compile_fail"]),
            &mut events,
        );

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// Targets in unrelated workspace packages are not part of the default oracle and must not
    /// prompt the caller to exclude tests that this run will never execute.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_compile_fail_target_outside_the_oracle_is_not_warned_about() {
        let mut events = Recorder::default();

        warn_about_compile_fail_targets(
            &[target("router_compile_fail"), target_in("internity", "compile_fail")],
            &["routerama".to_owned()],
            &config_with(&[]),
            &mut events,
        );

        let [warning] = events.warnings.as_slice() else {
            panic!("only the selected package should produce a warning, got {:?}", events.warnings);
        };

        assert!(warning.contains("router_compile_fail"), "{warning}");
        assert!(!warning.contains("internity"), "{warning}");
        assert!(!warning.contains("--exclude-test compile_fail"), "{warning}");
    }

    #[test]
    fn whole_workspace_oracles_warn_about_compile_fail_targets_outside_the_selected_packages() {
        let mut events = Recorder::default();
        warn_about_compile_fail_targets(
            &[target_in("external", "external_compile_fail")],
            &["routerama".to_owned()],
            &Config {
                test_workspace: true,
                ..Config::default()
            },
            &mut events,
        );
        let [warning] = events.warnings.as_slice() else {
            panic!("a workspace-wide oracle must include the external compile-fail target");
        };
        assert!(warning.contains("--exclude-test external_compile_fail"), "{warning}");
    }

    /// The ordinary case is a workspace with no such target at all, which must say nothing.
    #[test]
    fn a_workspace_without_one_is_silent() {
        let mut events = Recorder::default();

        warn_about_compile_fail_targets(&[], &["routerama".to_owned()], &config_with(&[]), &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// A workspace where `app` links `core`, and both are their own reach.
    fn oracle_plan(mutating: &str) -> Plan {
        let mut reach: HashMap<String, crate::HashSet<String>> = HashMap::default();

        let _app = reach.insert("app".to_owned(), ["app".to_owned(), "core".to_owned()].into_iter().collect());
        let _core = reach.insert("core".to_owned(), core::iter::once("core".to_owned()).collect());

        let mut pending = Mutant {
            ordinal: 1,
            outcome: Outcome::Pending,
            ..crate::testing::advise_fixture::mutant("a.rs", "arith.add_to_sub", Outcome::Pending, 0)
        };

        pending.package = mutating.to_owned().into();

        Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants: vec![pending],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach,
            specs: HashMap::default(),
        }
    }

    fn oracle_binary(package: &str, tests: Option<usize>) -> TestBinary {
        TestBinary {
            package: package.to_owned(),
            tests,
            ..crate::testing::test_binary("/tmp/t")
        }
    }

    fn capped(packages: &[String]) -> TestScope<'_> {
        TestScope {
            packages,
            package_local: false,
            whole_workspace: false,
        }
    }

    #[test]
    fn empty_oracle_warnings_delimit_withdrawn_packages_and_count_only_unshown_helpers() {
        let packages = ["core".to_owned()];
        let scope = capped(&packages);
        let mut events = Recorder::default();
        warn_about_an_empty_oracle(
            &oracle_plan("core"),
            &[],
            &scope,
            false,
            &["alpha".to_owned(), "beta".to_owned()],
            &mut events,
        );
        assert_eq!(events.warnings.len(), 1);
        assert!(
            events.warnings[0].contains("The preflight could not compile `alpha`, `beta`, so their tests were withdrawn"),
            "{:?}",
            events.warnings
        );

        for helpers in [2, 7] {
            let mut plan = oracle_plan("core");
            plan.reach.clear();
            for index in 1..=helpers {
                let _previous = plan
                    .reach
                    .insert(format!("helper{index}"), core::iter::once("core".to_owned()).collect());
            }
            let mut events = Recorder::default();
            warn_if_oracle_is_empty(&plan, &[], &scope, &Config::default(), &[], &mut events);
            let [warning] = events.warnings.as_slice() else {
                panic!("an inferred empty oracle must explain which helpers can supply its missing tests");
            };
            let names = if helpers == 2 {
                "`helper1`, `helper2`"
            } else {
                "`helper1`, `helper2`, `helper3`, `helper4`, `helper5` and 2 more"
            };
            assert!(
                warning.ends_with(&format!(
                    "{names} also links this code. Add `--test-package helper1` to let its tests decide a verdict, or `--test-workspace` for every package's"
                )),
                "{warning}"
            );
            assert!(!warning.contains("and 0 more"), "{warning}");
        }
    }

    #[test]
    fn sweep_costs_preserve_funnel_measurements() {
        assert!(sweep_cost(None, Duration::from_secs(1)).is_none());
        let cost = sweep_cost(
            Some(super::super::sweep::Spent {
                launches: 1,
                probes: 2,
                exact_probes: 3,
                exact_hits: 4,
                inferred_probes: 5,
                inferred_hits: 6,
                saved: 7,
                reach_saved: 8,
                ..super::super::sweep::Spent::default()
            }),
            Duration::from_secs(9),
        )
        .expect("completed sweep has a phase");
        assert_eq!(
            (
                cost.elapsed,
                cost.launches,
                cost.probes,
                cost.exact_probes,
                cost.exact_hits,
                cost.inferred_probes,
                cost.inferred_hits,
                cost.launches_saved,
                cost.reach_launches_saved,
            ),
            (Duration::from_secs(9), 1, 2, 3, 4, 5, 6, 7, 8)
        );
    }

    /// The crate being mutated has no tests of its own, but the crate above it does.
    ///
    /// Left unsaid, this run reports every mutant as uncovered and reads like a verdict on the code
    /// rather than on the scope the cap chose.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_cap_that_leaves_the_mutants_unjudged_names_the_package_that_could_judge_them() {
        let mut events = Recorder::default();
        let plan = oracle_plan("core");
        let named = [String::from("core")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(&plan, &[oracle_binary("core", Some(0))], &scope, true, &[], &mut events);

        let [warning] = events.warnings.as_slice() else {
            panic!("one unjudged package is one warning, got {:?}", events.warnings);
        };

        assert!(warning.contains("`app`"), "the package that could help is not named: {warning}");
        assert!(warning.contains("--test-package"), "the remedy is not named: {warning}");
        assert!(warning.contains("--test-workspace"), "the wider remedy is not named: {warning}");
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn empty_oracle_advice_sorts_caps_and_counts_helper_packages() {
        let mut events = Recorder::default();
        let mut plan = oracle_plan("core");
        let _app = plan.reach.remove("app");
        for helper in ["zeta", "beta", "eta", "delta", "alpha", "gamma"] {
            let _previous = plan
                .reach
                .insert(helper.to_owned(), ["core".to_owned(), helper.to_owned()].into_iter().collect());
        }
        let named = [String::from("core")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(&plan, &[oracle_binary("core", Some(0))], &scope, true, &[], &mut events);

        let [warning] = events.warnings.as_slice() else {
            panic!("one empty oracle produces one warning, got {:?}", events.warnings);
        };
        assert!(warning.contains("`alpha`, `beta`, `delta`, `eta`, `gamma` and 1 more"), "{warning}");
        assert!(warning.contains("--test-package alpha"), "{warning}");
        assert!(!warning.contains("`zeta` also links"), "{warning}");
    }

    /// A retreat narrows the oracle the caller named; it never replaces it with a different one.
    ///
    /// `--test-package p` with mutants in `q` is a caller who chose `p`'s tests as the oracle and
    /// deliberately left `q`'s out. Substituting the mutated packages hands the run exactly the
    /// binaries they opted out of, and reports the result as though they had asked for it.
    #[test]
    fn a_retreat_does_not_substitute_the_mutated_packages_for_the_oracle_the_caller_named() {
        let requested = vec![String::from("p"), String::from("r")];
        let intended = vec![String::from("q")];

        let kept = narrowed_oracle(requested, intended, &[String::from("p")]).unwrap();

        assert_eq!(kept, vec![String::from("r")], "the retreat may only subtract");
    }

    /// A retreat that leaves no oracle at all is an error, not a run that judges nothing.
    ///
    /// Every mutant would come back uncovered, which reads as a fact about the code rather than
    /// about a build that never happened.
    #[test]
    fn a_retreat_that_empties_the_oracle_is_reported_rather_than_run() {
        let requested = vec![String::from("p")];
        let intended = vec![String::from("q")];

        let cause = narrowed_oracle(requested, intended, &[String::from("p")]).unwrap_err();

        assert!(cause.to_string().contains("`p`"), "{cause}");
        assert!(cause.to_string().contains("none of the packages"), "{cause}");
    }

    /// An unrestricted oracle cannot be subtracted from, so the checked set is named outright.
    #[test]
    fn a_retreat_from_an_unrestricted_oracle_names_what_was_checked() {
        let intended = vec![String::from("q")];

        let kept = narrowed_oracle(Vec::new(), intended, &[String::from("p")]).unwrap();

        assert_eq!(kept, vec![String::from("q")]);
    }

    /// A retreat is what the advisory blames when there was one, whoever chose the oracle.
    ///
    /// Blaming the cap would send the reader to widen a scope that was never the problem, and the
    /// remedy the cap offers — `--test-package <p>` — would name a package that has just failed to
    /// compile.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_oracle_emptied_by_a_retreat_blames_the_retreat_rather_than_the_cap() {
        let mut events = Recorder::default();
        let plan = oracle_plan("core");
        let named = [String::from("core")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(
            &plan,
            &[oracle_binary("core", Some(0))],
            &scope,
            false,
            &[String::from("app")],
            &mut events,
        );

        let [warning] = events.warnings.as_slice() else {
            panic!("a retreat that empties the oracle is one warning, got {:?}", events.warnings);
        };

        assert!(warning.contains("could not compile `app`"), "{warning}");
        assert!(
            !warning.contains("--test-package"),
            "the remedy must not name what just failed: {warning}"
        );
    }

    /// A suite that does convict things must never be told it does not exist.
    #[test]
    fn a_cap_whose_own_tests_reach_the_mutants_says_nothing() {
        let mut events = Recorder::default();
        let plan = oracle_plan("core");
        let named = [String::from("core")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(&plan, &[oracle_binary("core", Some(4))], &scope, true, &[], &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    #[test]
    fn one_judged_mutant_or_binary_is_enough_to_suppress_empty_oracle_advice() {
        let named = [String::from("core")];
        let scope = capped(&named);

        let mut two_packages = oracle_plan("core");
        let mut app_mutant = two_packages.mutants[0].clone();
        app_mutant.id = "app-mutant".into();
        app_mutant.ordinal = 2;
        app_mutant.package = "app".to_owned().into();
        two_packages.mutants.push(app_mutant);
        let mut events = Recorder::default();
        warn_about_an_empty_oracle(&two_packages, &[oracle_binary("core", Some(1))], &scope, true, &[], &mut events);
        assert!(events.warnings.is_empty(), "{:?}", events.warnings);

        let plan = oracle_plan("core");
        let mut unreachable = oracle_binary("unrelated", Some(0));
        unreachable.package = "unrelated".to_owned();
        let mut events = Recorder::default();
        warn_about_an_empty_oracle(
            &plan,
            &[oracle_binary("core", Some(1)), unreachable],
            &scope,
            true,
            &[],
            &mut events,
        );
        assert!(events.warnings.is_empty(), "{:?}", events.warnings);

        let mut no_pending = oracle_plan("core");
        no_pending.mutants[0].outcome = Outcome::Killed;
        let mut events = Recorder::default();
        warn_about_an_empty_oracle(&no_pending, &[oracle_binary("core", Some(0))], &scope, true, &[], &mut events);
        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// `None` is a suite nobody counted, not a suite with nothing in it.
    ///
    /// `--no-baseline` and a `harness = false` target both produce it, and reading it as empty would
    /// announce a missing oracle on the strength of a measurement never taken.
    #[test]
    fn an_uncounted_suite_is_not_reported_as_an_empty_one() {
        let mut events = Recorder::default();
        let plan = oracle_plan("core");
        let named = [String::from("core")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(&plan, &[oracle_binary("core", None)], &scope, true, &[], &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// A caller who named `--test-package` has already answered this, and must not be asked again.
    #[test]
    fn a_cap_the_caller_chose_is_left_to_stand() {
        let mut events = Recorder::default();
        let plan = oracle_plan("core");
        let named = [String::from("core")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(&plan, &[oracle_binary("core", Some(0))], &scope, false, &[], &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// Nothing else links the code, so widening the oracle has nothing to offer and there is no
    /// advice to give — the mutants really are untested.
    #[test]
    fn a_package_nothing_else_links_gets_no_advice() {
        let mut events = Recorder::default();
        let plan = oracle_plan("app");
        let named = [String::from("app")];
        let scope = capped(&named);

        warn_about_an_empty_oracle(&plan, &[oracle_binary("app", Some(0))], &scope, true, &[], &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// A run already testing the whole workspace is never told to widen its oracle.
    ///
    /// The command line rejects `--test-workspace` beside `--test-package`, but a configuration file
    /// can set both, and then the scope carries a package list *and* the whole-workspace answer. The
    /// advice would be to turn on what is already on.
    #[test]
    fn a_whole_workspace_oracle_is_never_told_to_widen() {
        let mut events = Recorder::default();
        let plan = oracle_plan("core");
        let named = [String::from("harness")];
        let scope = TestScope {
            packages: &named,
            package_local: false,
            whole_workspace: true,
        };

        warn_about_an_empty_oracle(&plan, &[oracle_binary("harness", Some(0))], &scope, true, &[], &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }
}
