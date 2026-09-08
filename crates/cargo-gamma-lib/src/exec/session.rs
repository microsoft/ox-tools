// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;

use super::build::Round;
use super::test_binary::TestBinary;

/// What happened during a run, beyond the verdicts written back onto the mutants.
#[derive(Debug, Clone)]
pub struct Session {
    /// The sum of the test binaries' baseline durations, used to calibrate mutant budgets.
    pub baseline: Duration,

    /// How long the concurrently measured baseline took on the wall clock.
    pub baseline_wall: Duration,

    /// How many tests the baseline actually ran, or `None` if no harness announced a count.
    ///
    /// This is what ran rather than what exists: `--test-package`, `--include-test` and any filter
    /// passed through to the harness all narrow it. Carried on the session rather than only printed
    /// as the baseline finishes, because progress output resolves to whether a terminal is
    /// attached — so in CI, where "did my suite run at all" is the single most useful thing this
    /// figure answers, the transient line saying it is exactly the one nobody sees.
    pub tests: Option<usize>,

    /// The longest the baseline legitimately went without saying anything.
    pub quiet: Duration,

    /// The silence a mutant was allowed before it was presumed hung, when that was enabled.
    pub stall: Option<Duration>,

    /// How long the single build took.
    pub build: Duration,

    /// The largest peak memory any one test binary reached during the baseline.
    ///
    /// `None` when nothing measured it, which is both the default and what a host without an
    /// aggregate process-tree accounting facility can offer. Reported because it is the figure a
    /// memory ceiling is chosen from, and because a suite whose peak surprises its authors is worth
    /// knowing about whether or not a ceiling is being enforced.
    pub peak: Option<u64>,

    /// Whether this run actually metered memory, which is not always what was configured.
    ///
    /// Memory control is on by default, and a host without cgroup v2 delegation cannot provide it.
    /// A run that defaulted into it and could not have it degrades rather than stopping, so the
    /// configured policy is a request and this is the answer. Everything downstream reads this one,
    /// because asking the platform for accounting it already declined to give would fail every
    /// mutant in the sweep.
    pub metered: bool,

    /// Why memory went unbounded, when it was meant to be bounded and could not be.
    ///
    /// Carried to the end of the run rather than printed when it is discovered, because progress
    /// output is suppressed when nothing is watching it — and a CI runner with no cgroup delegation
    /// is exactly the case where the protection is missing *and* nobody sees the transient line
    /// saying so.
    pub unbounded: Option<String>,

    /// How many mutants were withdrawn because they could not compile.
    pub withdrawn: usize,

    /// Why they were withdrawn, grouped by rustc error code and mutator, densest pair first.
    ///
    /// The count alone says whether unviability is expensive; only this says whether it is
    /// something a mutator could be taught to avoid, or an unavoidable cost of instrumenting the
    /// tree at all. Carried on the session so that asking the question needs no code change.
    pub census: Vec<crate::exec::Withdrawal>,

    /// How many rollback rounds were needed.
    pub rounds: u32,

    /// What each of those rounds cost and withdrew, oldest first.
    ///
    /// Carried alongside `build` because the total on its own cannot tell a build that compiled
    /// first time from one that spent most of its time converging. The two want very different
    /// remedies — a faster machine against fewer unviable mutants — and a run that does not say
    /// which it was leaves that choice to guesswork.
    pub rounds_taken: Vec<Round>,

    /// The test binaries that were run.
    pub binaries: Vec<TestBinary>,

    /// Where the run put everything it kept on disk.
    ///
    /// The path rather than the size, because the size is a walk of a directory holding every build
    /// artifact of every round, and only the diagnostics dump ever prints it. See
    /// [`footprint`](crate::exec::footprint), which turns this into that figure on demand.
    ///
    /// Worth reporting at all because the disk is a real operating cost rather than a curiosity: a
    /// large workspace can leave tens of gigabytes here, which is more than the free space on a
    /// common CI runner, and a job whose next step fails for want of disk deserves to know where the
    /// disk went.
    pub scratch: camino::Utf8PathBuf,

    /// How many test targets `--include-test` or `--exclude-test` kept out of the oracle.
    ///
    /// Zero unless one of those was given. Reported because a narrowed oracle is the single most
    /// consequential thing that can happen to a score without appearing anywhere in it: a survivor
    /// here may be a mutant the excluded target would have caught, and a reader who did not write
    /// the `gamma.toml` has no other way to know the suite was not asked in full.
    pub filtered: usize,

    /// Whether the run had to build test targets it knew it would never consult.
    ///
    /// Building only the packages whose tests can reach a mutant is the cheaper thing to do, but
    /// cargo unifies features over the packages it is asked to build, so a test target that only
    /// compiles because some other package switches a feature on will not compile on its own. When
    /// that happens the selection is abandoned and the whole workspace is built, and the run says
    /// so: the scope the user asked for did not survive contact with their feature graph.
    pub widened: bool,

    /// What the stale build-ordering hints put in front of the compiler, and what came of it.
    ///
    /// A record whose build context no longer matches still knows which mutants failed to compile
    /// for it, and that knowledge is allowed to decide what the compiler sees first — never what
    /// gets built, judged or scored. These are the two facts that say whether it is paying: how
    /// many mutants were front-loaded, and how many of those the compiler then refused. See
    /// [`crate::exec::OrderingHints`] for why there is no "rounds saved" figure here.
    pub ordering: crate::exec::OrderingHints,

    /// What each phase of the run cost, gathered a clock at a time as the phases finish.
    ///
    /// The aggregates above cannot be taken apart after the fact — [`Self::build`] folds the copy,
    /// the preflight and the compile into one number, and whether the per-test census pays for
    /// itself is invisible while its cost hides inside that same figure. Only a clock started at
    /// each phase can say where the time went, so each is timed once, as it runs, and left here for
    /// the diagnostic reporter to surface. Nothing else reads it, and the run behaves identically whether or
    /// not anyone ever does.
    pub phases: Phases,
}

/// What each phase of a run cost, so a fixed or testing total can be read apart into its parts.
///
/// The copy and the preflight are components of [`Session::build`]; the census and the sweep are
/// components of the testing window. Neither set sums to its aggregate — compiling sits between the
/// copy and the baseline, and bookkeeping sits between the census and the sweep — so this is a
/// profile of where a slow run is slow, not a reconciliation of the totals it lives beside.
#[derive(Debug, Clone, Default)]
pub struct Phases {
    /// What duplicating the workspace into the scratch tree cost, before a line of it was
    /// instrumented. Part of [`Session::build`].
    pub copy: Duration,

    /// What the preflight cost: proving the tree compiles at all before any mutant was staged. Part
    /// of [`Session::build`].
    pub preflight: Duration,

    /// What the census cost and covered, or `None` when `--whole-test-binaries` disabled it.
    ///
    /// Absent rather than zero: a run with no census did not spend zero time censusing, it did not
    /// census, and the difference is the whole question of whether turning the census on was worth
    /// it.
    pub census: Option<CensusCost>,

    /// What the sweep cost and how it spent its launches, or `None` when nothing was swept.
    pub sweep: Option<SweepCost>,
}

/// What the per-test census cost and covered.
///
/// The census spends a subprocess per test to learn which tests can reach which sites, and is
/// repaid during the sweep by running fewer tests per mutant. Whether that trade is positive
/// depends on the workspace, and these are the figures that let anyone — including `--estimate` —
/// see which way it went, rather than folding the cost invisibly into the build.
#[derive(Debug, Clone)]
pub struct CensusCost {
    /// How long the whole census took, across every binary and every test.
    pub elapsed: Duration,

    /// How many sample subprocesses the census actually launched: one per test run, not counting
    /// the tests of a binary skipped after it spoiled, nor the per-binary listing runs.
    pub walked: usize,

    /// How many test binaries the census examined.
    pub binaries: usize,
}

/// What the sweep cost and how it spent its subprocess launches.
///
/// The launch count is what turns the cost model's `build + Σ(launch + prefix)` from a formula into
/// a measurement; the probe count is what says whether the killer hints and the census are earning
/// their keep, since a probe is a launch the run only made because a hint pointed at it.
#[derive(Debug, Clone)]
pub struct SweepCost {
    /// How long the sweep took, across every mutant and every binary it ran.
    pub elapsed: Duration,

    /// How many test-binary subprocesses the sweep launched in total.
    pub launches: usize,

    /// How many of those launches were hint-directed probes rather than ordinary binary runs.
    pub probes: usize,
}
