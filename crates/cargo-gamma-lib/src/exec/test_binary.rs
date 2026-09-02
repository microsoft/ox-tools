// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;

use super::census::{Census, CensusWork};
use super::config::Config;
use super::memory::MemoryPolicy;
use crate::discover::{Glob, Plan};
use crate::estimate::Workload;
use crate::model::{Mutant, Outcome};

/// A test executable and the package that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBinary {
    /// Where the executable is.
    pub path: Utf8PathBuf,

    /// The package it belongs to, which bounds the code it can possibly reach.
    pub package: String,

    /// Cargo's unambiguous identifier for the package.
    pub package_id: String,

    /// The cargo target that produced it, which is what `--exclude-test` matches against.
    ///
    /// A package's unit tests take the name of the lib or bin they live in, and each file under
    /// `tests/` becomes a target of its own, so this is the finest granularity cargo offers for
    /// naming part of a suite — and the granularity `package` is too coarse for.
    pub target: String,

    /// The directory holding the package's `Cargo.toml`, which is where cargo would run it.
    ///
    /// `cargo test` sets the working directory to the package root, not the workspace root, so a
    /// test that opens `tests/data/fixture.json` or `src/../golden.txt` only finds it there. Running
    /// every binary from the workspace root makes those tests fail identically with and without a
    /// mutant active, which turns a whole package's worth of mutants into false survivors.
    pub manifest_dir: Utf8PathBuf,

    /// How long it took with no mutant active, so the cheapest can be tried first.
    ///
    /// Measured under the same concurrency the sweep runs at, so it already carries the contention
    /// every mutant will meet. A figure taken on an idle machine would be a measurement of a
    /// situation that never occurs again during the run, and every budget derived from it would be
    /// too tight by whatever the load costs.
    ///
    /// `Duration::ZERO` when `--no-baseline` left nothing to measure, which is why [`budget`] and
    /// not this field records whether a cutoff was ever calibrated.
    ///
    /// [`budget`]: Self::budget
    pub baseline: Duration,

    /// How many tests the harness announced when it ran with no mutant active.
    ///
    /// `None` means nobody asked or nothing answered — `--no-baseline`, or a `harness = false`
    /// target that announces nothing — which is a different thing from `Some(0)`. Cargo emits a
    /// unit-test binary for every lib target whether or not it holds a single test, so `Some(0)` is
    /// the only evidence a run has that linking this binary to a mutant would prove nothing.
    pub tests: Option<usize>,

    /// This binary's share of a mutant's budget.
    ///
    /// `None` means no cutoff applies, because nothing calibrated one — the same distinction
    /// [`peak`] draws for memory. A budget is derived from the baseline, so a run that took no
    /// baseline has no measurement to derive it from, and a figure invented here would cut every
    /// mutant off at a duration nothing about this suite justified.
    ///
    /// [`peak`]: Self::peak
    pub budget: Option<Duration>,

    /// The peak memory the whole subtree reached with no mutant active, when it was measured.
    ///
    /// `None` means nobody measured it — the run did not ask, or the host could not — which is a
    /// different thing from a peak of zero and is why this is not a plain `u64`.
    pub peak: Option<u64>,

    /// The memory ceiling this binary's mutant runs are held to, when one applies.
    pub memory: Option<u64>,
}

impl TestBinary {
    /// Computes the timeout budget for this binary given an optional per-mutant multiplier override and a floor.
    ///
    /// An override rescales a calibrated budget; it cannot manufacture one, because on a run with
    /// no baseline there is no measurement to multiply.
    #[must_use]
    pub fn budget_for(&self, override_multiplier: Option<f64>, floor: Duration) -> Option<Duration> {
        let budget = self.budget?;

        Some(override_multiplier.map_or(budget, |multiplier| self.baseline.mul_f64(multiplier).max(floor)))
    }
}

/// Derives each test binary's timeout budget from its own measured baseline duration.
///
/// Each test binary is given a budget scaled from its own baseline runtime by the configured
/// multiplier, subject to the configured timeout floor: `max(binary.baseline * multiplier, floor)`.
///
/// The floor answers "below what duration is a verdict meaningless", and that is a statement about
/// measurement noise on one binary. A binary whose baseline is milliseconds gets the floor so that
/// scheduling noise is not misread as a hang.
///
/// `calibrated` says whether the baseline actually ran, and this is the other half of [`bound`]:
/// without a baseline every binary's recorded duration is zero, and scaling zero yields the floor
/// for every binary alike — a flat cutoff invented from no measurement, presented with the
/// confidence of a derived one. Any binary the floor is too small for then times out under every
/// mutant, making the run report resource exhaustion derived from no measurement. No cutoff at all
/// is the honest answer.
pub(super) fn apportion(binaries: &mut [TestBinary], multiplier: f64, floor: Duration, calibrated: bool) {
    for binary in binaries.iter_mut() {
        binary.budget = calibrated.then(|| binary.baseline.mul_f64(multiplier).max(floor));
    }
}

/// Orders the binaries and turns the baseline into the limits every mutant is judged against.
///
/// Cheapest first: the loop stops at the first binary that fails, so trying the quick ones first
/// makes a kill cost less. It changes no verdict, only what a verdict costs.
///
/// Both limits have to be derived after the baseline and before the first mutant runs, and both
/// stand down when there was no baseline to derive them from, so they are settled together.
pub(super) fn calibrate(binaries: &mut [TestBinary], config: &Config, memory: &MemoryPolicy) {
    binaries.sort_by_key(|binary| binary.baseline);

    apportion(binaries, config.test_timeout_multiplier, config.timeout_floor, config.baseline);
    bound(binaries, memory, config.baseline);
}

/// Derives each binary's memory ceiling from what the same binary used with no mutant active.
///
/// This belongs beside [`apportion`] because it is the other half of the same preparation: both
/// turn a baseline measurement into the limit a mutant of that binary is judged against, and both
/// have to happen after the baseline and before the first mutant runs.
///
/// `calibrated` says whether the baseline actually ran; see [`MemoryPolicy::ceiling`] for why a run
/// without one gets no derived ceiling at all.
pub(super) fn bound(binaries: &mut [TestBinary], policy: &MemoryPolicy, calibrated: bool) {
    for binary in binaries.iter_mut() {
        binary.memory = policy.ceiling(binary.peak, calibrated);
    }
}

/// The variable libtest reads to decide how many threads a test harness runs its tests on.
pub(super) const TEST_THREADS_VAR: &str = "RUST_TEST_THREADS";

/// How many threads one spawned test harness should be told to use.
///
/// A test harness defaults to one thread per core, and a run starts `jobs` of them at once, so the
/// machine is asked for `jobs × cores` threads to do `cores` worth of work. What that costs is not
/// merely inefficiency: every mutant's budget was calibrated from a suite that measured itself
/// under the same contention, and contention that varies with how many binaries happen to overlap
/// is noise on every number the run reports. Dividing the cores between the workers makes the load
/// a run places on the machine roughly constant and roughly equal to the machine.
///
/// `inherited` is whatever the environment already said. Someone who set it chose it, and a run
/// that overrode them would silently change the workload they asked for; `None` is returned then,
/// meaning "leave it alone".
pub(super) fn harness_threads(jobs: usize, cores: usize, inherited: Option<&str>) -> Option<usize> {
    if inherited.is_some_and(|value| !value.trim().is_empty()) {
        return None;
    }

    // At least one thread each: more workers than cores is a legitimate thing to ask for, and the
    // answer to it is one thread per worker rather than zero.
    Some((cores / jobs.max(1)).max(1))
}

/// Extracts the test executables cargo reported building.
pub(super) fn test_binaries(stdout: &str) -> Vec<TestBinary> {
    let mut binaries = Vec::new();

    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }

        let is_test = message
            .get("profile")
            .and_then(|profile| profile.get("test"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !is_test {
            continue;
        }

        if let Some(path) = message.get("executable").and_then(Value::as_str) {
            let package_id = message.get("package_id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let package = package_name(&package_id);

            let target = message
                .get("target")
                .and_then(|target| target.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();

            let manifest_dir = message
                .get("manifest_path")
                .and_then(Value::as_str)
                .map(Utf8PathBuf::from)
                .and_then(|manifest| manifest.parent().map(Utf8Path::to_path_buf))
                .unwrap_or_default();

            binaries.push(TestBinary {
                path: Utf8PathBuf::from(path),
                package,
                package_id,
                target,
                manifest_dir,
                baseline: Duration::ZERO,
                budget: None,
                tests: None,
                peak: None,
                memory: None,
            });
        }
    }

    binaries.sort_by(|left, right| left.path.cmp(&right.path));
    binaries.dedup_by(|left, right| left.path == right.path);
    binaries
}

/// Drops the test binaries `--include-test` and `--exclude-test` say must not decide a verdict.
///
/// This has to run before the baseline, not after, so the baseline only runs and measures binaries
/// that will actually decide verdicts.
pub(super) fn restrict(binaries: &mut Vec<TestBinary>, include: &[String], exclude: &[String]) {
    if include.is_empty() && exclude.is_empty() {
        return;
    }

    let patterns = TargetPatterns::new(include, exclude);

    binaries.retain(|binary| patterns.admits(&binary.target));
}

/// Whether a target name survives the include and exclude patterns.
///
/// Exclusion is checked first and wins, which is what makes `--include-test "*"` plus a few
/// exclusions mean what it looks like. An empty include list admits everything, so exclusion alone
/// is a subtraction from the whole suite rather than a selection of nothing.
pub(super) fn admits_target(name: &str, include: &[String], exclude: &[String]) -> bool {
    TargetPatterns::new(include, exclude).admits(name)
}

/// Returns the first `--include-test` or `--exclude-test` pattern that names no test target.
///
/// A pattern matching nothing is the failure these options exist to prevent. An `--exclude-test`
/// typo leaves the target it meant to remove in the oracle, so mutants that should have survived
/// are reported as caught and the score reads better than the suite deserves; an `--include-test`
/// typo empties the oracle instead. Neither says anything on its own, and both look in CI exactly
/// like a run that went well. The same reasoning already makes an unmatched `--file` an error.
pub(super) fn unmatched_test<'args>(tests: &[String], include: &'args [String], exclude: &'args [String]) -> Option<&'args str> {
    TargetPatterns::new(include, exclude).unmatched(tests)
}

struct TargetPatterns<'args> {
    include: Vec<(&'args str, Glob)>,
    exclude: Vec<(&'args str, Glob)>,
}

impl<'args> TargetPatterns<'args> {
    fn new(include: &'args [String], exclude: &'args [String]) -> Self {
        let compile = |patterns: &'args [String]| patterns.iter().map(|pattern| (pattern.as_str(), Glob::new(pattern))).collect();

        Self {
            include: compile(include),
            exclude: compile(exclude),
        }
    }

    fn admits(&self, name: &str) -> bool {
        !self.exclude.iter().any(|(_pattern, compiled)| compiled.matches(name))
            && (self.include.is_empty() || self.include.iter().any(|(_pattern, compiled)| compiled.matches(name)))
    }

    fn unmatched(&self, tests: &[String]) -> Option<&'args str> {
        self.include
            .iter()
            .chain(&self.exclude)
            .find(|(_pattern, compiled)| !tests.iter().any(|name| compiled.matches(name)))
            .map(|(pattern, _compiled)| *pattern)
    }
}

/// Whether a test binary can possibly reach code in `package`.
///
/// By default, a binary only decides verdicts for mutants in its own package. This keeps a
/// workspace run equivalent to running each package separately: a library's score does not depend
/// on whichever reverse-dependent packages happen to share the workspace. Widening is the caller's
/// to ask for, with `--test-package` or `--test-workspace`.
///
/// The alternative, judging a mutant by every package that can link it, makes a crate's score a
/// property of the workspace rather than of the crate: a library scores well because some dependent
/// happens to exercise it, and a refactor over in that dependent silently withdraws the coverage
/// with nothing to report it. It also makes the price of a run a function of the reverse-dependency
/// graph, so mutating one leaf crate compiles and runs most of the workspace.
///
/// Capping costs nothing in honesty, because a mutant no admitted binary can reach is reported as
/// uncovered rather than as surviving — the run says "nothing tests this", which is the truth, and
/// not "your tests missed this", which would not be.
///
/// Within that cap, reach is a pure optimization: a binary is not run against a mutant it cannot
/// link. An unknown package on either side means "assume it can", since a missed optimization costs
/// time while a wrong exclusion would report an untested mutant as unreachable and hide a real gap.
///
/// A binary that announced no tests at all is the one exclusion that is safe to make from the
/// evidence a run already has. Cargo emits a unit-test binary for every lib target whether or not
/// the target holds a single test, so a package with no tests still produces a binary, and a
/// binary that exists is enough to make every mutant in that package "reachable" — which reports a
/// package nobody tests as full of survivors rather than as uncovered. Those are materially
/// different findings: one says the tests are weak, the other says there are none. A harness that
/// announced zero tests can convict nothing, so it makes nothing reachable.
pub(super) fn reaches(binary: &TestBinary, package: &str, plan: &Plan, scope: &TestScope<'_>) -> bool {
    // `None` is "nobody counted" — no baseline, or a custom harness that announces nothing — and
    // must not be read as zero.
    if binary.tests == Some(0) {
        return false;
    }

    if !scope.admits(&binary.package) {
        return false;
    }

    if binary.package.is_empty() || package.is_empty() {
        return true;
    }

    if scope.package_local && binary.package != package {
        return false;
    }

    plan.reach.get(&binary.package).is_none_or(|reachable| reachable.contains(package))
}

/// Orders an unhinted mutant's reachable binaries without changing which binaries are reachable.
///
/// Tests from the package that owns the mutant are normally the closest oracle. Within the own
/// package and remaining-package tiers, measured baseline cost comes first, followed by stable
/// identity fields so Cargo's artifact order cannot make two equivalent runs diverge.
///
/// Exact per-mutant and learned file-local killers are applied later by the verdict path and
/// therefore still take precedence over this cold-run order; so does [`Census`]'s own
/// current-cost order, which further reorders this tier's tail once a census is available.
pub(super) fn order_reachable(binaries: &mut [&TestBinary], mutant_package: &str) {
    binaries.sort_by(|left, right| {
        let tier = |binary: &TestBinary| u8::from(binary.package != mutant_package);

        tier(left)
            .cmp(&tier(right))
            .then_with(|| left.baseline.cmp(&right.baseline))
            .then_with(|| left.package.cmp(&right.package))
            .then_with(|| left.target.cmp(&right.target))
            .then_with(|| left.path.cmp(&right.path))
    });
}

/// Which binaries can reach each package holding a pending mutant, indexed once.
///
/// Census economics, workload projection, and sweep scheduling each need this exact relationship,
/// and previously each derived it independently: three passes over the same pending mutants and
/// binaries, with three different owned key and value representations discarded in succession.
/// This is built once, after the binaries and the baseline are known, and lent to all three.
///
/// Package reachability is a coarser fact than a census: it says a binary is *permitted* to be
/// consulted for a mutant's package, never that a specific test or a specific mutation site is
/// covered. Nothing here may ever be read as evidence that a site or a test is uncovered — only
/// [`Census`], and only when its own census for that binary completed, settles that.
#[derive(Debug, Default)]
pub(super) struct Reachability<'binaries> {
    by_package: crate::HashMap<Arc<str>, Vec<&'binaries TestBinary>>,
}

impl<'binaries> Reachability<'binaries> {
    /// Indexes every package holding a pending, ordinal-positive mutant to the binaries that can
    /// reach it, in [`order_reachable`]'s cold-run order.
    ///
    /// Distinct packages are far fewer than mutants, so the reachable set is worked out once per
    /// package rather than once per mutant.
    pub(super) fn build(plan: &Plan, binaries: &'binaries [TestBinary], scope: &TestScope<'_>) -> Self {
        let mut by_package: crate::HashMap<Arc<str>, Vec<&TestBinary>> = crate::HashMap::default();

        for mutant in plan
            .mutants
            .iter()
            .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        {
            if by_package.contains_key(&mutant.package) {
                continue;
            }

            let mut found: Vec<&TestBinary> = binaries
                .iter()
                .filter(|binary| reaches(binary, &mutant.package, plan, scope))
                .collect();

            order_reachable(&mut found, &mutant.package);

            let _fresh = by_package.insert(Arc::clone(&mutant.package), found);
        }

        Self { by_package }
    }

    /// The binaries that can reach `package`'s mutants, or `None` when this index was never asked
    /// to cover that package — every package holding a pending mutant this run judges was, so
    /// `None` here means the caller asked about a package with no pending work.
    pub(super) fn reachable(&self, package: &str) -> Option<&[&'binaries TestBinary]> {
        self.by_package.get(package).map(Vec::as_slice)
    }

    /// How many distinct packages this index covers.
    ///
    /// Only the tests ask, to confirm the index memoizes by package rather than by mutant.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.by_package.len()
    }
}

/// Which packages' tests are allowed to decide verdicts, before the preflight has its say.
///
/// `--test-package` names the oracle outright. Failing that, `--test-workspace` lifts the cap
/// altogether. Failing both, the returned packages are the union whose own tests may judge their
/// own mutants. [`TestScope::package_local`] keeps that union package-local per verdict.
///
/// Taken from the run's package selection rather than from the packages that turned out to hold
/// mutants, because `--file` and `--in-diff` narrow the population without narrowing what the
/// caller asked cargo for. Deriving it from the population instead would mean that mutating one
/// file in a package withdrew the rest of that package's own test binaries from the oracle, turning
/// kills into survivors for no reason the caller could see.
pub(super) fn oracle_packages(selected: &[String], config: &Config) -> Vec<String> {
    if !config.test_packages.is_empty() || config.test_workspace {
        return config.test_packages.clone();
    }

    selected.to_vec()
}

/// Which packages need their test targets compiled at all.
///
/// A test binary is only ever run against a mutant its package can reach, so a package that cannot
/// reach anything being mutated produces binaries the run would build, baseline and then never
/// consult. Naming the useful subset lets cargo skip compiling the rest.
///
/// Returns `None` when the subset is the whole workspace, which is both the common case and the
/// one worth spelling as `--workspace`: cargo unifies features over the packages it is asked to
/// build, so narrowing the selection is a change in what gets compiled and not only in how much.
/// The caller is expected to fall back to the whole workspace if a narrowed build fails.
pub(super) fn build_packages(plan: &Plan, scope: &TestScope<'_>) -> Option<Vec<String>> {
    let mutated: crate::HashSet<&str> = plan
        .mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .map(|mutant| &*mutant.package)
        .collect();

    reaching_packages(&plan.reach, &mutated, scope)
}

/// The selection rule behind [`build_packages`], over an explicit set of mutated packages.
///
/// The preflight check needs the same subset before anything has been scanned, when the mutated set
/// can only be named as "every package this run intends to mutate". That is a superset of the
/// packages that will turn out to hold live mutants, which is the direction the preflight needs to
/// err in: checking more than the later builds compile is wasted work, while checking less would
/// leave a target uncleared and let a genuine error in it be absorbed as though a mutant had caused
/// it.
pub(super) fn reaching_packages(
    reach: &crate::HashMap<String, crate::HashSet<String>>,
    mutated: &crate::HashSet<&str>,
    scope: &TestScope<'_>,
) -> Option<Vec<String>> {
    // Reach is keyed by every workspace member, so its keys are the population being narrowed from.
    // Without it there is nothing to compare a subset against, so there is no subset.
    if reach.is_empty() {
        return None;
    }

    let mut wanted: Vec<String> = reach
        .iter()
        .filter(|(package, reachable)| {
            if !scope.admits(package) {
                // A package whose own code is being mutated still has to compile its test targets:
                // a mutant can live in one of them.
                return mutated.contains(package.as_str());
            }

            if scope.package_local {
                return mutated.contains(package.as_str());
            }

            scope.whole_workspace || reachable.iter().any(|name| mutated.contains(name.as_str()))
        })
        .map(|(package, _)| package.clone())
        .collect();

    if wanted.len() >= reach.len() {
        return None;
    }

    // Deterministic, because it becomes a command line that is worth being able to compare between
    // runs.
    wanted.sort();

    Some(wanted)
}

/// Totals the work every live mutant represents, counting only the binaries that can reach it.
///
/// Returns the serial suite time and the serial budget: what testing each live mutant once would
/// cost if every reachable binary ran to completion, and what it would cost if every reachable
/// binary instead ran out its timeout. Both are summed per mutant rather than taken from the whole
/// suite, because that is what a run actually does — a mutant in a leaf crate never starts the
/// binaries that cannot link it.
pub(super) fn workload(mutants: &[Mutant], reach: &Reachability<'_>, census: Option<&Census>) -> Workload {
    let mut total = Workload::default();

    // Its cost remains per-mutant when a census is present because each site has its own measured
    // set of tests; only the reachable set behind it is shared across every mutant of a package.
    for mutant in mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
    {
        let reachable = reach
            .reachable(&mutant.package)
            .expect("the shared reachability index was built from these same pending mutants");

        let mut suite = Duration::ZERO;
        let mut worst = Duration::ZERO;
        let mut running = 0_usize;

        for binary in reachable {
            match census.map_or(CensusWork::Whole, |census| census.work(binary, mutant.ordinal)) {
                CensusWork::Whole => suite += binary.baseline,
                CensusWork::Uncovered => continue,
                CensusWork::Selected(duration) => suite += duration,
                CensusWork::Hinted(_duration) => suite += binary.baseline,
            }

            worst += binary.budget.unwrap_or_default();
            running = running.saturating_add(1);
        }

        // A mutant that hangs hangs in one binary and is judged there, so what one costs is a
        // single binary's budget rather than every binary's. Which one it will be is unknown,
        // so the average stands in for it.
        let single = u32::try_from(running).map_or(worst, |count| worst.checked_div(count).unwrap_or(worst));

        total.suite += suite;
        total.budget += worst;
        total.single += single;
    }

    total
}

/// Which test binaries a run is allowed to consult.
#[derive(Debug, Clone, Copy)]
pub(super) struct TestScope<'names> {
    /// Packages named by `--test-package`. Empty means no restriction.
    pub(super) packages: &'names [String],

    /// Whether each mutant is judged only by tests from its own package.
    pub(super) package_local: bool,

    /// Whether `--test-workspace` lifted the cap, so every package's tests may decide verdicts.
    ///
    /// Reachability still applies: lifting the cap widens which packages may convict, not which
    /// code a binary can link.
    pub(super) whole_workspace: bool,
}

impl TestScope<'_> {
    /// Returns whether a binary's package survives the `--test-package` filter.
    pub(super) fn admits(&self, package: &str) -> bool {
        self.packages.is_empty() || self.packages.iter().any(|wanted| wanted == package)
    }
}

/// Extracts the package name from a cargo package id.
///
/// Two spellings are in circulation: the stable `path+file:///x/y#name@1.0.0` (name omitted when it
/// matches the last path segment) and the older `name 1.0.0 (source)`. An unrecognized id yields an
/// empty name, treated as "reaches everything" rather than "reaches nothing".
fn package_name(id: &str) -> String {
    if let Some((locator, fragment)) = id.rsplit_once('#') {
        return fragment.split_once('@').map_or_else(
            || {
                if is_version(fragment) {
                    // A bare version: the name is the last segment of the path before it.
                    locator.rsplit('/').next().unwrap_or_default().to_owned()
                } else {
                    fragment.to_owned()
                }
            },
            |(name, _version)| name.to_owned(),
        );
    }

    id.split_whitespace().next().unwrap_or_default().to_owned()
}

/// Whether a package id fragment is a bare version rather than a package name.
///
/// The release part of a version is digits and dots and nothing else, which no package name can be
/// mistaken for; anything after the first `-` or `+` is a pre-release or build tag and is ignored,
/// since those are made of the same letters a name is.
fn is_version(fragment: &str) -> bool {
    let release = fragment.split(['-', '+']).next().unwrap_or(fragment);

    !release.is_empty()
        && release.starts_with(|character: char| character.is_ascii_digit())
        && release.chars().all(|character| character.is_ascii_digit() || character == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope that admits every binary, which is what most of these tests are not about.
    const ANY: TestScope<'static> = TestScope {
        packages: &[],
        package_local: false,
        whole_workspace: false,
    };

    #[test]
    fn test_binaries_are_read_from_cargo_json() {
        // Only test artifacts are runnable; the library artifact in the same stream is not.
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","profile":{"test":false},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/unit"}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error"}}"#,
            "\n",
            "not json at all",
            "\n",
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/cli"}"#,
            "\n",
        );

        // The list is sorted so that a run's per-binary ordering does not depend on cargo's
        // scheduling, which would make timings and any early exit irreproducible.
        let paths: Vec<Utf8PathBuf> = test_binaries(stdout).into_iter().map(|binary| binary.path).collect();

        assert_eq!(paths, vec![Utf8PathBuf::from("/tmp/cli"), Utf8PathBuf::from("/tmp/unit")]);
    }

    /// A test artifact cargo reports with no executable string contributes nothing to the list.
    ///
    /// `cargo` emits a `compiler-artifact` message for a test target before it has finished linking
    /// it, and that message carries a `null` executable; treating it as a binary would hand the run
    /// a path that does not exist yet, and every attempt to run it would be misread as a crash
    /// rather than as the harmless intermediate message it actually is.
    #[test]
    fn an_artifact_with_no_executable_string_contributes_no_binary() {
        let stdout = r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":null}"#;

        assert!(test_binaries(stdout).is_empty());
    }

    /// Nothing to apportion is not an error.
    #[test]
    fn apportioning_across_no_binaries_does_nothing() {
        let mut binaries: Vec<TestBinary> = Vec::new();

        apportion(&mut binaries, 1.2, Duration::from_secs(1), true);

        assert!(binaries.is_empty());
    }

    /// A binary the graph says nothing about is admitted, because a missed optimization is cheap
    /// and a wrong exclusion would hide a real gap.
    #[test]
    fn a_binary_the_graph_says_nothing_about_is_admitted() {
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };

        assert!(reaches(&binary("unrelated"), "subject", &plan_reaching(&[]), &scope));
    }

    /// `--test-workspace` lifts the `--test-package` cap; it does not make a binary link code it
    /// cannot link.
    ///
    /// The cap's honesty rests on a mutant no admitted binary reaches being reported as uncovered
    /// rather than as surviving — "nothing tests this" rather than "your tests missed this". A
    /// whole-workspace scope that ignored the graph would run a binary against a mutant it cannot
    /// link, watch it pass, and record a survivor on that evidence.
    #[test]
    fn a_whole_workspace_scope_still_does_not_reach_what_a_binary_cannot_link() {
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let plan = plan_reaching(&[("harness", &["harness"])]);

        assert!(!reaches(&binary("harness"), "subject", &plan, &scope));
        assert!(reaches(&binary("harness"), "harness", &plan, &scope));
    }

    /// A binary whose harness announced no tests reaches nothing, whatever the build graph says.
    ///
    /// Regression, issue-011. Cargo emits a unit-test binary for every lib target whether or not it
    /// holds a test, so linkage alone always finds a binary and the uncovered bucket was
    /// unreachable. A harness that announced zero tests can convict nothing, so it must not be what
    /// makes a mutant look tested.
    #[test]
    fn a_binary_that_announced_no_tests_reaches_nothing() {
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let empty = TestBinary {
            tests: Some(0),
            ..binary("subject")
        };

        assert!(!reaches(&empty, "subject", &plan_reaching(&[]), &scope));
    }

    /// A binary nobody counted the tests of still reaches what it links.
    ///
    /// `None` is `--no-baseline`, or a `harness = false` target that announces nothing. Reading it
    /// as zero would report a whole run as uncovered on the strength of a measurement never taken.
    #[test]
    fn a_binary_whose_tests_were_never_counted_still_reaches_what_it_links() {
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let uncounted = TestBinary {
            tests: None,
            ..binary("subject")
        };

        assert!(reaches(&uncounted, "subject", &plan_reaching(&[]), &scope));
    }

    /// A binary that announced tests reaches what it links, as it always did.
    #[test]
    fn a_binary_that_announced_tests_reaches_what_it_links() {
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let populated = TestBinary {
            tests: Some(7),
            ..binary("subject")
        };

        assert!(reaches(&populated, "subject", &plan_reaching(&[]), &scope));
    }

    /// The harness width and the worker count multiply out to roughly the machine.
    ///
    /// Regression, issue-016. A harness defaults to one thread per core and a run starts `jobs` of
    /// them, so the machine was asked for `jobs × cores` threads to do `cores` of work — and every
    /// budget in the run was calibrated under a load nothing else ever reproduces.
    #[test]
    fn the_harness_width_divides_the_machine_between_the_workers() {
        assert_eq!(harness_threads(4, 16, None), Some(4));
        assert_eq!(harness_threads(1, 16, None), Some(16));
        assert_eq!(harness_threads(16, 16, None), Some(1));
    }

    /// More workers than cores still gets a thread each, rather than none.
    #[test]
    fn more_workers_than_cores_still_get_a_thread_each() {
        assert_eq!(harness_threads(64, 8, None), Some(1));

        // A run cannot have no workers, but the arithmetic must not divide by zero if it did.
        assert_eq!(harness_threads(0, 8, None), Some(8));
    }

    /// A width the caller already chose is left exactly as they chose it.
    ///
    /// Overriding it would silently change the workload someone asked for, and the run would be
    /// measuring and judging something other than what they meant to test.
    #[test]
    fn a_harness_width_the_caller_chose_is_left_alone() {
        assert_eq!(harness_threads(4, 16, Some("2")), None);

        // An empty setting is not a choice; it is what an unset variable looks like once it has
        // been through a shell.
        assert_eq!(harness_threads(4, 16, Some("  ")), Some(4));
    }

    #[test]
    fn a_binary_carries_the_directory_of_its_packages_manifest() {
        let line = r#"{"reason":"compiler-artifact","package_id":"path+file:///w/crates/subject#0.1.0","manifest_path":"/w/crates/subject/Cargo.toml","profile":{"test":true},"executable":"/w/target/debug/deps/subject-abc"}"#;
        let found = test_binaries(line);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest_dir, "/w/crates/subject");
    }

    #[test]
    fn a_binary_cargo_did_not_locate_has_no_manifest_directory() {
        let line = r#"{"reason":"compiler-artifact","package_id":"path+file:///w#0.1.0","profile":{"test":true},"executable":"/w/target/debug/deps/subject-abc"}"#;
        let found = test_binaries(line);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].manifest_dir, "");
    }

    fn binary(package: &str) -> TestBinary {
        TestBinary {
            package: package.to_owned(),
            ..crate::testing::test_binary("/tmp/t")
        }
    }

    /// The path has to differ, since `restrict` runs on a list `test_binaries` already deduplicated.
    fn target(name: &str) -> TestBinary {
        TestBinary {
            target: name.to_owned(),
            ..crate::testing::test_binary(&format!("/tmp/{name}"))
        }
    }

    fn names(binaries: &[TestBinary]) -> Vec<&str> {
        binaries.iter().map(|binary| binary.target.as_str()).collect()
    }

    #[test]
    fn the_target_name_is_kept_from_cargo_json() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","profile":{"test":true},"target":{"name":"conformance_xsd"},"executable":"/tmp/c"}"#,
            "\n"
        );

        let found = test_binaries(stdout);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, "conformance_xsd");
    }

    /// Cargo has always reported this, but an older or stubbed stream must not lose the binary.
    #[test]
    fn a_binary_with_no_target_name_is_still_kept() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/c"}"#,
            "\n"
        );

        let found = test_binaries(stdout);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, "");
    }

    #[test]
    fn no_patterns_leave_every_binary_in_place() {
        let mut binaries = vec![target("unit"), target("conformance_xsd")];

        restrict(&mut binaries, &[], &[]);

        assert_eq!(names(&binaries), vec!["unit", "conformance_xsd"]);
    }

    #[test]
    fn an_exclusion_glob_removes_the_targets_it_names() {
        let mut binaries = vec![target("unit"), target("conformance_xsd"), target("conformance_xpath")];

        restrict(&mut binaries, &[], &["conformance_*".to_owned()]);

        assert_eq!(names(&binaries), vec!["unit"]);
    }

    #[test]
    fn an_inclusion_glob_keeps_only_the_targets_it_names() {
        let mut binaries = vec![target("unit"), target("integration"), target("conformance_xsd")];

        restrict(&mut binaries, &["unit".to_owned(), "integration".to_owned()], &[]);

        assert_eq!(names(&binaries), vec!["unit", "integration"]);
    }

    /// Otherwise `--include-test "*"` with a few exclusions would quietly mean the whole suite.
    #[test]
    fn an_exclusion_beats_an_inclusion_that_also_matches() {
        let mut binaries = vec![target("unit"), target("conformance_xsd")];

        restrict(&mut binaries, &["*".to_owned()], &["conformance_*".to_owned()]);

        assert_eq!(names(&binaries), vec!["unit"]);
    }

    #[test]
    fn a_pattern_matching_a_declared_target_is_not_reported_as_unmatched() {
        let tests = vec!["unit".to_owned(), "conformance_xsd".to_owned()];

        assert_eq!(unmatched_test(&tests, &[], &["conformance_*".to_owned()]), None);
    }

    #[test]
    fn a_pattern_matching_nothing_is_reported() {
        let tests = vec!["unit".to_owned(), "conformance_xsd".to_owned()];
        let exclude = vec!["conformance_*".to_owned(), "confrmance_xpath".to_owned()];

        assert_eq!(unmatched_test(&tests, &[], &exclude), Some("confrmance_xpath"));
    }

    /// An inclusion typo empties the oracle instead of widening it, and is just as fatal.
    #[test]
    fn an_unmatched_inclusion_is_reported_too() {
        let tests = vec!["unit".to_owned()];

        assert_eq!(unmatched_test(&tests, &["untis".to_owned()], &[]), Some("untis"));
    }

    fn plan_reaching(edges: &[(&str, &[&str])]) -> Plan {
        let mut reach = crate::HashMap::default();

        for (from, to) in edges {
            let _previous = reach.insert((*from).to_owned(), to.iter().map(|name| (*name).to_owned()).collect());
        }

        Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach,
            specs: crate::HashMap::default(),
        }
    }

    /// A plan whose live mutants sit in `packages`, over the dependency graph `edges` describes.
    fn plan_mutating(edges: &[(&str, &[&str])], packages: &[&str]) -> Plan {
        let mut plan = plan_reaching(edges);

        plan.mutants = packages
            .iter()
            .enumerate()
            .map(|(index, package)| Mutant {
                id: (*package).to_owned().into(),
                ordinal: u32::try_from(index).unwrap_or(0).saturating_add(1),
                file: (Utf8PathBuf::from("src/lib.rs")).into(),
                package: ((*package).to_owned()).into(),
                span: 0..1,
                line: 1,
                end_line: 1,
                column: 1,
                mutator: ("arith.add_to_sub".to_owned()).into(),
                item_path: ("f".to_owned()).into(),
                trait_impl: None,
                occurrence: 0,
                replacement_index: 0,
                original: "a + b".to_owned().into(),
                replacement: "a - b".to_owned().into(),
                shape: crate::ops::collect::Shape::Expr,
                outcome: Outcome::Pending,
                suppression: None,
                expectation: None,
                test_timeout_multiplier: None,
                elapsed_ms: 0,
                killed_by: None,
                note: None,
            })
            .collect();

        plan
    }

    /// `app` and `tool` both link `core`; `aside` links nothing.
    fn graph() -> Vec<(&'static str, &'static [&'static str])> {
        vec![
            ("app", &["app", "core"] as &[&str]),
            ("tool", &["tool", "core"]),
            ("core", &["core"]),
            ("aside", &["aside"]),
        ]
    }

    /// A package is a correlated workload: every one of its mutants reaches the exact same
    /// binaries in the exact same order, so working the answer out once per package is not an
    /// optimization that can ever disagree with working it out once per mutant.
    #[test]
    fn reachability_memoizes_by_package_rather_than_by_mutant() {
        let plan = plan_mutating(&graph(), &["core", "core", "app"]);
        let binaries = vec![binary("app"), binary("core"), binary("tool")];

        let reach = Reachability::build(&plan, &binaries, &ANY);

        assert_eq!(reach.len(), 2, "one entry per distinct package, not one per mutant");
    }

    /// A package holding no pending mutant was never asked about, and stays absent rather than
    /// mapping to an empty answer — the two mean different things to a caller.
    #[test]
    fn reachability_answers_none_for_a_package_holding_no_pending_mutant() {
        let plan = plan_mutating(&graph(), &["core"]);
        let binaries = vec![binary("app"), binary("core"), binary("tool")];

        let reach = Reachability::build(&plan, &binaries, &ANY);

        assert!(
            reach.reachable("aside").is_none(),
            "aside holds no pending mutant, so nothing should have asked about it"
        );
    }

    /// The shared index applies [`order_reachable`]'s own-package-first, baseline-then-identity
    /// cold-run order while building each package's answer, rather than handing back an arbitrary
    /// one its caller would have to re-sort.
    #[test]
    fn reachability_orders_each_packages_binaries_with_order_reachable() {
        let plan = plan_mutating(&graph(), &["core"]);
        let mut app_dependent = binary("app");
        app_dependent.baseline = Duration::from_millis(1);
        let mut own = binary("core");
        own.baseline = Duration::from_secs(1);
        let binaries = vec![app_dependent, own];

        let reach = Reachability::build(&plan, &binaries, &ANY);
        let ordered = reach.reachable("core").expect("core holds a pending mutant");

        assert_eq!(
            ordered.iter().map(|binary| binary.package.as_str()).collect::<Vec<_>>(),
            ["core", "app"],
            "own package first, exactly as order_reachable orders it directly"
        );
    }

    /// With no census, every reachable binary is costed at its whole baseline, and a mutant's
    /// share of the run-out-of-time budget averages that binary set's own budgets.
    #[test]
    fn workload_sums_reachable_binary_baselines_with_no_census() {
        let plan = plan_mutating(&graph(), &["core"]);
        let mut app_dependent = binary("app");
        app_dependent.baseline = Duration::from_millis(300);
        app_dependent.budget = Some(Duration::from_secs(3));
        let mut own = binary("core");
        own.baseline = Duration::from_millis(700);
        own.budget = Some(Duration::from_secs(7));
        let binaries = vec![app_dependent, own];

        let reach = Reachability::build(&plan, &binaries, &ANY);
        let work = workload(&plan.mutants, &reach, None);

        assert_eq!(
            work.suite,
            Duration::from_secs(1),
            "one mutant, whole-binary cost for both of its reachable binaries"
        );
        assert_eq!(work.budget, Duration::from_secs(10), "both binaries' budgets, summed");
        assert_eq!(work.single, Duration::from_secs(5), "the average of the two binaries' budgets");
    }

    /// `workload` shares [`test_all`]'s exact invariant: the `Reachability` it is given must have
    /// been built from the very same pending mutants it iterates, so every pending mutant's package
    /// is guaranteed present. A package silently missing used to be skipped, which would silently
    /// under-count the workload instead of surfacing the mismatch that caused it.
    #[test]
    #[should_panic(expected = "the shared reachability index was built from these same pending mutants")]
    fn workload_refuses_a_reachability_missing_a_pending_mutants_package() {
        let plan = plan_mutating(&graph(), &["core"]);
        let binaries = vec![binary("core")];

        // Built from a plan that never mutates `core`, so the index has no entry for it — exactly
        // the mismatch `test_all` itself guards against with the same `.expect(...)`.
        let stale_plan = plan_mutating(&graph(), &["aside"]);
        let reach = Reachability::build(&stale_plan, &binaries, &ANY);

        let _work = workload(&plan.mutants, &reach, None);
    }

    #[test]
    fn a_package_whose_tests_can_reach_nothing_being_mutated_is_not_built() {
        let plan = plan_mutating(&graph(), &["core"]);

        // `aside` cannot link `core`, so every test binary it would produce is one the run would
        // compile, baseline and never consult.
        assert_eq!(
            build_packages(&plan, &ANY),
            Some(vec!["app".to_owned(), "core".to_owned(), "tool".to_owned()])
        );
    }

    #[test]
    fn mutating_everything_asks_for_the_whole_workspace() {
        let plan = plan_mutating(&graph(), &["core", "aside"]);

        assert_eq!(build_packages(&plan, &ANY), None, "a subset of everything is not a subset");
    }

    #[test]
    fn naming_the_tests_that_matter_narrows_the_build_to_them() {
        let plan = plan_mutating(&graph(), &["core"]);
        let named = [String::from("app")];
        let scope = TestScope {
            packages: &named,
            package_local: false,
            whole_workspace: false,
        };

        // Only `app`'s tests can return a verdict, so only `app`'s tests are worth compiling — and
        // `core` comes with them because a mutant can live in one of its own test targets.
        assert_eq!(build_packages(&plan, &scope), Some(vec!["app".to_owned(), "core".to_owned()]));
    }

    #[test]
    fn package_local_testing_builds_only_the_mutated_packages_own_tests() {
        let plan = plan_mutating(&graph(), &["core"]);
        let selected = [String::from("app"), String::from("core"), String::from("tool")];
        let scope = TestScope {
            packages: &selected,
            package_local: true,
            whole_workspace: false,
        };

        assert_eq!(build_packages(&plan, &scope), Some(vec!["core".to_owned()]));
    }

    #[test]
    fn testing_the_whole_workspace_builds_the_whole_workspace() {
        let plan = plan_mutating(&graph(), &["core"]);
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };

        assert_eq!(build_packages(&plan, &scope), None);
    }

    #[test]
    fn a_workspace_with_no_dependency_graph_is_built_whole() {
        let plan = plan_mutating(&[], &["core"]);

        assert_eq!(build_packages(&plan, &ANY), None, "nothing is known, so nothing can be ruled out");
    }

    /// A bare run consults the tests cargo would have run here, and no others.
    ///
    /// This is the cap the whole oracle rests on: `cargo gamma` inside a crate costs a multiple of
    /// `cargo test` inside that crate, rather than a multiple of everything that links it.
    #[test]
    fn the_oracle_defaults_to_the_packages_cargo_itself_would_have_tested() {
        let selected = [String::from("tick")];

        assert_eq!(oracle_packages(&selected, &Config::default()), vec!["tick".to_owned()]);
    }

    /// `--test-package` is the caller naming the oracle outright, so the default has nothing to add.
    #[test]
    fn naming_the_test_packages_replaces_the_default_cap() {
        let selected = [String::from("tick")];
        let config = Config {
            test_packages: vec!["harness".to_owned()],
            ..Config::default()
        };

        assert_eq!(oracle_packages(&selected, &config), vec!["harness".to_owned()]);
    }

    /// `--test-workspace` is the way to ask for the reach-everything oracle, so it lifts the cap.
    #[test]
    fn testing_the_whole_workspace_lifts_the_cap_entirely() {
        let selected = [String::from("tick")];
        let config = Config {
            test_workspace: true,
            ..Config::default()
        };

        assert!(
            oracle_packages(&selected, &config).is_empty(),
            "an empty restriction is what admits every package"
        );
    }

    /// The cap follows the package selection, not the mutants that selection turned out to produce.
    ///
    /// `--file` and `--in-diff` narrow the population without narrowing what cargo was asked for.
    /// Reading the cap off the mutated packages would mean that touching one file withdrew the rest
    /// of that package's own binaries from the oracle and reported their kills as survivors.
    #[test]
    fn the_cap_covers_every_selected_package_even_when_only_one_holds_mutants() {
        let selected = [String::from("tick"), String::from("tock")];

        assert_eq!(
            oracle_packages(&selected, &Config::default()),
            vec!["tick".to_owned(), "tock".to_owned()]
        );
    }

    /// A dependent's tests do not judge a mutant in the crate the run was pointed at.
    ///
    /// The reachability graph says `app` links `core` and so *could* convict, and before the cap it
    /// did. Running in `core` now asks only what `core`'s own suite thinks, which is what makes the
    /// score a property of the crate rather than of the workspace around it.
    #[test]
    fn a_dependents_tests_do_not_judge_a_mutant_in_the_crate_being_run_on() {
        let plan = plan_reaching(&[("app", &["app", "core"]), ("core", &["core"])]);
        let capped = [String::from("core")];
        let scope = TestScope {
            packages: &capped,
            package_local: false,
            whole_workspace: false,
        };

        assert!(!reaches(&binary("app"), "core", &plan, &scope), "the oracle escaped its cap");
        assert!(reaches(&binary("core"), "core", &plan, &scope), "the crate cannot judge itself");
    }

    #[test]
    fn a_workspace_run_judges_each_mutant_only_with_its_own_package() {
        let plan = plan_reaching(&[("app", &["app", "core"]), ("core", &["core"])]);
        let selected = [String::from("app"), String::from("core")];
        let scope = TestScope {
            packages: &selected,
            package_local: true,
            whole_workspace: false,
        };

        assert!(!reaches(&binary("app"), "core", &plan, &scope));
        assert!(reaches(&binary("core"), "core", &plan, &scope));
        assert!(reaches(&binary("app"), "app", &plan, &scope));
    }

    #[test]
    fn a_binary_only_reaches_what_its_package_links() {
        let plan = plan_reaching(&[("app", &["app", "core"]), ("core", &["core"])]);

        assert!(reaches(&binary("app"), "core", &plan, &ANY));
        assert!(reaches(&binary("app"), "app", &plan, &ANY));

        // The core crate does not link the app, so no test of it can reach the app's code.
        assert!(!reaches(&binary("core"), "app", &plan, &ANY));
    }

    #[test]
    fn an_unknown_package_reaches_everything() {
        let plan = plan_reaching(&[("app", &["app"])]);

        assert!(
            reaches(&binary(""), "core", &plan, &ANY),
            "an unattributed binary must not be skipped"
        );
        assert!(
            reaches(&binary("app"), "", &plan, &ANY),
            "an unattributed mutant must not be skipped"
        );
        assert!(reaches(&binary("stranger"), "core", &plan, &ANY), "a package we know nothing about");
    }

    #[test]
    fn a_test_package_filter_excludes_other_binaries() {
        let plan = plan_reaching(&[("app", &["app"]), ("other", &["other"])]);
        let named = [String::from("app")];
        let scope = TestScope {
            packages: &named,
            package_local: false,
            whole_workspace: false,
        };

        // Filtering by test package is a hard user request, so an otherwise reachable binary from
        // another package must not run.
        assert!(!reaches(&binary("other"), "other", &plan, &scope));
        assert!(reaches(&binary("app"), "app", &plan, &scope));
    }

    #[test]
    fn a_binary_is_attributed_to_the_package_that_produced_it() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/tmp/a","#,
            r#""package_id":"path+file:///w/crates/parser#cargo-gamma-lib@0.1.0"}"#,
            "\n",
        );

        assert_eq!(test_binaries(stdout)[0].package, "cargo-gamma-lib");
    }

    #[test]
    fn every_spelling_of_a_package_id_is_understood() {
        // The name is omitted when it matches the last path segment, which is the common case for
        // a workspace member and the one an over-eager parser gets wrong.
        assert_eq!(package_name("path+file:///w/crates/cargo-gamma-rt#0.1.0"), "cargo-gamma-rt");
        assert_eq!(
            package_name("path+file:///w/crates/parser#cargo-gamma-lib@0.1.0"),
            "cargo-gamma-lib"
        );
        assert_eq!(
            package_name("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"),
            "serde"
        );
        assert_eq!(package_name("serde 1.0.0 (registry+https://example.com)"), "serde");
    }

    #[test]
    fn a_pre_release_version_is_not_mistaken_for_a_package_name() {
        // The letters in `-beta` can look like a name, which would attribute the binary to a
        // package that does not exist and so quietly stop it reaching anything.
        assert_eq!(package_name("path+file:///w/crates/cargo-gamma-rt#1.0.0-beta.1"), "cargo-gamma-rt");
        assert_eq!(package_name("path+file:///w/crates/cargo-gamma-rt#1.0.0+build7"), "cargo-gamma-rt");

        // A name that merely begins with a digit is still a name.
        assert_eq!(package_name("path+file:///w/crates/x#3d-tiles"), "3d-tiles");
    }

    #[test]
    fn a_budget_is_scaled_from_the_binary_baseline() {
        let mut binaries = vec![binary("a"), binary("b")];

        binaries[0].baseline = Duration::from_secs(10);
        binaries[1].baseline = Duration::from_secs(30);

        apportion(&mut binaries, 1.2, Duration::ZERO, true);

        assert_eq!(binaries[0].budget, Some(Duration::from_secs(12)));
        assert_eq!(binaries[1].budget, Some(Duration::from_secs(36)));
    }

    /// A *measured* near-zero baseline, which is the only thing a zero duration may mean once the
    /// baseline has run.
    #[test]
    fn binaries_whose_baseline_was_instantaneous_get_the_floor() {
        let mut binaries = vec![binary("a"), binary("b"), binary("c")];

        apportion(&mut binaries, 1.2, Duration::from_secs(20), true);

        for entry in &binaries {
            assert_eq!(entry.budget, Some(Duration::from_secs(20)));
        }
    }

    /// Without a baseline every duration is zero because nothing was measured, and scaling that
    /// hands every binary the floor as though it had been derived. On a suite slower than the floor
    /// each mutant then runs out of time, and a timeout scores as an undetected mutant — a score of
    /// nearly zero made entirely of mutants no test ever got the chance to exercise.
    #[test]
    fn an_uncalibrated_run_gets_no_budget_rather_than_the_floor() {
        let mut binaries = vec![binary("a"), binary("b")];

        apportion(&mut binaries, 1.2, Duration::from_secs(20), false);

        for entry in &binaries {
            assert_eq!(entry.budget, None, "a cutoff was invented from no measurement");
            assert_eq!(entry.budget_for(None, Duration::from_secs(20)), None);

            // A per-mutant multiplier rescales a measurement; it cannot stand in for one.
            assert_eq!(entry.budget_for(Some(3.0), Duration::from_secs(20)), None);
        }
    }

    #[test]
    fn the_floor_is_a_promise_about_one_binary() {
        let mut binaries = vec![binary("a"), binary("b")];

        binaries[0].baseline = Duration::from_millis(1);
        binaries[1].baseline = Duration::from_secs(100);

        apportion(&mut binaries, 1.2, Duration::from_secs(20), true);

        // A binary whose proportional scaled duration is under the floor gets the floor.
        assert_eq!(binaries[0].budget, Some(Duration::from_secs(20)));

        // A binary that earned a larger share keeps it.
        assert_eq!(binaries[1].budget, Some(Duration::from_mins(2)));
    }

    #[test]
    fn a_crowded_workspace_does_not_dilute_the_floor_away() {
        let mut binaries: Vec<TestBinary> = (0..200).map(|_index| binary("a")).collect();

        for entry in &mut binaries {
            entry.baseline = Duration::from_millis(50);
        }

        apportion(&mut binaries, 1.2, Duration::from_secs(20), true);

        for entry in &binaries {
            assert_eq!(entry.budget, Some(Duration::from_secs(20)), "the floor was diluted again");
        }
    }

    #[test]
    fn a_binary_that_earns_more_than_the_floor_keeps_its_share() {
        let mut binaries = vec![binary("a"), binary("b")];

        binaries[0].baseline = Duration::from_secs(100);
        binaries[1].baseline = Duration::from_secs(100);

        apportion(&mut binaries, 1.2, Duration::from_secs(20), true);

        assert_eq!(binaries[0].budget, Some(Duration::from_mins(2)));
    }

    #[test]
    fn budget_for_computes_per_mutant_overridden_budget() {
        let mut b = binary("a");
        b.baseline = Duration::from_secs(10);
        b.budget = Some(Duration::from_secs(12)); // 10s baseline scaled by 1.2

        assert_eq!(b.budget_for(None, Duration::from_secs(1)), Some(Duration::from_secs(12)));
        assert_eq!(b.budget_for(Some(3.0), Duration::from_secs(1)), Some(Duration::from_secs(30)));
        assert_eq!(b.budget_for(Some(0.01), Duration::from_secs(5)), Some(Duration::from_secs(5)));
    }

    #[test]
    fn an_unreadable_package_id_reaches_everything_rather_than_nothing() {
        // Guessing "nothing" would silently stop testing a mutant and report it as unreachable,
        // which reads as a finding about the code rather than a failure of this parser.
        assert_eq!(package_name(""), "");
    }
}
