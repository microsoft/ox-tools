// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::num::NonZero;
use core::time::Duration;
use std::thread;

use camino::Utf8PathBuf;

use super::cargo_options::{BuildLimits, CargoOptions};
use super::incremental_mode::IncrementalMode;
use super::memory::MemoryPolicy;

/// Knobs for a run.
#[expect(
    clippy::struct_excessive_bools,
    reason = "an options bag mirroring independent command-line flags, not a state machine"
)]
#[derive(Debug, Clone)]
pub struct Config {
    /// How many mutants to test at once.
    pub jobs: usize,

    /// Multiple of each test binary's baseline duration a mutant is allowed before it is called a timeout.
    ///
    /// The 50% margin is deliberately narrow. It exists to absorb scheduling noise, not to let a
    /// mutant do more work than the original: the whole point of the timeout is that an infinite
    /// loop is a detection, and a generous multiplier turns every such mutant into a long wait
    /// before the same verdict. A confirmation run stands behind it — see `CONFIRM_FACTOR` — so a
    /// budget that is occasionally too tight costs a re-run rather than a wrong verdict, which is
    /// what makes a narrow margin affordable here.
    pub test_timeout_multiplier: f64,

    /// Lower bound on the timeout for any test binary, so that a binary which finishes instantly does not produce a
    /// budget so tight that scheduler noise reads as a hang.
    ///
    /// 20 seconds is chosen against process startup rather than against the suite: a mutant that
    /// has to link, load and start a test binary pays a fixed cost that a fast suite's own elapsed
    /// time says nothing about, and on a cold or loaded machine that cost alone can reach seconds.
    /// The floor only ever binds for suites finishing in under about 17 seconds, where the multiplier
    /// would otherwise yield a budget smaller than the startup it has to cover.
    pub timeout_floor: Duration,

    /// Whether to run the baseline. Skipping it is faster and strictly less trustworthy.
    pub baseline: bool,

    /// Whether a failing test is re-run with no mutant active before the kill is believed.
    ///
    /// Skipping the confirmation saves one run per kill — the cheapest class, since it stops at the
    /// first failing test — and buys a score that counts a flaky test's failures as detections. The
    /// run can no longer tell the two apart, so the flakes disappear into the kill count rather
    /// than being reported.
    pub confirm: bool,

    /// Whether to cut a mutant off as soon as its test binary stops reporting progress.
    pub stall: bool,

    /// Multiple of the longest silence the baseline produced that a mutant is allowed.
    ///
    /// An order of magnitude, and deliberately not a tight one. Unlike the timeout, this is
    /// calibrated against the *quietest* moment of a healthy run, which is the noisiest statistic
    /// the baseline produces: it is a single maximum over one execution, so a healthy suite can
    /// exceed its own measured quiet period on the next run for reasons that have nothing to do
    /// with the mutant. A stall verdict is also strictly less informative than a timeout — it says
    /// only that nothing was printed — so the detector is set to fire on hangs that are obvious by
    /// an order of magnitude, and to leave the marginal cases to the timeout.
    pub stall_factor: f64,

    /// Lower bound on the stall budget, so a suite that never goes quiet does not produce a budget
    /// that scheduler noise can trip.
    ///
    /// Lower than the timeout floor because it bounds silence rather than work: five seconds
    /// without a single line from a running test harness is already anomalous, whereas five
    /// seconds of *elapsed* time is ordinary. It binds for suites whose measured quiet period is
    /// under half a second, which is most fast suites.
    pub stall_floor: Duration,

    /// How cargo and the test binaries are invoked.
    pub cargo: CargoOptions,

    /// How long the build may take.
    pub build: BuildLimits,

    /// How much memory a mutant may use, and whether anything enforces it.
    pub memory: MemoryPolicy,

    /// Keep the scratch tree after the run instead of deleting it.
    pub leak_dirs: bool,

    /// Where to put reusable workspace and build state. `None` uses an isolated cache base
    /// outside the workspace's Cargo-configuration ancestor chain.
    pub cache_dir: Option<Utf8PathBuf>,

    /// Copy files the ignore rules exclude, not only the ones git tracks.
    pub copy_ignored: bool,

    /// Packages whose tests decide a verdict. Empty means each mutant's own package.
    pub test_packages: Vec<String>,

    /// Test target name globs whose tests may decide a verdict. Empty means all of them.
    pub include_tests: Vec<String>,

    /// Test target name globs whose tests must not decide a verdict.
    pub exclude_tests: Vec<String>,

    /// Let tests from every workspace package judge mutants they can reach.
    pub test_workspace: bool,

    /// Whether to run every selected test in a reachable binary instead of selecting cases by reachability.
    ///
    /// False by default: deterministic suites are measured once and each mutant runs only the
    /// specific test cases that can reach it. True is the conservative fallback for suites whose
    /// reachability depends on threads, clocks, randomness or hash iteration order.
    pub whole_test_binaries: bool,

    /// Whether to run test binaries through `cargo nextest`.
    pub nextest: bool,

    /// How an incremental run reuses state from the previous run.
    pub incremental: IncrementalMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jobs: resolve_jobs(None),
            test_timeout_multiplier: 1.5,
            timeout_floor: Duration::from_secs(20),
            baseline: true,
            confirm: true,
            stall: true,
            stall_factor: 10.0,
            stall_floor: Duration::from_secs(5),
            cargo: CargoOptions::default(),
            build: BuildLimits::default(),
            memory: MemoryPolicy::default(),
            leak_dirs: false,
            cache_dir: None,
            copy_ignored: false,
            test_packages: Vec::new(),
            include_tests: Vec::new(),
            exclude_tests: Vec::new(),
            test_workspace: false,
            whole_test_binaries: false,
            nextest: false,
            incremental: IncrementalMode::default(),
        }
    }
}

/// Resolves mutation parallelism, preserving an explicitly requested width.
///
/// Bounded above by what the interrupt registry can watch. Every worker holds one live child, and
/// a child the registry has no slot for is one a cancelled run cannot kill — so a `--jobs` beyond
/// that ceiling would not buy parallelism, it would buy processes that survive `Ctrl-C`. The
/// ceiling is far above any machine's useful width, so clamping to it costs nothing real.
pub(crate) fn resolve_jobs(jobs: Option<usize>) -> usize {
    let wanted = jobs.unwrap_or_else(|| default_jobs(available_parallelism()));

    wanted.min(watchable())
}

/// The parallelism the host makes available to this process.
///
/// This is deliberately separate from [`resolve_jobs`]: the latter adds a worker by default and
/// honours an explicit `--jobs`, neither of which changes the machine's available cores.
pub(crate) fn available_parallelism() -> usize {
    thread::available_parallelism().map_or(1, NonZero::get)
}

/// How many concurrent children the platform can still account for when the run is cancelled.
///
/// On Unix that is the interrupt registry's slot count: containment puts every child in a process
/// group of its own, and a group with no slot is one the handler's sweep never reaches. On Windows
/// each child is held by a job object that kills its subtree when the handle closes however the
/// parent dies, so there is no shared table to run out of and nothing to bound.
#[cfg(unix)]
const fn watchable() -> usize {
    cargo_gamma_process::capacity()
}

#[cfg(not(unix))]
const fn watchable() -> usize {
    usize::MAX
}

const fn default_jobs(available: usize) -> usize {
    available.saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_timeout_never_falls_below_the_floor() {
        let config = Config::default();
        let scaled = Duration::from_millis(1).mul_f64(config.test_timeout_multiplier);

        assert_eq!(scaled.max(config.timeout_floor), config.timeout_floor);
    }

    /// The programmatic default is the 50% margin the command default and the documentation promise.
    #[test]
    fn the_default_timeout_multiplier_is_one_and_a_half() {
        // Bit-exact: the default is an exactly representable value, so this must fail on a wrong
        // default rather than slip under an approximate tolerance — and comparing the bits keeps
        // the pedantic `float_cmp` lint quiet, as the tool does elsewhere by wrapping the value.
        assert_eq!(Config::default().test_timeout_multiplier.to_bits(), 1.5_f64.to_bits());
    }

    #[test]
    fn the_default_timeout_scales_with_a_slow_baseline() {
        let config = Config::default();
        let baseline = Duration::from_mins(1);
        let scaled = baseline.mul_f64(config.test_timeout_multiplier);

        assert_eq!(scaled.max(config.timeout_floor), Duration::from_secs(90));
    }

    #[test]
    fn one_processor_defaults_to_two_jobs() {
        assert_eq!(default_jobs(1), 2);
    }

    #[test]
    fn the_default_job_count_saturates_at_usize_max() {
        assert_eq!(default_jobs(usize::MAX), usize::MAX);
    }

    #[test]
    fn an_explicit_job_count_is_unchanged() {
        assert_eq!(resolve_jobs(Some(0)), 0);
        assert_eq!(resolve_jobs(Some(watchable())), watchable());
    }

    /// A width the interrupt registry could not account for is refused rather than granted.
    ///
    /// The leak it prevents: every worker holds one live child in a process group of its own, and a
    /// group the registry has no slot for is one the interrupt handler's sweep never visits — so
    /// past the ceiling, `Ctrl-C` kills the run and leaves the excess children behind holding the
    /// scratch trees that fail the next run.
    #[test]
    fn a_job_count_beyond_what_can_be_watched_is_clamped() {
        assert_eq!(resolve_jobs(Some(usize::MAX)), watchable());
        assert!(resolve_jobs(Some(watchable().saturating_add(1))) <= watchable());
    }

    /// Whatever the machine's width, the default cannot outrun the registry either.
    #[test]
    fn the_default_job_count_is_also_watchable() {
        assert!(resolve_jobs(None) <= watchable());
    }

    /// Memory control is off until it is asked for.
    #[test]
    fn memory_control_is_enforced_by_default() {
        // On the same footing as the wall-clock timeout: a mutation can turn bounded allocation
        // into unbounded allocation, and the user who most needs protecting from that is the one
        // who never thought to ask. The ceiling is derived from each binary's own baseline peak, so
        // it is a statement about this suite rather than a guess about suites in general.
        let config = Config::default();

        assert!(config.memory.measuring());
        assert!(config.memory.enforcing());
        assert!(config.memory.ceiling(Some(1024 * 1024), true).is_some());

        // Nobody asked for it, so a host that cannot deliver it degrades rather than refusing.
        assert!(!config.memory.insisted());
    }
}
