// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Preparing the tree, building the schema, and driving a run to its verdicts.

use core::fmt::Write as _;
use core::time::Duration;
use std::time::Instant;

use cargo_gamma_process::MemoryRequest;

use super::baseline::{Baseline, measure_baseline};
use super::build::{Abandoned, Converger};
use super::census::{self, Census};
use super::config::Config;
use super::events::Events;
use super::killers::Killers;
use super::memory;
use super::memory::MemoryPolicy;
use super::session::{CensusCost, Phases, Session, SweepCost};
use super::stall::Stall;
use super::sweep::{Sweep, test_all};
use super::test_binary::{
    Reachability, TestBinary, TestScope, admits_target, build_packages, calibrate, oracle_packages, reaches, reaching_packages, restrict,
    unmatched_test, workload,
};
use super::workspace::{Workspace, gamma_base};
use crate::discover::{CompileFailTarget, Plan, Survey, compile_fail_advice};
use crate::error::error;
use crate::estimate::project;
use crate::model::Outcome;
use crate::ops::registry::Selection;
use crate::{HashMap, HashSet, Result};

/// How many groups a "not run, by …" line names before it starts counting the rest.
const GROUP_LIMIT: usize = 5;

/// Runs every live mutant in the plan, writing verdicts back onto it.
///
/// # Errors
///
/// Returns an error if the tree cannot be prepared, the build cannot be made to succeed, or the
/// baseline does not pass — a failing baseline means every comparison in the run has nothing to
/// compare against.
pub fn run(survey: &Survey, selection: &Selection, config: &Config, events: &mut impl Events) -> Result<Measured> {
    run_with_locks(survey, selection, config, events, None)
}

pub(crate) fn run_with_locks(
    survey: &Survey,
    selection: &Selection,
    config: &Config,
    events: &mut impl Events,
    locks: Option<super::workspace::CacheLocks>,
) -> Result<Measured> {
    let Measured {
        mut plan,
        built,
        stuck,
        dropped,
    } = measure_with_locks(survey, selection, config, events, locks)?;

    // Nothing was live, so nothing was copied, built or measured. The plan still describes every
    // mutant that was found and why each one is not being run, which is what the caller reports.
    let Some(mut built) = built else {
        return Ok(Measured {
            plan,
            built: None,
            stuck,
            dropped,
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
    let base = gamma_base(&survey.root, config.cache_dir.as_deref());

    let mut killers = if config.incremental.is_enabled() {
        Killers::load(&base, &survey.root)
    } else {
        Killers::default()
    };

    // Built once and threaded through every phase that needs "which binaries can this package's
    // mutants reach": census economics, the workload projection, and the sweep's own scheduling and
    // verdict execution. Each of those used to answer that question with its own pass over `plan`
    // and `binaries`; a plan's package/binary shape does not change mid-run, so one shared, ordered
    // index answers all three identically and for a fraction of the work.
    let reach = Reachability::build(&plan, &built.session.binaries, &scope);

    // Taken after the baseline and before the first mutant, because it needs what the baseline
    // established — how long a binary takes, which is the budget a censused test is held to — and
    // because everything it decides is about how the sweep runs.
    let (census_targets, maximum_census_savings) = census_targets(&plan, &reach, &killers);
    let census_requested = !config.whole_test_binaries && !census_targets.is_empty() && !maximum_census_savings.is_zero();
    let census_started = Instant::now();
    let census = if census_requested {
        census::take(
            &built.work,
            &built.session.binaries,
            &census_targets,
            maximum_census_savings,
            config.jobs,
            stall,
            events,
        )
    } else {
        Census::default()
    };

    // Only recorded when a census actually ran: an absent census is not a census that took no time,
    // and the whole question is whether the default selection paid for itself.
    if census_requested {
        built.session.phases.census = Some(CensusCost {
            elapsed: census_started.elapsed(),
            walked: census.walked(),
            binaries: census_targets.len(),
        });
    }

    let work = workload(&plan.mutants, &reach, (!config.whole_test_binaries).then_some(&census));
    let projection = project(&plan.mutants, work, built.session.baseline_wall, built.session.build, config.jobs);

    events.measured(&plan, &built.session, &projection);

    let sweep = Sweep {
        timeout_floor: config.timeout_floor,
        stall,
        jobs: config.jobs,
        meter: built.session.metered,
        confirm: config.confirm,
        census: &census,
    };

    let sweep_started = Instant::now();
    let swept = test_all(&built.work, &mut plan, &reach, sweep, &mut killers, events);
    let sweep_elapsed = sweep_started.elapsed();

    // Written even when the sweep failed. A run that stopped partway still learned which test
    // caught every mutant it got to, and discarding that would make an abandoned run cost the next
    // one as much as it cost this one.
    if config.incremental.is_enabled() {
        killers.store(&base, &plan.mutants);
    }

    let spent = swept?;

    // `None` when nothing was swept: the phase is recorded as absent rather than as a real phase
    // that happened to cost nothing, which is what [`Phases::sweep`] documents its `Option` to mean.
    built.session.phases.sweep = spent.map(|spent| SweepCost {
        elapsed: sweep_elapsed,
        launches: spent.launches,
        probes: spent.probes,
    });

    Ok(Measured {
        plan,
        built: Some(built),
        stuck,
        dropped,
    })
}

/// Selected mutation sites each test binary can reach and the most case selection could save.
///
/// The duration is a serial upper bound: even a perfect census cannot avoid more than one whole
/// baseline run for each reachable mutant/binary pair.
///
/// A mutant carrying an exact checked killer hint that actually names a binary this mutant can
/// reach does not itself justify or pay for censusing any binary: [`sweep::mutant_cost`](super::sweep)
/// already prices it at one binary baseline (the probe), so a census could only ever shave time off
/// a launch the sweep does not expect to need to run in full. This eligibility test has to mirror
/// [`sweep::judge_ordered`](super::sweep)'s own hint precedence exactly — a hint naming a package
/// or target this run no longer builds, or one the test packages were narrowed away from, is
/// exactly as stale there as it would be here, and `judge_ordered` falls straight through it to the
/// unhinted binaries. A mutant whose hint cannot be matched to any reachable binary is therefore
/// treated as unhinted here too, entering the first pass below and justifying census normally,
/// rather than silently skipping census on the strength of a hint the sweep will never honor.
///
/// A hint that *is* eligible is only ever a *guess* until the sweep checks it — the probe can still
/// miss, at which point the mutant falls back to every binary that reaches it, exactly as if it had
/// never been hinted. So a validly hinted mutant's ordinal still rides along for free on any
/// binary an unhinted mutant already justified censusing (a shared site): skipping that would leave
/// the site absent from the completed census, which [`Census::selection`] and [`Census::work`]
/// would then read as *proven unreached* rather than merely unmeasured for this mutant — silently
/// turning a stale-hint fallback into a wrong `Uncovered` verdict. A binary no unhinted mutant ever
/// justifies censusing gets no census entry for the hinted mutant either, so a stale hint there
/// conservatively falls back to running that binary whole, never to skipping it as though it were
/// proven safe.
fn census_targets(plan: &Plan, reach: &Reachability<'_>, killers: &Killers) -> (HashMap<camino::Utf8PathBuf, HashSet<u32>>, Duration) {
    let mut targets: HashMap<camino::Utf8PathBuf, HashSet<u32>> = HashMap::default();
    let mut maximum_savings = Duration::ZERO;

    let pending = || {
        plan.mutants
            .iter()
            .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
    };

    // A hint only excuses a mutant from justifying its own census when it names a binary this
    // mutant can actually reach — the exact eligibility test `judge_ordered` itself applies before
    // trusting a hint. A stale or unreachable hint (naming a package or target this run dropped, or
    // one the test packages were narrowed away from) fails this test exactly as it fails there, so
    // such a mutant is treated as unhinted below rather than silently going uncensused.
    let eligibly_hinted = |mutant: &crate::model::Mutant| {
        killers.hint(&mutant.id).is_some_and(|hint| {
            reach
                .reachable(&mutant.package)
                .is_some_and(|reachable| reachable.iter().any(|binary| hint.names(&binary.package, &binary.target)))
        })
    };

    // Unhinted mutants (including those whose hint is stale or unreachable) alone decide which
    // binaries get censused at all, and are the only ones that pay the baseline into
    // `maximum_savings` — an eligibly hinted mutant riding along below never grows it.
    for mutant in pending().filter(|mutant| !eligibly_hinted(mutant)) {
        let Some(reachable) = reach.reachable(&mutant.package) else {
            continue;
        };

        for binary in reachable {
            let _new = targets.entry(binary.path.clone()).or_default().insert(mutant.ordinal);
            maximum_savings = maximum_savings.saturating_add(binary.baseline);
        }
    }

    // An eligibly hinted mutant only ever adds its ordinal to a binary some unhinted mutant already
    // put in `targets` — never a binary of its own, since that would make the census pay to select
    // cases for a probe it expects not to need.
    for mutant in pending().filter(|mutant| eligibly_hinted(mutant)) {
        let Some(reachable) = reach.reachable(&mutant.package) else {
            continue;
        };

        for binary in reachable {
            if let Some(sites) = targets.get_mut(&binary.path) {
                let _new = sites.insert(mutant.ordinal);
            }
        }
    }

    (targets, maximum_savings)
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

/// The packages whose tests decide verdicts, owned so a later phase can borrow it back.
#[derive(Debug, Clone)]
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
/// This is what lets every later compiler error be absorbed. The staged builds and the baseline
/// compile this same tree with guards written into it, so once this passes, an error that appears
/// afterwards was introduced by a mutant and the rollback loop can withdraw it without troubling
/// anyone. Without it, a tree whose own test targets do not compile fails in the middle of a run
/// and reports a mutant it cannot name, which sends the reader hunting through their own source
/// for a fault that was there before the tool arrived.
///
/// The packages named are the ones the final build will compile, but computed from the packages
/// this run intends to mutate rather than from the ones that turn out to hold live mutants. That
/// is a superset, and a superset is the safe direction: a target left unchecked here would have
/// its genuine errors absorbed later as though a mutant had caused them.
/// Returns the packages the check had to give up on to succeed at all, empty in the ordinary case.
/// They are the caller's problem as much as the tool's: see [`Converger::preflight`].
fn preflight(
    survey: &Survey,
    plan: &Plan,
    work: &Workspace,
    config: &Config,
    converger: &mut Converger,
    events: &mut impl Events,
) -> Result<Cleared> {
    events.begin("Validating", "Validated", "workspace");

    let requested: Vec<String> = oracle_packages(&survey.selected, config);
    let package_local = config.test_packages.is_empty() && !config.test_workspace;
    let scope = TestScope {
        packages: &requested,
        package_local,
        whole_workspace: config.test_workspace,
    };

    let intended = survey.packages();
    let intending: crate::HashSet<&str> = intended.iter().map(String::as_str).collect();
    let checking = reaching_packages(&survey.reach, &intending, &scope);
    let cleared = Converger::preflight(work, plan, checking.as_deref(), &intended, config.build, events)?;
    events.end("");
    let dropped = cleared.dropped;

    // The narrowed check failed and only the whole workspace passed, so every build this run makes
    // asks for the same feature unification. Narrowing again would rebuild the failure the check
    // has already shown belongs to the scope rather than to any mutant, and the rollback loop would
    // charge it to whichever mutants it happened to blame.
    if cleared.whole_workspace {
        converger.require_whole_workspace();
    }

    // The check only passed because it stopped asking about those packages, so the rest of the run
    // stops asking too. Building test targets this run has decided cannot convict anything would be
    // paying again for the compile that just failed, and running them would mean running binaries
    // the preflight never cleared — the one thing the preflight exists to prevent.
    if dropped.is_empty() {
        return Ok(Cleared {
            packages: requested,
            whole_workspace: config.test_workspace,
            dropped,
        });
    }

    Ok(Cleared {
        packages: narrowed_oracle(requested, intended, &dropped)?,
        whole_workspace: config.test_workspace,
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
/// Each package is taken from source to compiled object before the next one starts, in an order
/// where a package always follows what it depends on. That order is forced anyway — a mutant
/// cannot produce a diagnostic until everything its own package depends on compiles clean — and
/// following it deliberately lets a run say which package it is working on, and say it once, before
/// the wait rather than after it.
///
/// The tree is copied on demand rather than up front. A run whose every mutant turns out to be
/// suppressed has nothing to build, and it should not pay for a copy to discover that.
///
/// # Errors
///
/// Returns an error if a file cannot be parsed, the tree cannot be prepared, the build cannot be
/// made to succeed, or the baseline does not pass — a failing baseline means every comparison in
/// the run has nothing to compare against.
pub fn measure(survey: &Survey, selection: &Selection, config: &Config, events: &mut impl Events) -> Result<Measured> {
    measure_with_locks(survey, selection, config, events, None)
}

fn measure_with_locks(
    survey: &Survey,
    selection: &Selection,
    config: &Config,
    events: &mut impl Events,
    locks: Option<super::workspace::CacheLocks>,
) -> Result<Measured> {
    let started = Instant::now();

    let (memory, unbounded) = admit_memory_control(config)?;

    // Checked against what the workspace declares, before anything is copied or compiled. A typo
    // here changes which tests get to convict a mutant, so it should cost a second rather than a
    // full instrumented build.
    if let Some(pattern) = unmatched_test(&survey.tests, &config.include_tests, &config.exclude_tests) {
        return Err(error!("no test target matches `{pattern}`; patterns match cargo target names, not test function names").usage());
    }

    // Said before the tree is copied, let alone built. A compile-fail target is expensive per
    // mutant rather than once, so by the time its cost is visible in the progress display the run
    // has already committed hours to it — and what it looks like from outside is a run that hung.
    // Checked against the oracle and its filters so that unrelated workspace packages and targets
    // the caller has already excluded are not presented as costs of this run.
    warn_about_compile_fail_targets(&survey.compile_fail, &survey.selected, config, events);

    let mut plan = survey.skeleton();
    let mut converger = Converger::guided(ordering_hints(survey, config));

    // The scratch-tree copy, timed on its own: it is a component of the build's total, but a large
    // workspace can spend as much duplicating itself as compiling, and the aggregate cannot say
    // which.
    let copy_started = Instant::now();
    let mut work = Workspace::prepare_with_locks(&plan.root, config, events, locks)?;
    let copy = copy_started.elapsed();

    // Settled once, before anything is spawned, so the baseline and the sweep cannot disagree about
    // how wide the workload they measure and judge is.
    work.calibrate_harness(config.jobs);

    let preflight_started = Instant::now();
    let Cleared {
        packages,
        whole_workspace,
        dropped,
    } = preflight(survey, &plan, &work, config, &mut converger, events)?;
    let preflight_elapsed = preflight_started.elapsed();
    let oracle = Oracle::new(packages, config.test_packages.is_empty() && !config.test_workspace, whole_workspace);
    let scope = oracle.scope();

    let Staged { anything_live, mut stuck } = converge_stages(survey, selection, &mut plan, &mut converger, &work, config, events)?;

    plan.sort();
    converger.plan_reordered();

    // Nothing was live anywhere, or everything live was in a build that could not be made to
    // compile. Either way there is nothing left to build, measure or run, and the verdicts the
    // abandoned population carries are already written onto the plan.
    if !anything_live {
        converger.settle(&mut plan);

        return Ok(Measured {
            plan,
            built: None,
            stuck,
            dropped,
        });
    }

    // One line for the whole fixed cost that is left. The test binaries are built and then
    // immediately run with no mutant active, and neither half means anything without the other:
    // the build is what makes a baseline possible, and the baseline is what says the build was
    // worth having.
    events.begin("Baselining", "Baseline", "building the test binaries and running the suite");

    // The staged builds compiled libraries only. This is the build that compiles the test targets
    // and settles the run, and it withdraws whatever only a test target could have revealed. Only
    // the packages whose tests can actually be selected are asked for: the rest would be compiled,
    // baselined and never consulted. The preflight check cleared this same set, narrowed from the
    // packages that turned out to hold live mutants rather than from those the run set out to
    // mutate, so it is a subset of what was checked.
    let select = build_packages(&plan, &scope);
    let mut build = converger.finish(&work, &mut plan, select.as_deref(), config.build, events)?;

    // The build that decides the run could not be made to compile, so there is no test binary to
    // judge anything with and nothing left to measure. Every mutant still live carries
    // `NotBuilt`, which is what tells the reader that these are mutants nobody ran rather than
    // mutants the suite let through, and the run reports that rather than exiting empty-handed.
    if let Some(abandoned) = build.stuck {
        let count = abandoned.ordinals.len();

        stuck.push(describe_stuck(&plan, select.as_deref().unwrap_or_default(), &abandoned));
        events.complete(&format!(
            "the build could not be made to compile, {} not run",
            crate::report::quantity(count, "mutant")
        ));

        return Ok(Measured {
            plan,
            built: None,
            stuck,
            dropped,
        });
    }

    // Before the baseline, so the shares `apportion` computes describe the suite that will actually
    // run. A run with nothing left to run it cannot decide anything: every mutant would survive
    // unopposed and the report would read as a total failure of the test suite rather than as the
    // filter having eaten it.
    let present = build.binaries.len();

    restrict(&mut build.binaries, &config.include_tests, &config.exclude_tests);

    let filtered = present.saturating_sub(build.binaries.len());

    if build.binaries.is_empty() && present > 0 {
        return Err(error!("`--include-test` and `--exclude-test` left no test target to decide a verdict").usage());
    }

    let build_time = started.elapsed();

    // Armed after the build and before the baseline. The metadata nextest is handed describes
    // binaries that do not exist until the build has run, and the baseline has to be measured
    // through the same runner that will judge every mutant — a baseline taken one way and compared
    // against verdicts reached the other measures nothing.
    if config.nextest {
        work.arm_nextest(&build.binaries)?;
    }

    let baseline = take_baseline(&work, &mut build.binaries, config, &memory, events)?;

    warn_about_an_empty_oracle(&plan, &build.binaries, &scope, config.test_packages.is_empty(), &dropped, events);

    let stall = calibrate_stall(&baseline, config);

    calibrate(&mut build.binaries, config, &memory);

    let session = Session {
        baseline: baseline.elapsed,
        baseline_wall: baseline.wall,
        tests: baseline.tests,
        quiet: baseline.quiet,
        stall: stall.budget,
        build: build_time,
        peak: baseline.peak,
        metered: memory.measuring(),
        unbounded,
        withdrawn: build.withdrawn,
        census: build.census,
        rounds: build.rounds,
        rounds_taken: build.history,
        binaries: build.binaries,
        scratch: work.base().to_owned(),
        filtered,
        widened: build.widened,
        ordering: build.ordering,
        phases: Phases {
            copy,
            preflight: preflight_elapsed,
            census: None,
            sweep: None,
        },
    };

    // Everything that could have failed has. What was built is now worth keeping, so that the next
    // run in this workspace is incremental rather than starting cold.
    work.settle();

    Ok(Measured {
        plan,
        built: Some(Built { work, session, oracle }),
        stuck,
        dropped,
    })
}

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

/// What the staged builds settled before the run reaches the build that decides it.
struct Staged {
    /// Whether any stage got as far as compiling live mutants.
    anything_live: bool,

    /// One rendered diagnostic per stage the run could not build.
    stuck: Vec<String>,
}

/// The mutants an earlier run could not compile, offered to the build as an order rather than a fact.
///
/// Two sources, unioned: the scratch record left by whatever ran here last, and the artifact the
/// workspace checks in. Neither is filtered by build context, and that is the point — this is the
/// tier that survives a context mismatch, because being wrong about the order costs the order and
/// nothing else. What comes back is a set of mutant ids; [`Converger`] decides what to do with it,
/// and every mutant named is still built, still judged, and still scored exactly as if it had never
/// appeared here. See [`crate::exec::OrderingHints`].
///
/// Empty when incremental mode is off. `--incremental no` is what a caller reaches for when they suspect the
/// tool is remembering something it should not, so it turns off remembering — including the tiers
/// that could not have affected the answer.
fn ordering_hints(survey: &Survey, config: &Config) -> crate::HashSet<crate::model::MutantId> {
    if !config.incremental.is_enabled() {
        return crate::HashSet::default();
    }

    let base = gamma_base(&survey.root, config.cache_dir.as_deref());
    let record = crate::discover::RunRecord::load(&base);
    let checked_in = crate::discover::Hints::load(&survey.root);

    record
        .ordering()
        .into_iter()
        .map(crate::model::MutantId::new)
        .chain(checked_in.ordering().into_iter().map(crate::model::MutantId::new))
        .collect()
}

/// Scans, links and builds each stage in dependency order.
///
/// A stage that cannot be made to compile does not end the run. Its own mutants come back out of
/// the tree, which restores the very sources the preflight check already proved compile, so every
/// later stage is asked of a tree in no worse a state than the one this stage started from — and a
/// run that gets stuck early still ends up saying what it learned everywhere else. Stopping at the
/// first failure would answer "we got stuck in the first crate" with nothing at all about the
/// twenty that follow it.
fn converge_stages(
    survey: &Survey,
    selection: &Selection,
    plan: &mut Plan,
    converger: &mut Converger,
    work: &Workspace,
    config: &Config,
    events: &mut impl Events,
) -> Result<Staged> {
    let mut ordinals = 0_u32;
    let mut staged = Staged {
        anything_live: false,
        stuck: Vec::new(),
    };

    for stage in &crate::discover::stages(&survey.packages(), &survey.reach) {
        let name = stage.join(", ");

        // Named on the way in, before its files have even been read. Scanning and then compiling a
        // large crate is the longest a run goes without saying anything, and what makes that wait
        // legible is knowing whose wait it is.
        events.begin("Mutating", "Mutated", &name);

        let mut live = 0_usize;

        for package in stage {
            let scanned = survey.scan(Some(package), selection, &mut ordinals)?;

            live = live.saturating_add(scanned.mutants.iter().filter(|mutant| mutant.ordinal > 0).count());
            plan.absorb(scanned);
        }

        // A package with nothing to run is still named. A crate that quietly takes no part in a run
        // is worth noticing, and leaving it out of the sequence would make it look like it had
        // simply not been looked at.
        if live == 0 {
            events.end(", no mutants");
            continue;
        }

        for package in stage {
            work.link_runtime(package, &plan.files)?;
        }

        let before = converger.withdrawn();
        if let Some(abandoned) = converger.stage(work, plan, stage, config.build, events)? {
            let count = abandoned.ordinals.len();

            staged.stuck.push(describe_stuck(plan, stage, &abandoned));
            events.end(&format!(
                ", the build could not be made to compile, {} not run",
                crate::report::quantity(count, "mutant")
            ));

            continue;
        }

        staged.anything_live = true;

        // The count that closes the line is what survived compilation, which is why it waits for
        // the build. A mutant that could not compile is a fact about the tool rather than about the
        // code, and the summary accounts for all of them once.
        let viable = live.saturating_sub(converger.withdrawn().saturating_sub(before));

        events.end(&format!(", {}", crate::report::quantity(viable, "viable mutant")));
    }

    Ok(staged)
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
const NAMED_HELPERS: usize = 3;

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
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .map(|mutant| &*mutant.package)
        .collect();

    if mutated.is_empty() {
        return;
    }

    // A binary that announced tests it never counted is not evidence of an empty suite, so `None`
    // is read as "there may well be tests here" and stops the warning. Saying this over a suite
    // that does convict things would be worse than saying nothing.
    let judged = mutated.iter().any(|package| {
        binaries
            .iter()
            .any(|binary| binary.tests != Some(0) && reaches(binary, package, plan, scope))
    });

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

/// Runs the suite once with no mutant active, or reports that nothing was measured.
///
/// The measurement is taken at the sweep's own concurrency, because calibrating on an idle machine
/// and spending on a loaded one makes every budget derived from it too tight — and a mutant that
/// loses that race is recorded as a timeout, which lowers the score and can fail the run.
fn take_baseline(
    work: &Workspace,
    binaries: &mut [TestBinary],
    config: &Config,
    memory: &MemoryPolicy,
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
        meter: memory.measuring(),
        limit: memory.baseline_limit,
    };

    let total = binaries.len();
    let unit = if total == 1 { "test binary" } else { "test binaries" };
    let mut completed = 0_usize;

    events.phase_progress(completed, total, unit);

    let measured = measure_baseline(work, binaries, request, config.jobs, || {
        completed += 1;
        events.phase_progress(completed, total, unit);
    })?;

    // Reported after the fact rather than before, because the figures everything downstream is
    // derived from — each test binary's timeout is scaled from its baseline by `--test-timeout-multiplier`, and
    // the stall budget is calibrated from the longest silence within the baseline — only exist once the suite
    // has actually run.
    events.complete(&describe(&measured));

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
fn admit_memory_control(config: &Config) -> Result<(MemoryPolicy, Option<String>)> {
    settle_memory_control(config, memory::support())
}

/// Decides what memory control a run gets, given what the host can deliver.
///
/// Host support is passed in rather than probed here so that every branch is decided by its
/// arguments alone: whether a machine happens to have a delegated cgroup then has no bearing on
/// which of these paths a test exercises.
fn settle_memory_control(config: &Config, support: Result<(), String>) -> Result<(MemoryPolicy, Option<String>)> {
    let policy = config.memory;

    if !policy.measuring() {
        return Ok((policy, None));
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
    if policy.enforcing() && !config.baseline && policy.limit.is_none() {
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

/// Describes what the baseline measured, for the line that replaces its announcement.
///
/// The test count is omitted rather than guessed when no harness announced one, which is what a
/// target built with `harness = false` does.
fn describe(baseline: &Baseline) -> String {
    let duration = format!("{:.1?}", baseline.wall);
    let ran = baseline.tests.map_or_else(
        || format!("the suite passed in {duration}"),
        |tests| format!("{} ran in {duration}", crate::report::quantity(tests, "test")),
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
mod tests {
    use camino::{Utf8Path, Utf8PathBuf};

    use super::*;
    use crate::discover::Killer;
    use crate::exec::memory::{Demand, MemoryControl};
    use crate::fixtures;
    use crate::model::Mutant;
    use crate::testing::Recorder;

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
            digests: crate::HashMap::default(),
            root: Utf8PathBuf::from("/workspace"),
            files: Vec::new(),
            mutants,
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
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
        assert_eq!(describe(&baseline), "the suite passed in 500.0ms");
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

        assert_eq!(describe(&baseline), "4 tests ran in 500.0ms with a peak of 1.0 MB");
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

    /// The ordinary case is a workspace with no such target at all, which must say nothing.
    #[test]
    fn a_workspace_without_one_is_silent() {
        let mut events = Recorder::default();

        warn_about_compile_fail_targets(&[], &["routerama".to_owned()], &config_with(&[]), &mut events);

        assert!(events.warnings.is_empty(), "{:?}", events.warnings);
    }

    /// A workspace where `app` links `core`, and both are their own reach.
    fn oracle_plan(mutating: &str) -> Plan {
        let mut reach: crate::HashMap<String, crate::HashSet<String>> = crate::HashMap::default();

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
            digests: crate::HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants: vec![pending],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach,
            specs: crate::HashMap::default(),
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
    fn census_targets_only_selected_pending_sites_and_relevant_binaries() {
        let mut plan = oracle_plan("core");
        let mut settled = plan.mutants[0].clone();
        settled.ordinal = 2;
        settled.outcome = Outcome::Killed;
        plan.mutants.push(settled);

        let mut core = oracle_binary("core", Some(1));
        core.path = "/tmp/core".into();
        core.baseline = Duration::from_secs(2);

        let mut app = oracle_binary("app", Some(1));
        app.path = "/tmp/app".into();
        app.baseline = Duration::from_secs(3);

        let mut empty = oracle_binary("app", Some(0));
        empty.path = "/tmp/empty".into();
        empty.baseline = Duration::from_secs(100);

        let packages = [String::from("app")];
        let scope = capped(&packages);
        let binaries = [core, app, empty];
        let reach = Reachability::build(&plan, &binaries, &scope);
        let (targets, savings) = census_targets(&plan, &reach, &Killers::default());

        assert_eq!(targets.len(), 1);
        let selected: HashSet<u32> = core::iter::once(1).collect();
        assert_eq!(targets.get(Utf8Path::new("/tmp/app")), Some(&selected));
        assert_eq!(savings, Duration::from_secs(3));
    }

    /// A hinted mutant sharing a binary with an unhinted one still gets censused there — for free —
    /// but contributes nothing of its own to the census's economics.
    ///
    /// Skipping the hinted mutant's ordinal entirely would leave it out of `targets` for a binary
    /// the census is going to walk anyway, because the unhinted mutant beside it needs it. A
    /// completed census that never recorded that site would then make `Census::selection` read it
    /// as *proven unreached* instead of merely never asked about — turning a stale hint into a wrong
    /// `Uncovered` verdict rather than the whole-binary run a stale hint should fall back to.
    #[test]
    fn a_hinted_mutant_rides_along_on_a_binary_an_unhinted_mutant_already_justifies() {
        let mut plan = oracle_plan("core");
        let mut hinted = crate::testing::advise_fixture::mutant("a.rs", "arith.add_to_sub", Outcome::Pending, 1);
        hinted.package = "core".to_owned().into();
        hinted.ordinal = 2;
        plan.mutants.push(hinted);

        let mut killers = Killers::default();
        killers.record(
            plan.mutants[1].id.clone(),
            Killer {
                package: "core".to_owned(),
                target: String::new(),
                test: "tests::hint".to_owned(),
            },
        );

        let mut core = oracle_binary("core", Some(1));
        core.path = "/tmp/core".into();
        core.baseline = Duration::from_secs(2);

        let binaries = [core];
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let (targets, savings) = census_targets(&plan, &reach, &killers);

        assert_eq!(targets.len(), 1, "one binary is targeted, shared by both mutants");
        let expected: HashSet<u32> = [1, 2].into_iter().collect();
        assert_eq!(
            targets.get(Utf8Path::new("/tmp/core")),
            Some(&expected),
            "the hinted ordinal rides along with the unhinted one"
        );
        assert_eq!(
            savings,
            Duration::from_secs(2),
            "only the unhinted mutant pays the binary's baseline into the census's economics"
        );
    }

    /// A package whose only pending mutant already carries an exact checked killer hint never
    /// justifies a census of its own — the probe the sweep already plans to try is cheaper than
    /// anything a census could narrow it to.
    #[test]
    fn a_mutant_with_a_hint_and_no_unhinted_sibling_never_targets_its_own_binary() {
        let mut plan = oracle_plan("core");
        plan.mutants[0].ordinal = 1;

        let mut killers = Killers::default();
        killers.record(
            plan.mutants[0].id.clone(),
            Killer {
                package: "core".to_owned(),
                target: String::new(),
                test: "tests::hint".to_owned(),
            },
        );

        let mut core = oracle_binary("core", Some(1));
        core.path = "/tmp/core".into();
        core.baseline = Duration::from_secs(2);

        let binaries = [core];
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let (targets, savings) = census_targets(&plan, &reach, &killers);

        assert!(targets.is_empty(), "a hinted mutant alone must not pay to census its own binary");
        assert_eq!(savings, Duration::ZERO, "no census was justified, so it has nothing to be worth");
    }

    /// A hinted mutant whose binary is never independently justified by an unhinted mutant is left
    /// out of `targets` entirely, not merely un-costed — a stale hint there must fall back to a
    /// whole-binary run rather than being read as `Uncovered` by an absent-but-listed census entry.
    #[test]
    fn a_stale_hints_binary_stays_out_of_targets_when_nothing_else_justifies_it() {
        let mut plan = oracle_plan("core");
        plan.mutants[0].ordinal = 1;

        // Given its own explicit reach entry, `extra` no longer falls back to the "reaches
        // everything" default an absent package gets, which keeps it genuinely isolated from
        // `core` on both sides — the property this test is about.
        let _extra_reach = plan
            .reach
            .insert("extra".to_owned(), core::iter::once("extra".to_owned()).collect());

        let mut extra = crate::testing::advise_fixture::mutant("b.rs", "arith.add_to_sub", Outcome::Pending, 2);
        extra.package = "extra".to_owned().into();
        extra.ordinal = 5;
        plan.mutants.push(extra);

        let mut killers = Killers::default();
        killers.record(
            plan.mutants[1].id.clone(),
            Killer {
                package: "extra".to_owned(),
                target: String::new(),
                test: "tests::hint".to_owned(),
            },
        );

        let mut core = oracle_binary("core", Some(1));
        core.path = "/tmp/core".into();
        core.baseline = Duration::from_secs(2);

        let mut extra_binary = oracle_binary("extra", Some(1));
        extra_binary.path = "/tmp/extra".into();
        extra_binary.baseline = Duration::from_secs(4);

        let binaries = [core, extra_binary];
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let (targets, savings) = census_targets(&plan, &reach, &killers);

        assert_eq!(targets.len(), 1, "only the unhinted mutant's own binary is targeted");
        assert!(
            targets.contains_key(Utf8Path::new("/tmp/core")),
            "the unhinted mutant's binary is censused"
        );
        assert!(
            !targets.contains_key(Utf8Path::new("/tmp/extra")),
            "the hinted mutant's own binary, which nothing else justifies, must stay absent"
        );
        assert_eq!(
            savings,
            Duration::from_secs(2),
            "only the unhinted mutant's binary counts toward savings"
        );
    }

    /// A hint that no longer names a binary this mutant can reach — one recorded before a target
    /// was renamed, before the binary moved package, or before the test packages were narrowed away
    /// from it — is exactly as stale as [`super::sweep::judge_ordered`](super::sweep) would find it:
    /// it never matches any binary in `reach.reachable(&mutant.package)`, so this pass has to fall
    /// back to treating the mutant as unhinted rather than silently excusing it from a census.
    #[test]
    fn a_hint_naming_no_reachable_binary_is_treated_as_unhinted() {
        let mut plan = oracle_plan("core");
        plan.mutants[0].ordinal = 1;

        let mut killers = Killers::default();
        killers.record(
            plan.mutants[0].id.clone(),
            Killer {
                package: "core".to_owned(),
                target: "a-target-no-binary-carries".to_owned(),
                test: "tests::hint".to_owned(),
            },
        );

        let mut core = oracle_binary("core", Some(1));
        core.path = "/tmp/core".into();
        core.baseline = Duration::from_secs(2);

        let binaries = [core];
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let (targets, savings) = census_targets(&plan, &reach, &killers);

        let expected: HashSet<u32> = core::iter::once(1).collect();
        assert_eq!(
            targets.get(Utf8Path::new("/tmp/core")),
            Some(&expected),
            "a hint naming no reachable binary must not exempt the mutant from justifying its own census"
        );
        assert_eq!(
            savings,
            Duration::from_secs(2),
            "an ineligible hint pays the baseline into savings exactly like an unhinted mutant"
        );
    }

    /// The crate being mutated has no tests of its own, but the crate above it does.
    ///
    /// Left unsaid, this run reports every mutant as uncovered and reads like a verdict on the code
    /// rather than on the scope the cap chose.
    #[test]
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
