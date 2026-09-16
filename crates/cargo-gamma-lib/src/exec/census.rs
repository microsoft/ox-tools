// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Which tests can reach which mutation sites, measured rather than guessed.
//!
//! A mutant is only ever caught by a test that executes its site. The sweep does not know which
//! tests those are, so it runs every test of every binary that links the mutated package and pays
//! for the whole suite to reach one line. A census establishes the answer once, and then each
//! mutant runs only the tests that can possibly convict it.
//!
//! # Why this needs no coverage instrumentation
//!
//! The instrumented tree already calls `gamma_rt::a(id)` at every mutation site. Running it with
//! `GAMMA_CENSUS` set puts the runtime into census mode: every guard answers `false`, so the
//! process runs the code the author wrote, and each site records the fact that it was reached. That
//! is a finer probe than a coverage region and it is already keyed by the exact thing a mutant is
//! named by, so no second build and no `-C instrument-coverage` are involved.
//!
//! # Why it is exact rather than approximate
//!
//! A mutant `M` sits at one site `S`, and `M` changes nothing whatsoever before `S` executes. So a
//! test that reaches `S` with no mutant active reaches it with `M` active too, and a test that
//! never reaches `S` never fires `M` at all — its execution is identical to the baseline's, and it
//! still does not reach `S`. A complete census therefore measures the relation the sweep needs,
//! not a conservative approximation of it.
//!
//! The one thing that breaks the argument is a program whose execution is not a function of its
//! input: threads racing, wall-clock reads, randomness, hash iteration order. A test that reaches a
//! site only on some runs may be censused on a run where it did not, and the mutant it would have
//! caught is then reported as surviving. `--whole-test-binaries` is the conservative opt-out for
//! such a suite. A census cut short by its economic budget can only provide positive hints: a test
//! observed reaching a site is tried first, but anything other than a kill falls back to the whole
//! binary. Only a complete census can exclude tests or establish that a site is uncovered.
//!
//! # How a cut-short census is caught
//!
//! "Thrown away whole" needs a way to *know* a run was cut short, and an appended file gives no
//! obvious sign: a truncated write and an honest empty reach both leave a file this can parse. So
//! the runtime ends every clean census with a [`SEAL`] record, written from an `atexit` hook so it
//! lands even when the test reached no site at all. The file is appended and flushed as a prefix of
//! what was written, so the seal intact at the end proves every record before it is present too. A
//! file that is absent, unsealed, or sealed and then appended to is discarded rather than read,
//! which is what stops a dropped record ever being mistaken for an unreached site.

#[cfg(test)]
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::time::Duration;
use std::io::Read as _;
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::Mutex;
use std::sync::{Arc, mpsc};
use std::time::Instant;
use std::{fs, io, thread};

use camino::{Utf8Path, Utf8PathBuf};
#[cfg(test)]
use cargo_gamma_process::faults::{self as process_faults, Fault as ProcessFault};
use cargo_gamma_process::{MemoryRequest, ProcessTree, prepare};
use gamma_rt::{OVERFLOW, SEAL};

use super::events::Events;
#[cfg(test)]
use super::faults::{self, Fault};
use super::harness_filters::HarnessFilters;
#[cfg(test)]
use super::loader::LOADER_VAR;
use super::loader::configure_loader;
use super::stall::Stall;
use super::sweep::MIN_SCOUT_WAIT;
use super::test_binary::TestBinary;
use super::verdict::{Attempt, Only, Verdict, observe};
use super::workspace::Workspace;
use crate::discover::{GENERALIZED_HINTS_VERSION, GeneralizedHints, Killer, ReachCluster, SiteIdentity};
use crate::model::Mutant;
use crate::{HashMap, HashSet};

/// How much longer than the binary's whole baseline a single censused test may take.
///
/// One test out of a suite should be a fraction of the whole, so the whole baseline is already a
/// generous ceiling; the multiplier is there so that the *slowest* test in a fast binary is not cut
/// off by a budget calibrated on the average.
const CENSUS_FACTOR: u32 = 4;

/// Which tests reach which mutation sites, per test binary.
#[derive(Debug, Default)]
pub(super) struct Census {
    /// Keyed by binary path, which is what the sweep has in hand. A binary missing from here was
    /// never censused, or its census was discarded, and its mutants run its whole suite.
    binaries: HashMap<Utf8PathBuf, Reach>,

    /// How many sample subprocesses the census actually launched.
    ///
    /// One per scope run in census mode. The per-binary listing subprocess is excluded; only sample
    /// launches that probe reachability are counted.
    ///
    /// Carried so the diagnostics can weigh the census's cost against its sweep dividend: the count
    /// is exactly the number of sample subprocesses the census spent, and a run cannot say whether
    /// the census paid for itself without it. Zero on a [`Census::default`], which is what an
    /// unasked-for census is.
    walked: usize,
    listing_launches: usize,
    listed_binaries: usize,
    estimated_cost: Duration,
    walk_admitted: bool,
}

/// One binary's census.
#[derive(Debug, Default)]
struct Reach {
    /// Every test the harness announced, in the order it announced them.
    names: Vec<Box<str>>,

    /// For each site ordinal, an index into `intern` that names the tests reaching it.
    ///
    /// Interning eliminates duplication: many sites share the exact same reaching test set (e.g.
    /// all sites in one function body are typically reached by the same tests). Storing an index
    /// per site rather than a full vector per site reduces peak memory proportionally to that
    /// sharing factor.
    reached: HashMap<u32, u32>,

    /// Deduplicated reach sets. Each entry is a sorted set of test indices into `names`.
    intern: Vec<Arc<[u32]>>,

    /// Reverse lookup from sorted reach set to its index in `intern`.
    intern_index: HashMap<Arc<[u32]>, u32>,

    /// Sample cost assigned to every test run by each retained scope, parallel to `names`.
    times: Vec<Duration>,

    /// Whether every listed test completed and therefore absence proves a site is uncovered.
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Scope {
    tests: Vec<u32>,
}

impl Scope {
    fn names<'a>(&self, names: &'a [Box<str>]) -> Vec<&'a str> {
        self.tests
            .iter()
            .filter_map(|index| usize::try_from(*index).ok())
            .filter_map(|index| names.get(index))
            .map(Box::as_ref)
            .collect()
    }
}

impl Reach {
    /// Resolves a site to its interned reach set, or `None` if it was never recorded.
    fn tests_for(&self, ordinal: u32) -> Option<&[u32]> {
        let index = self.reached.get(&ordinal)?;
        self.intern.get(*index as usize).map(|s| &**s)
    }

    /// Interns a sorted set of test indices, returning its index.
    fn intern_set(&mut self, mut set: Vec<u32>) -> u32 {
        set.sort_unstable();
        set.dedup();

        let key: Arc<[u32]> = set.into();

        if let Some(&existing) = self.intern_index.get(&key) {
            return existing;
        }

        // #[gamma::skip(expr.decrement, reason = "the fallback differs only after allocating more than u32::MAX distinct reach sets, which cannot be observed within the process memory budget")]
        let index = u32::try_from(self.intern.len()).unwrap_or(u32::MAX);
        let _previous = self.intern_index.insert(Arc::clone(&key), index);
        self.intern.push(key);
        index
    }

    fn finish(&mut self) {
        // #[gamma::skip(assign_value.default, reason = "HashMap::default() and Default::default() construct the same empty index")]
        self.intern_index = HashMap::default();
    }
}

/// The work a censused binary represents for one mutant.
pub(super) enum CensusWork {
    /// Run the whole binary: it was not measured, or narrowing would not save enough to use.
    Whole,

    /// The census established that no test in the binary reaches the site.
    Uncovered,

    /// Run the selected tests, whose measured census durations sum to this.
    Selected(Duration),

    /// Try measured tests first, then run the whole binary if none kills the mutant.
    Hinted(Duration),
}

/// How a mutant should use one binary's census.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CensusSelection<'census> {
    /// Run the whole binary.
    Whole,

    /// A complete census established that no test reaches the site.
    Uncovered,

    /// A complete census established the only tests that can reach the site.
    Selected(Vec<&'census str>),

    /// An incomplete census found candidate tests that may kill the mutant.
    Hinted(Vec<&'census str>),
}

impl Census {
    /// The tests in `binary` that can reach the mutant with ordinal `ordinal`.
    ///
    /// `None` means nothing is known about this binary and its whole suite must run. `Some` of an
    /// empty slice is the opposite and much stronger claim: this binary was censused, and no test
    /// in it reaches that site. The distinction is the whole safety property — an absent census
    /// must never be read as an absent test.
    pub(super) fn selection(&self, binary: &TestBinary, ordinal: u32) -> CensusSelection<'_> {
        let Some(reach) = self.binaries.get(&binary.path) else {
            return CensusSelection::Whole;
        };

        let Some(indices) = reach.tests_for(ordinal) else {
            return if reach.complete {
                CensusSelection::Uncovered
            } else {
                CensusSelection::Whole
            };
        };

        // Past half the suite, naming the tests costs more than it saves: the command line grows
        // with every name, and the run it replaces was barely narrower. Answering `None` here says
        // "nothing is known", which is the answer that makes the caller run the whole binary — the
        // same thing, reached more cheaply.
        if indices.len().saturating_mul(2) > reach.names.len() {
            return CensusSelection::Whole;
        }

        let names = indices
            .iter()
            .filter_map(|index| usize::try_from(*index).ok())
            .filter_map(|index| reach.names.get(index))
            .map(Box::as_ref)
            .collect();

        if reach.complete {
            CensusSelection::Selected(names)
        } else {
            CensusSelection::Hinted(names)
        }
    }

    /// Legacy projection used by the reachability unit tests.
    #[cfg(test)]
    fn reaching(&self, binary: &TestBinary, ordinal: u32) -> Option<Vec<&str>> {
        match self.selection(binary, ordinal) {
            CensusSelection::Whole | CensusSelection::Hinted(_) => None,
            CensusSelection::Uncovered => Some(Vec::new()),
            CensusSelection::Selected(names) => Some(names),
        }
    }

    /// Estimates the work this binary contributes for one mutant from the census itself.
    pub(super) fn work(&self, binary: &TestBinary, ordinal: u32) -> CensusWork {
        let Some(reach) = self.binaries.get(&binary.path) else {
            return CensusWork::Whole;
        };

        let Some(indices) = reach.tests_for(ordinal) else {
            return if reach.complete { CensusWork::Uncovered } else { CensusWork::Whole };
        };

        if indices.len().saturating_mul(2) > reach.names.len() {
            return CensusWork::Whole;
        }

        let duration = indices
            .iter()
            .filter_map(|index| usize::try_from(*index).ok())
            .filter_map(|index| reach.times.get(index))
            .copied()
            .sum();

        if reach.complete {
            CensusWork::Selected(duration)
        } else {
            CensusWork::Hinted(duration)
        }
    }

    /// How many binaries were censused, for the line the run prints.
    pub(super) fn len(&self) -> usize {
        self.binaries.len()
    }

    /// Whether at least one binary produced a usable census result.
    pub(super) fn has_evidence(&self) -> bool {
        !self.binaries.is_empty()
    }

    /// Whether every candidate binary produced complete reachability evidence.
    pub(super) fn is_complete_for(&self, candidates: usize) -> bool {
        self.binaries.len() == candidates && self.binaries.values().all(|reach| reach.complete)
    }

    /// Candidate binaries that produced complete evidence.
    pub(super) fn complete_binaries(&self) -> usize {
        self.binaries.values().filter(|reach| reach.complete).count()
    }

    /// Candidate binaries that produced only positive checked hints.
    pub(super) fn incomplete_binaries(&self) -> usize {
        self.binaries.values().filter(|reach| !reach.complete).count()
    }

    pub(super) const fn listing_launches(&self) -> usize {
        self.listing_launches
    }

    pub(super) const fn listed_binaries(&self) -> usize {
        self.listed_binaries
    }

    pub(super) const fn estimated_cost(&self) -> Duration {
        self.estimated_cost
    }

    pub(super) const fn walk_admitted(&self) -> bool {
        self.walk_admitted
    }

    /// Whether enumeration succeeded but the projected walk could not repay its own cost.
    pub(super) const fn walk_declined(&self) -> bool {
        !self.walk_admitted && self.listed_binaries != 0
    }

    /// How many sample subprocesses the census actually launched.
    ///
    /// The count of sample runs that were really spawned, which excludes the tests of a binary that
    /// was spoiled before reaching them and the per-binary listing subprocesses. See the [`walked`]
    /// field.
    ///
    /// [`walked`]: Census::walked
    pub(super) const fn walked(&self) -> usize {
        self.walked
    }

    /// Adds positive census reach clusters to the shared durable hint schema.
    pub(super) fn persist_clusters(&self, mutants: &[Mutant], binaries: &[TestBinary], hints: &mut GeneralizedHints) {
        hints.version = GENERALIZED_HINTS_VERSION;
        let by_ordinal: HashMap<u32, &Mutant> = mutants.iter().map(|mutant| (mutant.ordinal, mutant)).collect();
        let mut sites: HashMap<SiteIdentity, Vec<Killer>> = hints
            .reach
            .iter()
            .filter_map(|cluster| {
                let tests = hints.test_sets.get(cluster.test_set as usize)?.clone();

                Some((cluster.site.clone(), tests))
            })
            .collect();
        let mut replaced = HashSet::default();

        for binary in binaries {
            let Some(reach) = self.binaries.get(&binary.path) else {
                continue;
            };
            for (mutant, indices) in reach.reached.iter().filter_map(|(&ordinal, &set_index)| {
                let mutant = *by_ordinal.get(&ordinal)?;
                let indices = reach.intern.get(set_index as usize)?;
                Some((mutant, indices))
            }) {
                if indices.len().saturating_mul(2) > reach.names.len() {
                    continue;
                }
                let measured: Duration = indices
                    .iter()
                    .filter_map(|index| usize::try_from(*index).ok())
                    .filter_map(|index| reach.times.get(index))
                    .copied()
                    .sum();
                if !can_repay(measured, binary.baseline) {
                    continue;
                }
                let site = SiteIdentity::from_mutant(mutant);
                if replaced.insert(site.clone()) {
                    sites.remove(&site);
                }
                let tests = sites.entry(site).or_default();
                tests.extend(
                    indices
                        .iter()
                        .filter_map(|index| usize::try_from(*index).ok())
                        .filter_map(|index| reach.names.get(index))
                        .map(|name| Killer {
                            package: binary.package.clone(),
                            target: binary.target.clone(),
                            test: name.to_string(),
                        }),
                );
            }
        }

        let mut entries: Vec<(SiteIdentity, Vec<Killer>)> = sites
            .into_iter()
            .filter_map(|(site, mut tests)| {
                tests.sort_by(|left, right| {
                    left.package
                        .cmp(&right.package)
                        .then_with(|| left.target.cmp(&right.target))
                        .then_with(|| left.test.cmp(&right.test))
                });
                tests.dedup();

                (!tests.is_empty()).then_some((site, tests))
            })
            .collect();
        entries.sort_by(|(left, _), (right, _)| {
            left.file
                .cmp(&right.file)
                .then_with(|| left.item.cmp(&right.item))
                .then_with(|| left.mutator.cmp(&right.mutator))
                .then_with(|| left.normalized_text.cmp(&right.normalized_text))
                .then_with(|| left.occurrence.cmp(&right.occurrence))
        });

        let mut test_sets: Vec<Vec<Killer>> = entries.iter().map(|(_, tests)| tests.clone()).collect();
        test_sets.sort_by(|left, right| {
            left.iter()
                .map(|killer| (&killer.package, &killer.target, &killer.test))
                .cmp(right.iter().map(|killer| (&killer.package, &killer.target, &killer.test)))
        });
        test_sets.dedup();

        hints.reach = entries
            .into_iter()
            .filter_map(|(site, tests)| {
                let test_set = test_sets.iter().position(|known| *known == tests)?;
                Some(ReachCluster {
                    site,
                    test_set: u32::try_from(test_set).ok()?,
                })
            })
            .collect();
        hints.test_sets = test_sets;
    }

    /// A census recording `binary` as examined with `total` tests, `reached` of which reach `site`.
    ///
    /// For tests in sibling modules, whose own `#[cfg(test)]` code cannot reach the private [`Reach`]
    /// this builds. It exists to drive how a *consumer* reacts to a census — in particular the
    /// boundary where a site most of the suite reaches makes [`reaching`] answer `None`, which means
    /// "run the whole binary" and must never be confused with the empty list that means "no test
    /// reaches it".
    ///
    /// [`reaching`]: Census::reaching
    #[cfg(all(test, unix))]
    pub(super) fn examined(binary: &Utf8Path, site: u32, reached: usize, total: usize) -> Self {
        Self::fixture(binary, site, reached, total, true)
    }

    /// A deliberately incomplete census for testing checked-hint fallbacks.
    #[cfg(test)]
    pub(super) fn partial(binary: &Utf8Path, site: u32, reached: usize, total: usize) -> Self {
        Self::fixture(binary, site, reached, total, false)
    }

    /// A deliberately complete census for orchestration tests.
    #[cfg(test)]
    pub(super) fn complete(binary: &Utf8Path) -> Self {
        Self::fixture(binary, 7, 1, 1, true)
    }

    /// A census whose listing succeeded but whose walk could not repay itself.
    #[cfg(test)]
    pub(super) fn declined() -> Self {
        Self {
            binaries: HashMap::default(),
            walked: 0,
            listing_launches: 1,
            listed_binaries: 1,
            estimated_cost: Duration::ZERO,
            walk_admitted: false,
        }
    }

    #[cfg(test)]
    fn fixture(binary: &Utf8Path, site: u32, reached: usize, total: usize, complete: bool) -> Self {
        let names: Vec<Box<str>> = (0..total).map(|index| format!("tests::t{index}").into()).collect();
        let indices: Vec<u32> = (0..reached).filter_map(|index| u32::try_from(index).ok()).collect();

        let mut reach = Reach {
            names,
            reached: HashMap::default(),
            intern: Vec::new(),
            intern_index: HashMap::default(),
            times: vec![Duration::from_millis(1); total],
            complete,
        };
        let set_index = reach.intern_set(indices);
        let _previous2 = reach.reached.insert(site, set_index);
        reach.finish();

        let mut census = Self::default();
        let _previous = census.binaries.insert(binary.to_owned(), reach);

        census
    }
}

/// Measures which tests reach which sites, one binary at a time.
///
/// Never fails the run. A binary that cannot be listed, or whose sampled tests did not all pass,
/// simply does not appear in the result. A budget-limited binary retains only positive reach hints,
/// and every inconclusive hint falls back to the whole binary.
pub(super) fn take(
    work: &Workspace,
    binaries: &[TestBinary],
    targets: &HashMap<Utf8PathBuf, HashSet<u32>>,
    maximum_savings: Duration,
    jobs: usize,
    stall: Stall,
    events: &mut impl Events,
) -> Census {
    let mut census = Census::default();
    let binaries: Vec<&TestBinary> = binaries.iter().filter(|binary| targets.contains_key(&binary.path)).collect();
    census.listing_launches = binaries.len();

    events.begin(
        "Optimizing",
        "Optimized",
        &format!("{} test {}", binaries.len(), plural(binaries.len())),
    );

    let total = binaries.len();
    let unit = format!("test {}", plural(total));
    let mut completed = 0_usize;

    events.phase_progress(completed, total, &unit);

    let mut listed: Vec<(&TestBinary, Vec<Box<str>>, Duration)> = Vec::with_capacity(binaries.len());
    // A proxy for the walk's cost, not a measurement of it: `--list` pays only process startup and
    // enumeration, while the walk below pays that once per binary and then, per listed test, the
    // loader setup, containment, output supervision, and execution that `walk` (below) actually
    // performs. A model closer to the walk's true cost needs a campaign measurement comparing
    // current, sampled, and census-disabled admission over test count, duration, mutant count, and
    // reach density; scaling this estimate by a guessed per-test multiplier without that data would
    // only trade one unmeasured policy for another.
    let mut estimated_cost = Duration::ZERO;

    for binary in &binaries {
        let started = Instant::now();
        let Some(mut names) = list(work, binary) else {
            completed += 1;
            events.phase_progress(completed, total, &unit);

            continue;
        };
        let listing_cost = started.elapsed();
        names.sort_unstable();
        let scopes = initial_scopes(&names);
        let launch_cost = listing_cost.max(MIN_SCOUT_WAIT);

        estimated_cost = add_listing_cost(estimated_cost, launch_cost, scopes.len());
        listed.push((binary, names, launch_cost));
    }
    census.listed_binaries = listed.len();
    census.estimated_cost = estimated_cost;

    if !can_repay(estimated_cost, maximum_savings) {
        events.end("");

        return census;
    }
    census.walk_admitted = !listed.is_empty();

    // The count comes back from the walk rather than being summed from the lists above, because a
    // binary spoiled partway through skips the rest of its tests: those are listed but never
    // launched, and `walked` counts launches, not intentions.
    let deadline = Instant::now().checked_add(maximum_savings);
    let (mapped, walked) = walk(work, listed, targets, deadline, jobs, stall, || {
        completed += 1;
        events.phase_progress(completed, total, &unit);
    });

    for (path, reach) in mapped {
        let _previous = census.binaries.insert(path, reach);
    }

    census.walked = walked;

    events.end(&format!(
        ", {} of {} {} mapped, over {walked} {}",
        census.len(),
        binaries.len(),
        plural(binaries.len()),
        if walked == 1 { "sample" } else { "samples" }
    ));

    census
}

/// Whether the estimated census work is smaller than everything it could possibly save.
///
/// `estimated_cost` is the listing-time proxy described where the caller builds it, not a
/// measurement of the walk it is gating.
fn can_repay(estimated_cost: Duration, maximum_savings: Duration) -> bool {
    !maximum_savings.is_zero() && estimated_cost < maximum_savings
}

fn add_listing_cost(estimated: Duration, launch: Duration, scopes: usize) -> Duration {
    estimated.saturating_add(launch.saturating_mul(u32::try_from(scopes).unwrap_or(u32::MAX)))
}

/// "binary" or "binaries", so the line above reads as English either way.
const fn plural(count: usize) -> &'static str {
    if count == 1 { "binary" } else { "binaries" }
}

/// How long a binary is given to answer `--list` before the answer is abandoned.
///
/// A real libtest `--list` enumerates a table already in the binary and returns in milliseconds, so
/// anything approaching this is not listing at all. The number is generous against a cold page
/// cache and a loaded machine rather than tuned, because the cost of being wrong is asymmetric: too
/// short loses a census that would have saved time, too long is the hang this exists to bound.
const LIST_BUDGET: Duration = Duration::from_secs(30);

/// How much of a listing is worth reading before the binary is not listing.
///
/// A libtest listing is one short line per test, so even a very large suite is well under this. A
/// binary that ignored the flags and started running instead can print without limit, and buffering
/// that would turn one unlucky target into an out-of-memory kill of the whole run.
const LIST_CAP: usize = 4 * 1024 * 1024;

/// How often the wait below looks at a child that has not finished.
///
/// Listing is expected to be over before the first poll, so this trades a millisecond of latency on
/// the answer for not spinning a core for the length of the budget on the one that hangs.
const LIST_POLL: Duration = Duration::from_millis(5);

/// Asks a test binary to name its tests.
///
/// The binary is run directly even when the run is under nextest, because listing is a question
/// about the executable rather than about the runner, and the libtest listing format is the one
/// both the filter arguments and the census launches below are expressed in.
///
/// `None` for anything that did not answer in that format — a `harness = false` target, most
/// obviously — or did not answer within [`LIST_BUDGET`], which is the same target when its
/// `fn main()` ignores the flags and runs its suite instead. Either way the binary is left without
/// a census and therefore run in full, which is the answer this had before the census existed.
fn list(work: &Workspace, binary: &TestBinary) -> Option<Vec<Box<str>>> {
    let command = listing_command(work, binary);

    // A successful process with no libtest records may be a custom harness that ignored both
    // flags. Without a positive record there is no evidence that the output was a complete census,
    // so leave the binary uncensused and run it whole.
    listed(command, LIST_BUDGET).filter(|names| !names.is_empty())
}

/// Builds the direct `--list` invocation for one test binary.
fn listing_command(work: &Workspace, binary: &TestBinary) -> Command {
    let mut command = Command::new(binary.path.as_std_path());

    let _ = command
        .args(["--list", "--format=terse"])
        // The user's own filters, so the census only ever records tests that are actually going to
        // run. Without them the listing discovers names outside the filter and feeds them back as a
        // selection, which the launcher then has to refuse. Everything but a `--format` of their
        // own is carried, since that one would fight with the format asked for here.
        .args(HarnessFilters::parse(work.test_arguments()).selecting())
        .current_dir(working_directory(work, binary).as_std_path())
        .stderr(Stdio::null());

    configure_loader(&mut command, work.launch());
    work.configure_test_environment(&mut command, binary);
    work.inherit_test_cache_home(&mut command);

    command
}

fn read_listing(pipe: &mut impl io::Read) -> (Vec<u8>, bool) {
    let mut text = Vec::new();
    // #[gamma::skip(literal.int_increment, reason = "the buffer length changes only read chunking; the bounded accumulator and whole-stream flag expose identical bytes")]
    let mut buffer = [0_u8; 8192];
    let mut whole = true;

    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,

            Ok(read) => {
                // Reading continues past the cap so the child never blocks on a full pipe; only the
                // keeping stops.
                let room = LIST_CAP.saturating_sub(text.len());

                text.extend_from_slice(&buffer[..read.min(room)]);
                whole &= read <= room;
            }

            Err(cause) if cause.kind() == std::io::ErrorKind::Interrupted => {}

            Err(_truncated) => {
                // #[gamma::skip(assign_value.default, reason = "false is the default bool value, so the replacement is identical")]
                whole = false;

                // #[gamma::skip(all, reason = "retrying a terminal pipe error spins forever because the same broken reader cannot recover")]
                break;
            }
        }
    }

    (text, whole)
}

/// Runs a listing command under the same containment and bound every other spawn here gets.
///
/// Separate from [`list`] because the command is the only thing a test needs to vary, and a binary
/// that hangs on `--list` cannot be written as a test binary the way the rest of the helper scripts
/// are: the flags this passes are exactly the ones such a target ignores.
///
/// The containment is not optional here even though nothing is metered. A target built with
/// `harness = false` is a `fn main()` that may ignore unknown flags entirely, and the ones that do
/// run their whole suite in answer to this — spawning whatever their tests spawn. Uncontained, a
/// `SIGTERM` to the tool would leave all of that running, holding scratch-tree locks that fail the
/// next run; unbounded, one such target hangs a run that has nothing watching it, since this is the
/// only spawn in the subsystem outside the wait loop.
///
/// A budget reached is `None` rather than a partial listing: half an enumeration is a census that
/// believes some tests do not exist, and a test believed not to exist is one no mutant is ever run
/// against.
fn listed(mut command: Command, budget: Duration) -> Option<Vec<Box<str>>> {
    enum ListingStatus {
        Observed(std::process::ExitStatus),
        Missing,
    }

    // Nothing is metered — the question is what the binary is, not what it costs — so the request
    // asks for no measurement and no ceiling. Containment does not follow that request: `prepare`
    // seals every launch it can, and the listing of a `harness = false` target is exactly the kind
    // of repository-controlled code that spawns things and leaves the process group behind it.
    let _ = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());

    let prepared = prepare(command, MemoryRequest::default()).ok()?;
    let spawned = prepared.spawn().ok()?;

    let mut subtree = match ProcessTree::adopt(spawned) {
        Ok(subtree) => subtree,

        // Adoption already ended and reaped the unwatchable child.
        Err(_unwatchable) => return None,
    };

    // Read on another thread rather than after the wait: a binary that prints more than a pipe
    // holds blocks in `write` until somebody empties it, and a wait that has not started reading
    // would then time out every target with a listing longer than 64 KB.
    //
    // Whether the stream really ended travels with the bytes for the same reason it does on the
    // verdict path: a prefix of a listing is a set of tests that appear not to exist, and a test
    // that appears not to exist is one no mutant is ever run against.
    // #[gamma::skip(literal.int_decrement, reason = "a zero-capacity output channel deadlocks the child wait and reader rendezvous until the mutation budget expires")]
    // #[gamma::skip(literal.int_increment, reason = "one reader sends exactly one terminal message, so capacities one and two have identical ordering and backpressure")]
    let (sender, receiver) = mpsc::sync_channel(1);
    let reading = subtree.take_stdout().map(|mut pipe| {
        let sender = sender.clone();
        let failed = sender.clone();

        // The handle is deliberately detached. A descendant that escaped containment can retain
        // this pipe indefinitely, and the listing deadline must bound the caller rather than make
        // it wait for a reader no process can force to return.
        #[cfg(test)]
        let refused = faults::fired(Fault::Thread);
        #[cfg(not(test))]
        let refused = false;

        let spawned = if refused {
            // #[gamma::skip(all, reason = "the synthetic reader-thread error is deliberately discarded and all variants produce the same conservative missing-census result")]
            Err(io::Error::other("the reader thread a test asked to fail"))
        } else {
            // #[gamma::skip(all, reason = "the detached thread name is diagnostic metadata and does not affect listing bytes or admission")]
            thread::Builder::new().name("cargo-gamma-census-output".to_owned()).spawn(move || {
                let (text, whole) = read_listing(&mut pipe);
                let _sent = sender.send((text, whole));
            })
        };

        if spawned.is_err() {
            let _sent = failed.send((Vec::new(), false));
        }
    });
    // #[gamma::skip(stmt.delete_call, reason = "retaining the coordinator sender prevents channel disconnection and holds a completed listing until its deadline")]
    drop(sender);

    let deadline = Instant::now() + budget;

    let status = loop {
        match subtree.observe() {
            Ok(Some(status)) => break ListingStatus::Observed(status),

            // #[gamma::skip(match_guard.always_false, reason = "disabling the deadline arm leaves the zero-duration polling loop non-terminating after the budget expires")]
            Ok(None) if Instant::now() >= deadline => break ListingStatus::Missing,

            // #[gamma::skip(iter.min_to_max, reason = "using the remaining budget instead of the polling interval changes only wake-up latency; the same child status and deadline decide the result")]
            Ok(None) => thread::sleep(LIST_POLL.min(deadline.saturating_duration_since(Instant::now()))),

            // The handle is gone, so the one question that would settle this cannot be asked again.
            Err(_unaskable) => break ListingStatus::Missing,
        }
    };

    // Whatever the child spawned goes with it when no normal exit was observed: a listing that
    // started a server leaves it holding the pipe this is about to read, and the reader would then
    // wait out the whole run for an end of file that never comes. Cleanup failure stays fail-open:
    // census is an optimization, and the bounded drain below turns an unclosed pipe into a missing
    // census rather than failing mutation testing before the ordinary discovery path can run.
    match &status {
        ListingStatus::Missing => {
            let _reaped = subtree.terminate();
        }
        ListingStatus::Observed(_) => {
            debug_assert!(
                subtree.released(),
                "the observed listing released containment before its output was read"
            );
        }
    }

    // A group cleanup normally closes every inherited descriptor, but a descendant that called
    // `setsid` can retain stdout after leaving that group. The original listing deadline covers
    // this final drain too: abandoning its blocked reader loses only a census optimization, while
    // joining it would turn one escaped process into an unbounded run.
    let (text, whole) = reading
        .and_then(|()| receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok())
        .unwrap_or_default();

    let ListingStatus::Observed(status) = status else {
        return None;
    };
    if !whole || !status.success() {
        return None;
    }

    let text = String::from_utf8(text).ok()?;

    Some(
        text.lines()
            .filter_map(|line| line.strip_suffix(": test"))
            .map(|name| name.trim().into())
            .collect(),
    )
}

/// Where a test binary would be run from, mirroring what the sweep does.
fn working_directory<'work>(work: &'work Workspace, binary: &'work TestBinary) -> &'work Utf8Path {
    if binary.manifest_dir.as_str().is_empty() {
        work.root()
    } else {
        &binary.manifest_dir
    }
}

/// Samples deterministic test scopes and refines only scopes whose measured work can repay it.
///
/// A complete hierarchy proves group-level absence. Any failed, expired, or inconsistent sample
/// makes the binary incomplete, retaining positive observations only as checked hints.
fn walk(
    work: &Workspace,
    listed: Vec<(&TestBinary, Vec<Box<str>>, Duration)>,
    targets: &HashMap<Utf8PathBuf, HashSet<u32>>,
    deadline: Option<Instant>,
    jobs: usize,
    stall: Stall,
    completed: impl FnMut(),
) -> (Vec<(Utf8PathBuf, Reach)>, usize) {
    walk_with(
        work,
        listed,
        targets,
        // #[gamma::skip(all, reason = "walk_with currently executes the deterministic planner serially and intentionally ignores this compatibility parameter")]
        jobs,
        stall,
        sample,
        || deadline.is_some_and(|limit| deadline_reached(Instant::now(), limit)),
        completed,
    )
}

fn deadline_reached(now: Instant, limit: Instant) -> bool {
    now >= limit
}

#[derive(Debug)]
struct ScopeEvidence {
    scope: Scope,
    sites: Vec<u32>,
    elapsed: Duration,
}

struct Planner<'a, S, E> {
    work: &'a Workspace,
    binary: &'a TestBinary,
    names: &'a [Box<str>],
    wanted: &'a HashSet<u32>,
    path: &'a Utf8Path,
    stall: Stall,
    launch_cost: Duration,
    sampler: &'a S,
    expired: &'a E,
    launches: usize,
    complete: bool,
}

impl<S, E> Planner<'_, S, E>
where
    S: Fn(&Workspace, &TestBinary, &[&str], &Utf8Path, Stall) -> Option<(Vec<u32>, Duration)>,
    E: Fn() -> bool,
{
    fn explore(&mut self, scope: Scope) -> Vec<ScopeEvidence> {
        if (self.expired)() {
            // #[gamma::skip(assign_value.default, reason = "false is the default bool value, so the replacement is identical")]
            self.complete = false;
            return Vec::new();
        }

        self.launches = self.launches.saturating_add(1);
        let selected = scope.names(self.names);
        let Some((mut sites, elapsed)) = (self.sampler)(self.work, self.binary, &selected, self.path, self.stall) else {
            // #[gamma::skip(assign_value.default, reason = "false is the default bool value, so the replacement is identical")]
            self.complete = false;
            return Vec::new();
        };

        sites.retain(|site| self.wanted.contains(site));
        sites.sort_unstable();
        sites.dedup();

        let Some(children) = subdivision(&scope, self.names).filter(|children| {
            should_subdivide(
                children,
                &scope,
                self.names,
                &sites,
                self.wanted,
                self.launch_cost,
                self.binary.baseline,
            )
        }) else {
            return vec![ScopeEvidence { scope, sites, elapsed }];
        };

        let mut finer = Vec::new();
        let mut child_sites = HashSet::default();
        let complete_before = self.complete;

        for child in children {
            for evidence in self.explore(child) {
                child_sites.extend(evidence.sites.iter().copied());
                finer.push(evidence);
            }
        }

        let consistent = consistent_subdivision(complete_before, self.complete, &sites, &child_sites);

        if consistent {
            finer
        } else {
            // #[gamma::skip(assign_value.default, reason = "false is the default bool value, so the replacement is identical")]
            self.complete = false;
            vec![ScopeEvidence { scope, sites, elapsed }]
        }
    }
}

fn should_subdivide(
    children: &[Scope],
    scope: &Scope,
    names: &[Box<str>],
    sites: &[u32],
    wanted: &HashSet<u32>,
    launch_cost: Duration,
    baseline: Duration,
) -> bool {
    mixed(sites, wanted) && subdivision_repays(children.len(), launch_cost, baseline, scope.tests.len(), names.len(), sites.len())
}

fn consistent_subdivision(complete_before: bool, complete_after: bool, parent_sites: &[u32], child_sites: &HashSet<u32>) -> bool {
    complete_after == complete_before
        && child_sites.len() == parent_sites.len()
        && parent_sites.iter().all(|site| child_sites.contains(site))
}

fn initial_scopes(names: &[Box<str>]) -> Vec<Scope> {
    let whole = Scope {
        tests: (0..names.len()).filter_map(|index| u32::try_from(index).ok()).collect(),
    };

    module_children(&whole, names).unwrap_or_else(|| contiguous_children(&whole, true))
}

fn subdivision(scope: &Scope, names: &[Box<str>]) -> Option<Vec<Scope>> {
    if scope.tests.len() <= 1 {
        return None;
    }

    module_children(scope, names)
        .or_else(|| Some(contiguous_children(scope, false)))
        .filter(|children| children.len() > 1)
}

fn module_children(scope: &Scope, names: &[Box<str>]) -> Option<Vec<Scope>> {
    let split: Vec<Vec<&str>> = scope
        .tests
        .iter()
        .filter_map(|index| usize::try_from(*index).ok())
        .filter_map(|index| names.get(index))
        .map(|name| name.split("::").collect())
        .collect();

    let first = split.first()?;
    let common = (0..first.len())
        .take_while(|&part| split.iter().all(|name| name.get(part) == first.get(part)))
        .count();

    if split.iter().any(|name| name.len() <= common + 1) {
        return None;
    }

    let mut groups: Vec<Scope> = Vec::new();
    let mut previous: Option<&str> = None;

    for (&index, parts) in scope.tests.iter().zip(&split) {
        let key = parts[common];
        if previous != Some(key) {
            groups.push(Scope { tests: Vec::new() });
            previous = Some(key);
        }
        groups
            .last_mut()
            .expect("a group is inserted before its first test")
            .tests
            .push(index);
    }

    (groups.len() > 1).then_some(groups)
}

fn contiguous_children(scope: &Scope, initial: bool) -> Vec<Scope> {
    let chunk = if initial && scope.tests.len() == 2 {
        1
    } else if initial {
        ceil_sqrt(scope.tests.len()).max(1)
    } else {
        scope.tests.len().div_ceil(2)
    };

    scope.tests.chunks(chunk).map(|tests| Scope { tests: tests.to_vec() }).collect()
}

fn ceil_sqrt(value: usize) -> usize {
    let mut root = 0_usize;
    while root.saturating_mul(root) < value {
        // #[gamma::skip(all, reason = "not advancing the integer square-root candidate makes this loop non-terminating")]
        root = root.saturating_add(1);
    }
    root
}

fn mixed(sites: &[u32], wanted: &HashSet<u32>) -> bool {
    sites.len() > 1 && sites.len() < wanted.len()
}

fn subdivision_repays(
    launches: usize,
    launch_cost: Duration,
    baseline: Duration,
    scope_tests: usize,
    total_tests: usize,
    reached_sites: usize,
) -> bool {
    if total_tests == 0 || reached_sites == 0 {
        return false;
    }

    let sampling = launch_cost.saturating_mul(u32::try_from(launches).unwrap_or(u32::MAX));
    let remaining = scale_duration(baseline, scope_tests, total_tests).saturating_mul(u32::try_from(reached_sites).unwrap_or(u32::MAX));

    sampling.saturating_mul(2) < remaining
}

fn scale_duration(duration: Duration, numerator: usize, denominator: usize) -> Duration {
    // #[gamma::skip(expr.decrement, reason = "usize converts exactly to u128 on every supported target, so this fallback cannot be observed")]
    let denominator = u128::try_from(denominator).unwrap_or(u128::MAX).max(1);
    // #[gamma::skip(expr.decrement, reason = "usize converts exactly to u128 on every supported target, so this fallback cannot be observed")]
    let numerator = u128::try_from(numerator).unwrap_or(u128::MAX);
    let nanos = duration.as_nanos().saturating_mul(numerator) / denominator;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the planner's execution seam is injected for deterministic tests"
)]
fn walk_with<S, E>(
    work: &Workspace,
    listed: Vec<(&TestBinary, Vec<Box<str>>, Duration)>,
    targets: &HashMap<Utf8PathBuf, HashSet<u32>>,
    _jobs: usize,
    stall: Stall,
    sampler: S,
    expired: E,
    mut completed: impl FnMut(),
) -> (Vec<(Utf8PathBuf, Reach)>, usize)
where
    S: Fn(&Workspace, &TestBinary, &[&str], &Utf8Path, Stall) -> Option<(Vec<u32>, Duration)>,
    E: Fn() -> bool,
{
    let directory = work.base().join("census");

    if fs::create_dir_all(directory.as_std_path()).is_err() {
        return (Vec::new(), 0);
    }

    let mut mapped = Vec::with_capacity(listed.len());
    let mut launches = 0_usize;

    for (binary_at, (binary, names, launch_cost)) in listed.into_iter().enumerate() {
        let Some(wanted) = targets.get(&binary.path) else {
            completed();
            continue;
        };
        let path = directory.join(format!("{binary_at}.bin"));
        let (evidence, binary_launches, complete) = {
            let mut planner = Planner {
                work,
                binary,
                names: &names,
                wanted,
                path: &path,
                stall,
                launch_cost,
                sampler: &sampler,
                expired: &expired,
                launches: 0,
                complete: true,
            };
            let mut evidence = Vec::new();

            for scope in initial_scopes(&names) {
                evidence.extend(planner.explore(scope));
            }

            (evidence, planner.launches, planner.complete)
        };
        launches = launches.saturating_add(binary_launches);
        let mut raw_reached: HashMap<u32, Vec<u32>> = HashMap::default();
        let mut times = vec![Duration::ZERO; names.len()];

        for observed in evidence {
            let divisor = u32::try_from(observed.scope.tests.len()).unwrap_or(u32::MAX).max(1);
            let per_test = observed.elapsed / divisor;
            let remainder = observed.elapsed.saturating_sub(per_test * divisor);
            for (position, index) in observed.scope.tests.iter().enumerate() {
                if let Ok(index) = usize::try_from(*index)
                    && let Some(time) = times.get_mut(index)
                {
                    let share = if position == 0 {
                        per_test.saturating_add(remainder)
                    } else {
                        per_test
                    };
                    *time = time.saturating_add(share);
                }
            }
            for site in observed.sites {
                raw_reached.entry(site).or_default().extend(observed.scope.tests.iter().copied());
            }
        }

        if !complete && raw_reached.is_empty() {
            completed();
            continue;
        }

        let mut reach = Reach {
            names,
            reached: HashMap::default(),
            intern: Vec::new(),
            intern_index: HashMap::default(),
            times,
            complete,
        };

        for (site, mut indices) in raw_reached {
            indices.sort_unstable();
            indices.dedup();
            let set_index = reach.intern_set(indices);
            let _previous = reach.reached.insert(site, set_index);
        }
        reach.finish();

        mapped.push((binary.path.clone(), reach));
        completed();
    }

    (mapped, launches)
}

/// Runs one test scope in census mode and returns the site ordinals it reached.
///
/// `None` means the sample cannot be trusted, which is the whole binary's problem rather than this
/// test's: see [`walk`].
fn sample(work: &Workspace, binary: &TestBinary, names: &[&str], path: &Utf8Path, stall: Stall) -> Option<(Vec<u32>, Duration)> {
    // Appended to by the runtime, so a leftover from the previous test on this worker would be read
    // as part of this one.
    let _removed = fs::remove_file(path.as_std_path());

    let attempt = Attempt {
        // No mutant. A census must see the program the author wrote, or the sites it records are
        // the ones some mutant steered it towards.
        active: None,
        timeout: binary
            .budget
            .map(|budget| binary.baseline.saturating_mul(CENSUS_FACTOR).max(budget)),
        stall,
        request: MemoryRequest { meter: false, limit: None },
        only: Only::These(names),
        census: Some(path),
    };

    let started = Instant::now();

    match observe(work, binary, attempt).verdict {
        Verdict::Passed => {}
        _spoiled => return None,
    }

    let elapsed = started.elapsed();

    // With the runtime sealing every census it completes — even one whose test reached no site, and
    // so left only the lone seal — an absent or unreadable file is no longer an empty reach. It
    // means the census never finished: the open failed, or the process died before it could seal.
    // That is the binary's problem, not this test's answer, so the sample is spoiled rather than
    // believed empty.
    let bytes = read_bounded(path).ok()?;

    decode(&bytes).map(|sites| (sites, elapsed))
}

/// The largest census file this will read into memory.
///
/// [`gamma_rt::write_reached`](gamma_rt) serializes each set bit of its
/// `MAX_CENSUS_SITES`-sized bitmap at most once, followed by protocol markers. Deriving the bound
/// from the bitmap, marker set, and wire-record type keeps those parts from drifting independently.
const CENSUS_MARKERS: [u32; 2] = [OVERFLOW, SEAL];
const CENSUS_RECORD_BYTES: u64 = core::mem::size_of::<u32>() as u64;
const MAX_CENSUS_BYTES: u64 = (gamma_rt::MAX_CENSUS_SITES as u64 + CENSUS_MARKERS.len() as u64) * CENSUS_RECORD_BYTES;

/// Census bytes whose size has been checked against the runtime protocol.
///
/// Keeping this state in the type prevents the decoder, which allocates capacity from the input
/// length, from accepting an unchecked byte slice when another production caller is added.
#[derive(Debug)]
struct BoundedCensus(Vec<u8>);

/// Why a census file could not be read within the runtime protocol boundary.
#[derive(Debug)]
enum CensusReadError {
    Io(io::Error),
    Oversized { actual: u64, maximum: u64 },
}

impl core::fmt::Display for CensusReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(cause) => cause.fmt(f),
            Self::Oversized { actual, maximum } => {
                write!(
                    f,
                    "the census file contains {actual} bytes, more than the runtime protocol maximum of {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for CensusReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(cause) => Some(cause),
            Self::Oversized { .. } => None,
        }
    }
}

impl From<io::Error> for CensusReadError {
    fn from(cause: io::Error) -> Self {
        Self::Io(cause)
    }
}

/// Reads a census file into memory, refusing anything past [`MAX_CENSUS_BYTES`].
///
/// The path this opens is disclosed to the censused test through `GAMMA_CENSUS`, and the
/// coordinator reading it back is long-lived, judging every mutant after this one — so a test that
/// replaced the runtime's short record stream with an arbitrarily large or sparse file must not be
/// able to make this allocate on the test's behalf. The cap is enforced by bounding the read itself
/// with [`std::io::Read::take`], independently of metadata: sparse files can have large logical
/// lengths while consuming little physical storage, and files can change after inspection.
fn read_bounded(path: &Utf8Path) -> Result<BoundedCensus, CensusReadError> {
    let file = fs::File::open(path.as_std_path())?;
    let mut bytes = Vec::new();

    // Probe beyond the accepted boundary so an exact-length file can be distinguished from a
    // truncated read of an oversized one.
    let _read = file.take(MAX_CENSUS_BYTES.saturating_add(1)).read_to_end(&mut bytes)?;

    if u64::try_from(bytes.len()).is_ok_and(|len| len > MAX_CENSUS_BYTES) {
        return Err(CensusReadError::Oversized {
            actual: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            maximum: MAX_CENSUS_BYTES,
        });
    }

    Ok(BoundedCensus(bytes))
}

/// Turns the runtime's records into site ordinals, or `None` if the file is not a whole census.
///
/// A census is trustworthy only if it ends with the runtime's [`SEAL`]: the file is an append-only
/// stream flushed as a prefix, so a seal intact at the end proves every record before it survived.
/// Everything else is refused — an unsealed file (a truncated exit write or a crash), a record
/// after the seal (a stale census appended to, or two runs sharing a path), and the [`OVERFLOW`]
/// marker (the runtime ran out of table) — because each means sites may be missing, and missing is
/// the one thing this must never guess at.
///
/// The [`BoundedCensus`] input proves the length-based preallocation below is within the protocol
/// bound.
fn decode(bytes: &BoundedCensus) -> Option<Vec<u32>> {
    let bytes = bytes.0.as_slice();

    if !bytes.len().is_multiple_of(4) {
        return None;
    }

    // #[gamma::skip(all, reason = "this expression is only an allocation-capacity hint; decoding pushes the same records and exposes the same result for every capacity")]
    let mut sites = Vec::with_capacity(bytes.len() / 4);
    let mut sealed = false;

    for record in bytes.chunks_exact(4) {
        // A record after the seal means the file is not the single clean prefix the runtime writes:
        // a stale census was appended to, or two runs shared the path. Either way it is untrusted.
        if sealed {
            return None;
        }

        match u32::from_le_bytes(record.try_into().ok()?) {
            OVERFLOW => return None,
            SEAL => sealed = true,
            ordinal => sites.push(ordinal),
        }
    }

    // No seal means the file is a prefix of unknown completeness, not a whole census.
    sealed.then_some(sites)
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::io::{Read, Write as _};

    use super::*;

    struct InterruptedThenData {
        interrupted: bool,
        data: std::io::Cursor<Vec<u8>>,
    }

    impl Read for InterruptedThenData {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }

            self.data.read(buf)
        }
    }

    struct DataThenError {
        data: Option<Vec<u8>>,
    }

    impl Read for DataThenError {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(data) = self.data.take() {
                buf[..data.len()].copy_from_slice(&data);
                return Ok(data.len());
            }

            Err(io::Error::other("truncated"))
        }
    }

    #[test]
    fn listing_reader_retries_interrupts_and_rejects_terminal_errors() {
        let mut interrupted = InterruptedThenData {
            interrupted: false,
            data: std::io::Cursor::new(b"whole".to_vec()),
        };
        assert_eq!(read_listing(&mut interrupted), (b"whole".to_vec(), true));

        let mut truncated = DataThenError {
            data: Some(b"prefix".to_vec()),
        };
        assert_eq!(read_listing(&mut truncated), (b"prefix".to_vec(), false));

        let mut exact = std::io::Cursor::new(vec![b'x'; LIST_CAP]);
        assert!(read_listing(&mut exact).1);
        let mut excessive = std::io::Cursor::new(vec![b'x'; LIST_CAP + 1]);
        assert!(!read_listing(&mut excessive).1);
    }

    #[test]
    fn listing_output_beyond_the_cap_is_not_authoritative() {
        let (_directory, work) =
            crate::testing::helper_workspace("census-list-cap", &["flood:4194305", "print:meaningful::killer: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper());

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    #[test]
    fn listing_output_exactly_at_the_cap_is_still_a_whole_stream() {
        let (_directory, work) = crate::testing::helper_workspace("census-list-cap-exact", &["flood:4194304", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper());

        assert_eq!(
            listed(command, Duration::from_secs(30)),
            Some(Vec::new()),
            "the byte cap is inclusive; only the first byte beyond it makes a listing partial"
        );
    }

    #[test]
    fn listing_reader_thread_failure_degrades_to_no_census() {
        let (_directory, work) = crate::testing::helper_workspace("census-list-thread", &["print:a::test: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper());
        let _refused = faults::arm(Fault::Thread);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    #[test]
    fn a_listing_child_that_cannot_be_adopted_degrades_to_no_census() {
        let (_directory, work) = crate::testing::helper_workspace("census-list-adopt", &["print:a::test: test", "exit:0"]);
        let command = listing_command(&work, &crate::testing::helper());
        let _refused = process_faults::arm(ProcessFault::Adopt);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    /// Production workspaces hold an exclusive scratch lock; adopted test workspaces do not.
    static WALK_TEST: Mutex<()> = Mutex::new(());

    /// Listing is a real launch and needs the same dynamic libraries as running a test.
    #[test]
    fn listing_a_binary_uses_the_runs_loader_path() {
        let (_scratch, work) = crate::testing::helper_workspace("list-loader", &["exit:0"]);
        let mut binary = crate::testing::helper();
        binary.manifest_dir = work.root().join("crate");
        let command = listing_command(&work, &binary);
        let arguments: Vec<_> = command.get_args().collect();
        let configured = command
            .get_envs()
            .find(|(name, _value)| *name == std::ffi::OsStr::new(LOADER_VAR))
            .and_then(|(_name, value)| value);
        let cache_home = command
            .get_envs()
            .find(|(name, _value)| *name == std::ffi::OsStr::new(crate::testing::CACHE_HOME_VAR))
            .and_then(|(_name, value)| value);
        let expected_cache_home = crate::testing::cache_home(work.root());

        assert_eq!(
            arguments.get(..2),
            Some([std::ffi::OsStr::new("--list"), std::ffi::OsStr::new("--format=terse")].as_slice())
        );
        assert_eq!(command.get_current_dir(), Some(binary.manifest_dir.as_std_path()));
        assert_eq!(
            configured,
            work.launch().loader.as_deref(),
            "the listing launch omitted the libraries used to build its executable"
        );
        assert_eq!(
            cache_home,
            expected_cache_home.as_deref().map(|path| path.as_std_path().as_os_str()),
            "the listing launch omitted the isolated cache home inherited by test processes"
        );
    }

    /// A binary that ignores the listing flags and never returns is abandoned, not waited out.
    ///
    /// This is what `harness = false` buys: a `fn main()` free to treat `--list` as noise and run
    /// its whole suite instead. Every other spawn in this subsystem is bounded, so a target like
    /// that would hang the one place with nothing watching it — and it would hang it before a
    /// single mutant had been judged, with no output and no way to tell what it was waiting for.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_never_answers_a_listing_is_abandoned_when_its_budget_runs_out() {
        let mut command = std::process::Command::new("/bin/sh");

        // Ignores its arguments and outlives any budget a listing could reasonably be given, which
        // is exactly the shape of the custom harness this bounds.
        let _ = command.args(["-c", "sleep 300"]);

        let started = Instant::now();
        let names = listed(command, Duration::from_millis(200));

        assert!(names.is_none(), "a binary that never listed was believed: {names:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the listing waited far past its budget: {:?}",
            started.elapsed()
        );
    }

    /// A session leader is outside the listing process group, so it retains the stdout pipe after
    /// the shell exits. The listing still has to return at its budget rather than joining a reader
    /// that can no longer be made to see EOF.
    #[test]
    #[cfg(unix)]
    fn an_escaped_descendant_holding_listing_stdout_does_not_outlive_the_listing_budget() {
        if !std::process::Command::new("setsid")
            .arg("true")
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping escaped-descendant fixture because `setsid` is unavailable");
            return;
        }

        let directory = crate::testing::workdir("census-escaped-listing-");
        let marker = Utf8PathBuf::from_path_buf(directory.path().join("escaped.pid")).expect("UTF-8 path");
        let mut command = std::process::Command::new("/bin/sh");
        let script = format!("setsid sh -c 'echo $$ > \"{marker}\"; sleep 5' & while [ ! -s \"{marker}\" ]; do sleep 0.01; done");

        let _ = command.args(["-c", &script]);

        let started = Instant::now();
        let names = listed(command, Duration::from_millis(100));

        for _attempt in 0..20 {
            if marker.as_std_path().exists() {
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        let pid = fs::read_to_string(marker.as_std_path()).expect("the escaped descendant started");
        let _stopped = std::process::Command::new("kill").args(["-TERM", pid.trim()]).status();

        assert!(names.is_none(), "an incomplete listing was accepted: {names:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the listing waited for an escaped pipe holder: {:?}",
            started.elapsed()
        );
    }

    /// A binary that answers in libtest's terse format is still read the way it always was.
    ///
    /// The bound and the containment are around the same question, so the answer to it has to be
    /// unchanged — a listing lost to the new machinery costs a binary its census and puts its whole
    /// suite behind every one of its mutants.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_answers_the_listing_is_read_as_the_tests_it_named() {
        let mut command = std::process::Command::new("/bin/sh");

        let _ = command.args(["-c", "printf 'suite::first: test\\nsuite::second: test\\n'"]);

        let names = listed(command, Duration::from_secs(30)).expect("the listing was answered");

        assert_eq!(names, vec!["suite::first".into(), "suite::second".into()]);
    }

    /// A binary that fails while listing contributes no names at all.
    ///
    /// A non-zero exit means whatever it printed is not an enumeration this can trust, and a
    /// partial enumeration is worse than none: a test believed not to exist is one no mutant is
    /// ever run against, which turns a listing failure into a silently inflated score.
    #[test]
    #[cfg(unix)]
    fn a_binary_that_fails_while_listing_is_not_read_as_the_tests_it_managed_to_print() {
        let mut command = std::process::Command::new("/bin/sh");

        let _ = command.args(["-c", "printf 'suite::first: test\\n'; exit 1"]);

        assert!(listed(command, Duration::from_secs(30)).is_none());
    }

    /// A listing sees only the tests the user's own filters allow.
    ///
    /// The census turns the listing into a selection the sweep later asks for by name, so a name
    /// discovered outside the user's filter is one the run would have to refuse — and, before the
    /// filters were composed at all, one it would have run. The helper answers with a name only
    /// when the filter reached it, so the assertion is about the arguments the listing carried.
    #[test]
    fn a_listing_is_narrowed_by_the_filters_the_user_gave() {
        const SCRIPT: &str = "when-arg:parser|print:suite::parser_works: test";

        let (_scratch, mut work) = crate::testing::helper_workspace("census-list-filtered", &[SCRIPT]);
        let binary = crate::testing::helper();

        work.set_test_args(vec![crate::testing::directive(SCRIPT), "parser".to_owned()]);

        assert_eq!(
            list(&work, &binary),
            Some(vec!["suite::parser_works".into()]),
            "the user's filter should have reached the listing"
        );

        // The same binary with no filter to carry prints nothing. An empty successful output is
        // inconclusive because a custom harness can ignore the listing flags and do the same.
        work.set_test_args(vec![crate::testing::directive(SCRIPT)]);

        assert_eq!(list(&work, &binary), None);
    }

    fn binary(path: &str) -> TestBinary {
        TestBinary {
            path: Utf8PathBuf::from(path),
            package: "p".to_owned(),
            package_id: String::new(),
            target: "t".to_owned(),
            manifest_dir: Utf8PathBuf::new(),
            linked_sources: None,
            baseline: Duration::from_secs(1),
            tests: None,
            budget: Some(Duration::from_secs(1)),
            peak: None,
            memory: None,
        }
    }

    #[test]
    fn an_uncensused_binary_is_not_the_same_as_one_that_reaches_nothing() {
        // The distinction the whole feature rests on. Reading `None` as "no test reaches this"
        // would report every mutant in an unlistable binary as uncovered without running one test.
        let mut census = Census::default();

        assert_eq!(census.reaching(&binary("/t/a"), 7), None);
        assert!(!census.is_complete_for(1));

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            Reach {
                complete: true,
                ..Reach::default()
            },
        );
        assert!(census.is_complete_for(1));

        assert_eq!(census.reaching(&binary("/t/a"), 7), Some(Vec::new()));
    }

    /// Builds a `Reach` from test names, a site-to-indices map, and per-test durations.
    fn reach_from(names: Vec<Box<str>>, sites: Vec<(u32, Vec<u32>)>, times: Vec<Duration>) -> Reach {
        let mut reach = Reach {
            names,
            reached: HashMap::default(),
            intern: Vec::new(),
            intern_index: HashMap::default(),
            times,
            complete: true,
        };

        for (site, indices) in sites {
            let set_index = reach.intern_set(indices);
            let _previous = reach.reached.insert(site, set_index);
        }
        reach.finish();

        reach
    }

    #[test]
    fn a_site_reports_the_tests_that_reached_it_and_no_others() {
        let mut census = Census::default();

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            reach_from(
                vec!["first".into(), "second".into(), "third".into()],
                vec![(7, vec![0, 2]), (9, vec![1])],
                vec![Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(3)],
            ),
        );

        assert_eq!(census.reaching(&binary("/t/a"), 9), Some(vec!["second"]));
        assert_eq!(census.reaching(&binary("/t/a"), 11), Some(Vec::new()));
        assert_eq!(census.len(), 1);
        assert!(
            census.binaries[&Utf8PathBuf::from("/t/a")].intern_index.is_empty(),
            "the construction-only interning index is discarded once reach is finalized"
        );

        // Two of three is past half, so the binary runs whole rather than being asked for most of
        // itself by name.
        assert_eq!(census.reaching(&binary("/t/a"), 7), None);
    }

    #[test]
    fn a_site_most_of_the_suite_reaches_is_not_worth_naming() {
        // The saving is what is left after the narrowing, and naming five of nine tests to skip
        // four is a longer command line for almost nothing.
        let mut census = Census::default();

        let names: Vec<Box<str>> = (0..9).map(|index| format!("test{index}").into()).collect();

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            reach_from(
                names,
                vec![(7, vec![0, 1, 2, 3, 4]), (9, vec![0, 1, 2, 3])],
                vec![Duration::from_secs(1); 9],
            ),
        );

        assert_eq!(census.reaching(&binary("/t/a"), 7), None);
        assert_eq!(census.reaching(&binary("/t/a"), 9).map(|tests| tests.len()), Some(4));
    }

    /// A census of `total` tests where `reached` of them reach site 7.
    fn suite_of(total: usize, reached: usize) -> Census {
        let mut census = Census::default();

        let names: Vec<Box<str>> = (0..total).map(|index| format!("test{index}").into()).collect();
        let indices: Vec<u32> = (0..reached).filter_map(|index| u32::try_from(index).ok()).collect();

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            reach_from(names, vec![(7, indices)], vec![Duration::from_secs(1); total]),
        );

        census
    }

    #[test]
    fn exactly_half_the_suite_is_still_worth_naming() {
        // The boundary itself, which decides whether a mutant runs four tests or forty. The rule is
        // `2 * reached > total`, so an even split is *not* past half and the narrowing is kept.
        // Testing only well inside each side would let the comparison drift to `>=` unnoticed,
        // which would silently widen every evenly-split binary to its whole suite.
        assert_eq!(suite_of(10, 5).reaching(&binary("/t/a"), 7).map(|tests| tests.len()), Some(5));
    }

    #[test]
    fn one_test_past_half_the_suite_is_not_worth_naming() {
        // The first input on the other side of the same boundary.
        assert_eq!(suite_of(10, 6).reaching(&binary("/t/a"), 7), None);
    }

    #[test]
    fn partial_reach_is_only_a_checked_hint() {
        let path = Utf8Path::new("/t/a");
        let binary = binary("/t/a");
        let census = Census::partial(path, 7, 2, 5);

        assert_eq!(
            census.selection(&binary, 7),
            CensusSelection::Hinted(vec!["tests::t0", "tests::t1"])
        );
        assert!(matches!(
            census.work(&binary, 7),
            CensusWork::Hinted(duration) if duration == Duration::from_millis(2)
        ));
        assert_eq!(census.selection(&binary, 8), CensusSelection::Whole);
    }

    #[test]
    fn census_must_cost_less_than_its_maximum_possible_savings() {
        assert!(can_repay(Duration::from_secs(1), Duration::from_secs(2)));
        assert!(!can_repay(Duration::from_secs(2), Duration::from_secs(2)));
        assert!(!can_repay(Duration::from_secs(3), Duration::from_secs(2)));
        assert!(!can_repay(Duration::ZERO, Duration::ZERO));
        assert_eq!(
            add_listing_cost(Duration::from_millis(3), Duration::from_millis(5), 4),
            Duration::from_millis(23)
        );
        assert_eq!(add_listing_cost(Duration::MAX, Duration::from_millis(5), 4), Duration::MAX);
    }

    #[test]
    fn scope_planning_prefers_module_hierarchy_and_falls_back_to_contiguous_chunks() {
        let hierarchical: Vec<Box<str>> = ["alpha::one", "alpha::two", "beta::nested::one", "beta::nested::two"]
            .into_iter()
            .map(Into::into)
            .collect();
        let shared_module_leaves: Vec<Box<str>> = ["tests::one", "tests::two"].into_iter().map(Into::into).collect();
        let flat: Vec<Box<str>> = (0..10).map(|index| format!("case{index:02}").into()).collect();

        assert_eq!(
            initial_scopes(&hierarchical),
            vec![Scope { tests: vec![0, 1] }, Scope { tests: vec![2, 3] }]
        );
        assert_eq!(
            initial_scopes(&shared_module_leaves),
            vec![Scope { tests: vec![0] }, Scope { tests: vec![1] }]
        );
        assert_eq!(
            initial_scopes(&flat),
            vec![
                Scope { tests: vec![0, 1, 2, 3] },
                Scope { tests: vec![4, 5, 6, 7] },
                Scope { tests: vec![8, 9] },
            ]
        );
        assert!(initial_scopes(&[]).is_empty());
        assert_eq!(initial_scopes(&["only".into()]), vec![Scope { tests: vec![0] }]);
        assert_eq!(
            initial_scopes(&["one".into(), "two".into()]),
            vec![Scope { tests: vec![0] }, Scope { tests: vec![1] }]
        );

        let nested: Vec<Box<str>> = ["root::alpha::one", "root::alpha::two", "root::beta::one"]
            .into_iter()
            .map(Into::into)
            .collect();
        let scope = Scope { tests: vec![0, 1, 2] };
        assert_eq!(
            module_children(&scope, &nested),
            Some(vec![Scope { tests: vec![0, 1] }, Scope { tests: vec![2] }])
        );
        assert_eq!(subdivision(&Scope { tests: vec![0] }, &nested), None);
    }

    #[test]
    fn subdivision_requires_a_clear_conservative_profit() {
        assert!(subdivision_repays(2, Duration::from_millis(5), Duration::from_secs(1), 50, 100, 2,));
        assert!(!subdivision_repays(2, Duration::from_millis(5), Duration::from_secs(1), 50, 0, 2,));
        assert!(!subdivision_repays(2, Duration::from_millis(5), Duration::from_secs(1), 50, 100, 0,));
        assert!(!subdivision_repays(
            2,
            Duration::from_millis(5),
            Duration::from_millis(20),
            50,
            100,
            2,
        ));
        assert!(!subdivision_repays(
            2,
            Duration::from_millis(5),
            Duration::from_millis(10),
            100,
            100,
            2,
        ));
        assert!(
            !subdivision_repays(2, Duration::from_millis(5), Duration::from_millis(20), 50, 100, 1),
            "an exact tie is not enough to pay for speculative subdivision"
        );

        assert_eq!(scale_duration(Duration::from_nanos(11), 3, 2), Duration::from_nanos(16));
        assert_eq!(scale_duration(Duration::from_nanos(11), 0, 2), Duration::ZERO);
        assert_eq!(
            scale_duration(Duration::from_nanos(11), 3, 0),
            Duration::from_nanos(33),
            "a zero denominator is conservatively clamped to one"
        );
        assert_eq!(
            scale_duration(Duration::MAX, usize::MAX, 1),
            Duration::from_nanos(u64::MAX),
            "an unrepresentable scaled duration saturates"
        );

        assert_eq!([0, 1, 2, 3, 4, 5, 15, 16, 17].map(ceil_sqrt), [0, 1, 2, 2, 2, 3, 4, 4, 5]);
        let wanted = HashSet::from_iter([1, 2, 3]);
        assert!(!mixed(&[], &wanted));
        assert!(!mixed(&[1], &wanted));
        assert!(mixed(&[1, 2], &wanted));
        assert!(!mixed(&[1, 2, 3], &wanted));

        let now = Instant::now();
        assert!(!deadline_reached(now, now + Duration::from_nanos(1)));
        assert!(deadline_reached(now, now));
        assert!(deadline_reached(now + Duration::from_nanos(1), now));

        let children = [Scope { tests: vec![0] }, Scope { tests: vec![1] }];
        let scope = Scope { tests: (0..50).collect() };
        let names: Vec<Box<str>> = (0..100).map(|index| format!("test{index}").into()).collect();
        assert!(should_subdivide(
            &children,
            &scope,
            &names,
            &[1, 2],
            &HashSet::from_iter([1, 2, 3]),
            Duration::from_millis(5),
            Duration::from_secs(1),
        ));
        assert!(!should_subdivide(
            &children,
            &scope,
            &names,
            &[1],
            &HashSet::from_iter([1, 2, 3]),
            Duration::from_millis(5),
            Duration::from_secs(1),
        ));

        let all = HashSet::from_iter([1, 2]);
        assert!(consistent_subdivision(true, true, &[1, 2], &all));
        assert!(!consistent_subdivision(true, false, &[1, 2], &all));
        assert!(!consistent_subdivision(true, true, &[1], &all));
        assert!(!consistent_subdivision(true, true, &[1, 3], &all));
    }

    #[test]
    fn census_read_errors_expose_io_causes_but_not_protocol_limits() {
        let io = CensusReadError::Io(io::Error::other("read refused"));
        assert_eq!(io.to_string(), "read refused");
        assert_eq!(
            std::error::Error::source(&io).map(ToString::to_string).as_deref(),
            Some("read refused")
        );

        let oversized = CensusReadError::Oversized {
            actual: MAX_CENSUS_BYTES + 1,
            maximum: MAX_CENSUS_BYTES,
        };
        assert!(oversized.to_string().contains("more than"), "{oversized}");
        assert!(std::error::Error::source(&oversized).is_none());
    }

    #[test]
    fn localized_modules_use_fewer_total_launches_than_whole_binary_and_per_case_plans() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-localized-", &[]);
        let binary = TestBinary {
            package: "subject".to_owned(),
            baseline: Duration::from_secs(1),
            ..crate::testing::helper()
        };
        let names: Vec<Box<str>> = (0..8)
            .flat_map(|module| (0..8).map(move |case| format!("module{module}::case{case}").into()))
            .collect();
        let targets = HashMap::from_iter([(binary.path.clone(), (0..100).collect::<HashSet<u32>>())]);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, selected, _path, _stall| {
                let module = selected[0]
                    .strip_prefix("module")
                    .and_then(|name| name.split_once("::"))
                    .and_then(|(number, _case)| number.parse::<u32>().ok())
                    .expect("the fixture names a numbered module");
                Some((vec![module], Duration::from_millis(1)))
            },
            || false,
            || {},
        );
        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };
        let sweep_launches = (0..100)
            .filter(|ordinal| !matches!(census.selection(&binary, *ordinal), CensusSelection::Uncovered))
            .count();
        let scoped = walked + sweep_launches;
        let whole_binary = 100;
        let per_case = 64 + sweep_launches;

        assert_eq!(walked, 8, "one launch per deterministic module group");
        assert_eq!(sweep_launches, 8, "only the localized sites still need a sweep launch");
        assert!(
            scoped < whole_binary,
            "{scoped} scoped launches did not beat {whole_binary} whole-binary launches"
        );
        assert!(
            scoped < per_case,
            "{scoped} scoped launches did not beat {per_case} per-case launches"
        );
    }

    #[test]
    fn a_malformed_group_keeps_other_positive_observations_as_hints_only() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-malformed-group-", &[]);
        let binary = binary("/t/a");
        let names: Vec<Box<str>> = (0..4).map(|index| format!("case{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([1, 2]))]);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, selected, _path, _stall| {
                selected
                    .first()
                    .is_some_and(|name| *name == "case0")
                    .then_some((vec![1], Duration::from_millis(7)))
            },
            || false,
            || {},
        );
        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert_eq!(walked, 2);
        assert_eq!(census.selection(&binary, 1), CensusSelection::Hinted(vec!["case0", "case1"]));
        assert_eq!(census.selection(&binary, 2), CensusSelection::Whole);
        let reach = &census.binaries[&binary.path];
        assert_eq!(
            reach.times,
            vec![
                Duration::from_micros(3500),
                Duration::from_micros(3500),
                Duration::ZERO,
                Duration::ZERO
            ]
        );
        assert!(reach.intern_index.is_empty());
    }

    #[test]
    fn inconsistent_subdivision_cannot_prove_absence() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-nondeterministic-group-", &[]);
        let mut binary = binary("/t/a");
        binary.baseline = Duration::from_secs(10);
        let names: Vec<Box<str>> = (0..4).map(|index| format!("case{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([1, 2, 3]))]);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, selected, _path, _stall| {
                if selected.len() == 2 && selected[0] == "case0" {
                    Some((vec![1, 2], Duration::ZERO))
                } else if selected.len() == 1 && selected[0] == "case0" {
                    Some((vec![1], Duration::ZERO))
                } else {
                    Some((Vec::new(), Duration::ZERO))
                }
            },
            || false,
            || {},
        );
        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert_eq!(walked, 4, "two root chunks and two attempted child samples");
        assert_eq!(census.selection(&binary, 1), CensusSelection::Hinted(vec!["case0", "case1"]));
        assert_eq!(census.selection(&binary, 2), CensusSelection::Hinted(vec!["case0", "case1"]));
        assert_eq!(census.selection(&binary, 3), CensusSelection::Whole);
    }

    #[test]
    fn planner_filters_sorts_and_deduplicates_one_terminal_scope() {
        let (_directory, work) = crate::testing::helper_workspace("census-planner-terminal-", &[]);
        let binary = binary("/t/a");
        let names: Vec<Box<str>> = vec!["tests::one".into()];
        let wanted = HashSet::from_iter([2, 7]);
        let sampler = |_work: &Workspace, _binary: &TestBinary, selected: &[&str], _path: &Utf8Path, _stall: Stall| {
            assert_eq!(selected, ["tests::one"]);
            Some((vec![9, 7, 2, 7, 9], Duration::from_millis(13)))
        };
        let expired = || false;
        let mut planner = Planner {
            work: &work,
            binary: &binary,
            names: &names,
            wanted: &wanted,
            path: Utf8Path::new("/unused"),
            stall: Stall::NONE,
            launch_cost: MIN_SCOUT_WAIT,
            sampler: &sampler,
            expired: &expired,
            launches: 0,
            complete: true,
        };

        let evidence = planner.explore(Scope { tests: vec![0] });

        assert_eq!(planner.launches, 1);
        assert!(planner.complete);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].scope.tests, vec![0]);
        assert_eq!(evidence[0].sites, vec![2, 7]);
        assert_eq!(evidence[0].elapsed, Duration::from_millis(13));
    }

    #[test]
    fn planner_keeps_consistent_child_evidence_instead_of_the_parent() {
        let (_directory, work) = crate::testing::helper_workspace("census-planner-children-", &[]);
        let mut binary = binary("/t/a");
        binary.baseline = Duration::from_secs(10);
        let names: Vec<Box<str>> = (0..4).map(|index| format!("case{index}").into()).collect();
        let wanted = HashSet::from_iter([1, 2, 3]);
        let sampler = |_work: &Workspace, _binary: &TestBinary, selected: &[&str], _path: &Utf8Path, _stall: Stall| {
            let sites = if selected.len() == 4 {
                vec![1, 2]
            } else if selected[0] == "case0" {
                vec![1]
            } else {
                vec![2]
            };
            Some((sites, Duration::from_millis(u64::try_from(selected.len()).expect("small fixture"))))
        };
        let expired = || false;
        let mut planner = Planner {
            work: &work,
            binary: &binary,
            names: &names,
            wanted: &wanted,
            path: Utf8Path::new("/unused"),
            stall: Stall::NONE,
            launch_cost: MIN_SCOUT_WAIT,
            sampler: &sampler,
            expired: &expired,
            launches: 0,
            complete: true,
        };

        let evidence = planner.explore(Scope { tests: vec![0, 1, 2, 3] });

        assert!(planner.complete);
        assert_eq!(planner.launches, 3);
        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].scope.tests, vec![0, 1]);
        assert_eq!(evidence[0].sites, vec![1]);
        assert_eq!(evidence[1].scope.tests, vec![2, 3]);
        assert_eq!(evidence[1].sites, vec![2]);
    }

    #[test]
    fn the_bail_out_boundary_sits_at_the_same_place_for_an_odd_suite() {
        // An odd suite has no exact half, so the last kept case is the largest count under it.
        assert_eq!(suite_of(11, 5).reaching(&binary("/t/a"), 7).map(|tests| tests.len()), Some(5));
        assert_eq!(suite_of(11, 6).reaching(&binary("/t/a"), 7), None);
    }

    #[test]
    fn a_site_only_one_test_reaches_is_always_worth_naming() {
        // The case the census exists for: one test out of a large suite, which is the whole
        // dividend the per-test measurement is paid for.
        assert_eq!(suite_of(1000, 1).reaching(&binary("/t/a"), 7).map(|tests| tests.len()), Some(1));
    }

    #[test]
    fn an_index_naming_no_test_is_dropped_rather_than_panicking() {
        // Only reachable through a corrupt census, and a run that dies reading one is worse than a
        // run that maps a site to one test instead of two.
        let mut census = Census::default();

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            reach_from(
                vec!["first".into(), "second".into(), "third".into(), "fourth".into()],
                vec![(7, vec![0, 40])],
                vec![Duration::from_secs(1); 4],
            ),
        );

        assert_eq!(census.reaching(&binary("/t/a"), 7), Some(vec!["first"]));
    }

    #[test]
    fn estimate_uses_measured_tests_and_keeps_whole_and_uncovered_distinct() {
        let mut census = Census::default();

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            reach_from(
                vec!["first".into(), "second".into(), "third".into()],
                vec![(7, vec![0]), (9, vec![0, 1])],
                vec![Duration::from_secs(2), Duration::from_secs(3), Duration::from_secs(5)],
            ),
        );

        assert!(matches!(
            census.work(&binary("/t/a"), 7),
            CensusWork::Selected(duration) if duration == Duration::from_secs(2)
        ));
        assert!(matches!(census.work(&binary("/t/a"), 8), CensusWork::Uncovered));
        assert!(matches!(census.work(&binary("/t/a"), 9), CensusWork::Whole));
        assert!(matches!(census.work(&binary("/t/b"), 7), CensusWork::Whole));

        let partial = Census::partial(Utf8Path::new("/t/partial"), 7, 2, 5);
        assert!(matches!(
            partial.work(&binary("/t/partial"), 7),
            CensusWork::Hinted(duration) if duration == Duration::from_millis(2)
        ));
        assert!(matches!(partial.work(&binary("/t/partial"), 8), CensusWork::Whole));
    }

    fn decode_fixture(bytes: &[u8]) -> Option<Vec<u32>> {
        decode(&BoundedCensus(bytes.to_vec()))
    }

    #[test]
    fn whole_sealed_records_decode_in_the_order_they_were_written() {
        assert_eq!(decode_fixture(&sealed(&[3, 9, 1])), Some(vec![3, 9, 1]));
    }

    #[test]
    fn a_lone_seal_is_an_empty_reach_not_a_failure() {
        // The census of a test that reached no site: the runtime still sealed it, so it decodes to
        // an honest empty reach rather than being mistaken for a run that never finished.
        assert_eq!(decode_fixture(&sealed(&[])), Some(Vec::new()));
    }

    #[test]
    fn an_unsealed_census_is_refused_even_when_its_records_are_whole() {
        // Aligned, parseable, every record a real site — but no seal. That is exactly the shape a
        // truncated or aborted run leaves behind, so it must not be read as the reach it resembles;
        // the empty file is the same story with nothing written before the process died.
        let whole: Vec<u8> = [3_u32, 9, 1].iter().flat_map(|site| site.to_le_bytes()).collect();

        assert_eq!(decode_fixture(&whole), None);
        assert_eq!(decode_fixture(&[]), None);
    }

    #[test]
    fn a_census_cut_short_during_its_exit_write_is_refused() {
        // A batch can have reached the file before a later exit write fails. With no trailing seal,
        // this aligned prefix must not be mistaken for a complete one-site census.
        let prefix = 73_u32.to_le_bytes();

        assert_eq!(decode_fixture(&prefix), None);
    }

    #[test]
    fn a_truncated_census_is_refused_rather_than_read_as_far_as_it_goes() {
        // A partial trailing record means the process died before its buffer was flushed, so sites
        // are missing — and a missing site is exactly what would be misread as an unreached one.
        assert_eq!(decode_fixture(&[1, 0, 0]), None);
        assert_eq!(decode_fixture(&[1, 0, 0, 0, 2]), None);

        let mut sealed_with_partial_record = sealed(&[]);
        sealed_with_partial_record.push(1);
        assert_eq!(
            decode_fixture(&sealed_with_partial_record),
            None,
            "a seal before a partial trailing record does not make the prefix trustworthy"
        );
    }

    #[test]
    fn anything_after_the_seal_discards_the_census() {
        // A record past the seal means the file is not one clean prefix: a stale census was appended
        // to, or two runs shared the path. Neither is this run's whole and only story.
        let trailing: Vec<u8> = [SEAL, 7].iter().flat_map(|record| record.to_le_bytes()).collect();
        let doubled: Vec<u8> = [SEAL, SEAL].iter().flat_map(|record| record.to_le_bytes()).collect();

        assert_eq!(decode_fixture(&trailing), None);
        assert_eq!(decode_fixture(&doubled), None);
    }

    #[test]
    fn the_overflow_marker_discards_the_whole_census_even_when_it_is_sealed() {
        // The runtime seals a census it overflowed just like any other clean exit, so the marker
        // has to override the seal rather than be excused by it.
        let bytes: Vec<u8> = [3_u32, OVERFLOW, 9, SEAL].iter().flat_map(|record| record.to_le_bytes()).collect();

        assert_eq!(decode_fixture(&bytes), None);
    }

    /// The independently derived wire-format boundary is accepted and any excess is refused.
    ///
    /// Deriving the fixture without [`MAX_CENSUS_BYTES`] makes a regression that shrinks the
    /// production bound observable instead of shrinking the test input with it.
    #[test]
    fn read_bounded_accepts_the_protocol_maximum_and_refuses_excess() {
        let scratch = crate::testing::workdir("sec2-oversized");
        let path = Utf8PathBuf::from_path_buf(scratch.path().join("census.bin")).expect("a temp path is valid UTF-8");
        let protocol_maximum = (gamma_rt::MAX_CENSUS_SITES + CENSUS_MARKERS.len()) * core::mem::size_of::<u32>();

        fs::write(path.as_std_path(), vec![0_u8; protocol_maximum]).expect("the boundary fixture file is writable");
        let accepted = read_bounded(&path).expect("the protocol maximum is accepted");

        assert_eq!(accepted.0.len(), protocol_maximum);

        fs::OpenOptions::new()
            .append(true)
            .open(path.as_std_path())
            .expect("the boundary fixture remains writable")
            .write_all(&[0, 0])
            .expect("excess bytes are appended");
        let error = read_bounded(&path).expect_err("a file past the protocol maximum must be refused");
        assert!(error.to_string().contains("protocol"), "{error}");
        assert!(matches!(
            error,
            CensusReadError::Oversized {
                actual,
                maximum
            } if actual == u64::try_from(protocol_maximum + 1).expect("the protocol fixture fits u64")
                && maximum == u64::try_from(protocol_maximum).expect("the protocol fixture fits u64")
        ));
    }

    /// A sparse file can have a large logical length while consuming little physical storage.
    ///
    /// The fixture is comfortably beyond the protocol bound, so rejection demonstrates that the
    /// reader consumes only its bounded prefix rather than allocating for the file's logical size.
    #[test]
    fn read_bounded_refuses_a_sparse_file_with_a_large_logical_length() {
        const SPARSE_LOGICAL_LENGTH: u64 = 64 * 1024 * 1024;

        let scratch = crate::testing::workdir("sec2-sparse");
        let path = Utf8PathBuf::from_path_buf(scratch.path().join("census.bin")).expect("a temp path is valid UTF-8");

        const {
            assert!(SPARSE_LOGICAL_LENGTH > MAX_CENSUS_BYTES);
        }
        let file = fs::File::create(path.as_std_path()).expect("the sparse fixture file is creatable");
        file.set_len(SPARSE_LOGICAL_LENGTH)
            .expect("the filesystem under the test work directory supports sparse files");
        drop(file);

        let error = read_bounded(&path).expect_err("a sparse file past the protocol's maximum must be refused");

        assert!(error.to_string().contains("protocol"), "{error}");
    }

    /// `walked` counts sample subprocesses that actually launched, not tests that were listed.
    ///
    /// Grouped sampling can cover several listed tests with one launch. Summing the list lengths up
    /// front would therefore overstate the census's cost. This drives three listed tests through
    /// two initial scopes, one of which spoils the binary, and asserts that the two launches are
    /// counted even though the incomplete binary contributes no exact reach.
    #[test]
    fn walked_counts_launches_that_happened_not_tests_that_were_listed() {
        // The first scope includes `spoiler` and exits non-zero. The second writes a lone seal — a
        // valid, empty census — and passes, but the incomplete binary is still discarded.
        const SCRIPT: &[&str] = &["when-arg:spoiler|exit:1", "write-le:GAMMA_CENSUS|4294967293"];

        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");

        // A tree of its own, so the census scratch directory `walk` derives from `base()` cannot
        // collide with another test's.
        let (_directory, mut work) = crate::testing::helper_workspace("cor13-walked", SCRIPT);
        isolate_census_base(&mut work);

        let binary = TestBinary {
            package: "subject".to_owned(),
            baseline: Duration::from_millis(1),
            budget: Some(Duration::from_mins(1)),
            ..crate::testing::helper()
        };

        let names: Vec<Box<str>> = vec!["reaches".into(), "spoiler".into(), "after".into()];
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);

        let mut completed = 0;
        let (mapped, walked) = walk(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            None,
            1,
            Stall::NONE,
            || completed += 1,
        );

        assert_eq!(walked, 2, "three listed tests were covered by two launched sample scopes");
        assert_eq!(completed, 1, "the spoiled binary completed exactly once");
        assert!(mapped.is_empty(), "a spoiled binary contributes no reach at all");
    }

    #[test]
    fn grouped_sampling_stops_at_coarse_scopes_when_every_site_is_common() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-saturation-", &[]);
        let binary = TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::helper()
        };
        let names: Vec<Box<str>> = (0..5).map(|index| format!("tests::t{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);
        let sampled = AtomicUsize::new(0);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, _name, _path, _stall| {
                let _ = sampled.fetch_add(1, Ordering::Relaxed);
                Some((vec![7], Duration::ZERO))
            },
            || false,
            || {},
        );

        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert_eq!(sampled.load(Ordering::Relaxed), 2);
        assert_eq!(walked, 2);
        assert_eq!(census.selection(&binary, 7), CensusSelection::Whole);
    }

    #[test]
    fn grouped_scope_cost_is_distributed_across_tests_without_multiplying_it() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-group-cost-", &[]);
        let mut binary = TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::helper()
        };
        binary.baseline = Duration::from_secs(10);
        let names: Vec<Box<str>> = (0..4).map(|index| format!("tests::t{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, selected, _path, _stall| {
                let sites = selected
                    .first()
                    .is_some_and(|name| *name == "tests::t0")
                    .then_some(7)
                    .into_iter()
                    .collect();
                Some((sites, Duration::from_millis(10)))
            },
            || false,
            || {},
        );
        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert!(matches!(
            census.work(&binary, 7),
            CensusWork::Selected(duration) if duration == Duration::from_millis(10)
        ));
    }

    /// A sampler that reports the same site several times for one test must count as a single
    /// reach toward saturation, not several: `intern_set` removes duplicates from the final list
    /// either way, so a miscount here can only be caught by the saturation threshold itself, not
    /// by inspecting the finished census. If the site count were bumped once per duplicate entry
    /// instead of once per newly-recorded test, this one test alone (contributing 5 duplicate
    /// entries against a 5-test suite) would trip saturation on its own, cutting the walk short
    /// after a single sample instead of after the 3 distinct tests genuine saturation requires.
    #[test]
    fn a_duplicated_site_within_one_sample_does_not_inflate_the_saturation_count() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-dup-site-", &[]);
        let binary = TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::helper()
        };
        let names: Vec<Box<str>> = (0..5).map(|index| format!("tests::t{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([42]))]);
        let sampled = AtomicUsize::new(0);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, _name, _path, _stall| {
                let _ = sampled.fetch_add(1, Ordering::Relaxed);
                // Every sample reports site 42 five times over, as a pathological sampler might.
                Some((vec![42, 42, 42, 42, 42], Duration::ZERO))
            },
            || false,
            || {},
        );

        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert_eq!(sampled.load(Ordering::Relaxed), 2);
        assert_eq!(walked, 2);
        assert_eq!(census.selection(&binary, 42), CensusSelection::Whole);
    }

    /// The same duplicate-site sampler under many concurrent workers: the exact sample at which
    /// saturation is observed can vary with scheduling (workers may race past the check before the
    /// flag is visible), but the count each worker contributes per test can never be inflated by a
    /// sampler repeating one site, and the finished census must still agree that the site demands
    /// the whole binary.
    #[test]
    fn a_duplicated_site_within_one_sample_stays_race_free_across_many_workers() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-dup-site-race-", &[]);
        let binary = TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::helper()
        };
        let names: Vec<Box<str>> = (0..5).map(|index| format!("tests::t{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([42]))]);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            8,
            Stall::NONE,
            |_work, _binary, _name, _path, _stall| Some((vec![42, 42, 42, 42, 42], Duration::from_millis(1))),
            || false,
            || {},
        );

        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert_eq!(walked, 2);
        assert_eq!(census.selection(&binary, 42), CensusSelection::Whole);
    }

    /// Many workers walking the same suite reach exactly the same census as one worker would.
    ///
    /// Each worker now accumulates its own observations locally instead of contending for one
    /// shared, per-binary lock, and the merge afterwards trusts that no two workers ever produced
    /// the same `(site, test)` observation. A race in that assumption could only show up as an
    /// observation silently dropped (under-counted) or duplicated (over-counted) once several workers
    /// are actually racing each other over the same binary — which is exactly what running one
    /// fixed, deterministic suite through a single worker and then through several checks for.
    #[test]
    fn many_workers_reach_the_same_census_as_one_worker() {
        const TESTS: usize = 40;
        const SITES: u32 = 10;

        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, work) = crate::testing::helper_workspace("census-race-", &[]);
        let binary = TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::helper()
        };

        let names: Vec<Box<str>> = (0..TESTS).map(|index| format!("tests::t{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), (0..SITES).collect::<HashSet<u32>>())]);

        // Each test deterministically reaches a handful of sites, purely as a function of its own
        // index — never of which worker happens to run it or when — so any disagreement between the
        // one-worker and many-worker runs can only come from the merge itself.
        let sampler = |_work: &Workspace, _binary: &TestBinary, names: &[&str], _path: &Utf8Path, _stall: Stall| {
            let index: u32 = names[0]
                .strip_prefix("tests::t")
                .and_then(|digits| digits.parse().ok())
                .expect("these test names are always `tests::t<index>`");
            let sites: Vec<u32> = (0..SITES).filter(|site| (index + site).is_multiple_of(3)).collect();

            Some((sites, Duration::from_millis(1)))
        };

        let run = |jobs: usize| {
            let (mapped, walked) = walk_with(
                &work,
                vec![(&binary, names.clone(), MIN_SCOUT_WAIT)],
                &targets,
                jobs,
                Stall::NONE,
                sampler,
                || false,
                || {},
            );

            (
                Census {
                    binaries: HashMap::from_iter(mapped),
                    walked,
                    ..Census::default()
                },
                walked,
            )
        };

        let (single, single_walked) = run(1);
        let (many, many_walked) = run(8);

        assert_eq!(single_walked, 6);
        assert_eq!(
            single_walked, many_walked,
            "the same fixed suite launches the same number of samples regardless of worker count"
        );

        for site in 0..SITES {
            assert_eq!(
                single.reaching(&binary, site),
                many.reaching(&binary, site),
                "site {site} disagrees between one worker and many"
            );
        }
    }

    #[test]
    fn the_budget_keeps_completed_samples_as_hints_without_trusting_missing_ones() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, mut work) = crate::testing::helper_workspace("census-budget-", &[]);
        isolate_census_base(&mut work);
        let binary = TestBinary {
            package: "subject".to_owned(),
            ..crate::testing::helper()
        };
        let names: Vec<Box<str>> = (0..8).map(|index| format!("tests::t{index}").into()).collect();
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7, 8]))]);
        let spent = AtomicBool::new(false);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, names, MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, _name, _path, _stall| {
                spent.store(true, Ordering::Relaxed);
                Some((vec![7], Duration::from_millis(20)))
            },
            || spent.load(Ordering::Relaxed),
            || {},
        );

        let census = Census {
            binaries: HashMap::from_iter(mapped),
            walked,
            ..Census::default()
        };

        assert_eq!(walked, 1);
        assert_eq!(
            census.selection(&binary, 7),
            CensusSelection::Hinted(vec!["tests::t0", "tests::t1", "tests::t2"])
        );
        assert_eq!(census.selection(&binary, 8), CensusSelection::Whole);
    }

    #[test]
    fn a_partial_stream_note_raised_by_a_census_worker_reaches_the_run() {
        crate::notes::alone(|| {
            let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
            let (_directory, work) = crate::testing::helper_workspace("census-worker-notes-", &[]);
            let binary = TestBinary {
                package: "subject".to_owned(),
                ..crate::testing::helper()
            };
            let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);

            let (_mapped, walked) = walk_with(
                &work,
                vec![(&binary, vec!["partial".into()], MIN_SCOUT_WAIT)],
                &targets,
                1,
                Stall::NONE,
                |_work, _binary, _name, _path, _stall| {
                    crate::notes::note("census sample produced a partial stream");
                    None
                },
                || false,
                || {},
            );

            assert_eq!(walked, 1);
            assert_eq!(crate::notes::drain(), ["census sample produced a partial stream"]);
        });
    }

    /// A clean run that leaves no census spoils its binary, rather than reading as empty.
    ///
    /// An absent census file is what a failed `fopen` — or a process that died before it could seal
    /// — leaves behind. With mandatory sealing, "reached nothing" is a lone seal, not an absent
    /// file, so the absence can only mean the census never completed. Reading it as an empty reach
    /// would tell the sweep that no test convicts the mutants at these sites, turning a lost
    /// measurement into a false "survived".
    #[test]
    fn a_clean_run_that_leaves_no_census_spoils_rather_than_reading_empty() {
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");

        // Exits cleanly and writes nothing whatsoever to the census path.
        let (_directory, work) = crate::testing::helper_workspace("cor4-absent", &["exit:0"]);

        let binary = TestBinary {
            package: "subject".to_owned(),
            baseline: Duration::from_millis(1),
            budget: Some(Duration::from_mins(1)),
            ..crate::testing::helper()
        };

        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);
        let mut completed = 0;
        let (mapped, walked) = walk(
            &work,
            vec![(&binary, vec!["quiet".into()], MIN_SCOUT_WAIT)],
            &targets,
            None,
            1,
            Stall::NONE,
            || {
                completed += 1;
            },
        );

        assert_eq!(walked, 1, "the one test was launched");
        assert_eq!(completed, 1, "the spoiled binary completed exactly once");
        assert!(
            mapped.is_empty(),
            "an absent census spoils the binary rather than reading as an empty reach"
        );
    }

    /// The bytes the runtime writes for `sites`, ending with the seal that vouches the file is whole.
    fn sealed(sites: &[u32]) -> Vec<u8> {
        sites
            .iter()
            .chain(core::iter::once(&SEAL))
            .flat_map(|record| record.to_le_bytes())
            .collect()
    }

    /// Identical reach sets are interned to a single allocation, reducing memory.
    #[test]
    fn identical_reach_sets_are_interned() {
        let mut reach = Reach {
            names: vec!["a".into(), "b".into(), "c".into()],
            times: vec![Duration::from_secs(1); 3],
            ..Reach::default()
        };

        // Two sites with the same reaching tests share one intern slot.
        let idx1 = reach.intern_set(vec![0, 2, 2, 0]);
        let idx2 = reach.intern_set(vec![2, 0]); // same set, different insertion order

        assert_eq!(idx1, idx2, "identical sets should map to the same intern index");
        assert_eq!(reach.intern.len(), 1, "only one interned set should exist");
        assert_eq!(reach.intern[0].as_ref(), &[0, 2], "interned sets are sorted and deduplicated");
    }

    /// Different reach sets get distinct intern slots.
    #[test]
    fn different_reach_sets_are_distinct() {
        let mut reach = Reach {
            names: vec!["a".into(), "b".into(), "c".into()],
            times: vec![Duration::from_secs(1); 3],
            ..Reach::default()
        };

        let idx1 = reach.intern_set(vec![0, 1]);
        let idx2 = reach.intern_set(vec![0, 2]);

        assert_ne!(idx1, idx2);
        assert_eq!(reach.intern.len(), 2);
    }

    /// The interned reach representation produces the same `reaching` answers as raw vectors.
    #[test]
    fn interned_reach_answers_match_raw_vectors() {
        let mut census = Census::default();

        let _previous = census.binaries.insert(
            Utf8PathBuf::from("/t/a"),
            reach_from(
                vec!["alpha".into(), "beta".into(), "gamma".into(), "delta".into(), "epsilon".into()],
                vec![(1, vec![0, 2]), (2, vec![0, 2]), (3, vec![1])],
                vec![Duration::from_millis(10); 5],
            ),
        );

        // Sites 1 and 2 share a reach set; site 3 has its own.
        assert_eq!(census.reaching(&binary("/t/a"), 1), Some(vec!["alpha", "gamma"]));
        assert_eq!(census.reaching(&binary("/t/a"), 2), Some(vec!["alpha", "gamma"]));
        assert_eq!(census.reaching(&binary("/t/a"), 3), Some(vec!["beta"]));
        assert_eq!(census.reaching(&binary("/t/a"), 4), Some(Vec::new()));

        // Verify the underlying interning saves memory.
        let reach = &census.binaries[&Utf8PathBuf::from("/t/a")];
        assert_eq!(reach.intern.len(), 2, "sites 1 and 2 share one interned set");
    }

    #[test]
    fn durable_reach_clusters_intern_identical_test_sets() {
        let binary = binary("/t/a");
        let duplicate_binary = self::binary("/t/b");
        let mut census = Census::default();
        let _previous = census.binaries.insert(
            binary.path.clone(),
            reach_from(
                vec!["tests::a".into(), "tests::b".into()],
                vec![(1, vec![0]), (2, vec![0])],
                vec![Duration::from_millis(1); 2],
            ),
        );
        let _previous = census.binaries.insert(
            duplicate_binary.path.clone(),
            reach_from(
                vec!["tests::a".into(), "tests::b".into()],
                vec![(1, vec![0]), (2, vec![0])],
                vec![Duration::from_millis(1); 2],
            ),
        );
        let mut first = crate::fixtures::mutant();
        first.ordinal = 1;
        first.occurrence = 1;
        let mut second = first.clone();
        second.ordinal = 2;
        second.occurrence = 2;
        let mut hints = GeneralizedHints::empty_supported();
        hints.version = 0;

        census.persist_clusters(&[first, second], &[binary, duplicate_binary], &mut hints);

        assert_eq!(hints.version, GENERALIZED_HINTS_VERSION);
        assert_eq!(hints.test_sets.len(), 1);
        assert_eq!(hints.reach.len(), 2);
        assert_eq!(hints.reach[0].test_set, hints.reach[1].test_set);
        assert_eq!(
            hints.test_sets[0].len(),
            1,
            "the same durable killer from two binaries is deduplicated"
        );
    }

    #[test]
    fn durable_reach_clusters_sort_by_stable_site_digest_before_occurrence() {
        let binary = binary("/t/a");
        let mut census = Census::default();
        let _previous = census.binaries.insert(
            binary.path.clone(),
            reach_from(
                vec!["tests::a".into(), "tests::unused".into()],
                vec![(1, vec![0]), (2, vec![0]), (3, vec![0])],
                vec![Duration::from_millis(1); 2],
            ),
        );
        let mut later_text = crate::fixtures::mutant();
        later_text.ordinal = 1;
        later_text.original = "z + 1".into();
        later_text.occurrence = 0;
        let mut later_occurrence = later_text.clone();
        later_occurrence.ordinal = 2;
        later_occurrence.original = "a /* formatting */ + 1".into();
        later_occurrence.occurrence = 1;
        let mut earlier_occurrence = later_occurrence.clone();
        earlier_occurrence.ordinal = 3;
        earlier_occurrence.occurrence = 0;
        let mut expected: Vec<(String, u32)> = [&later_text, &later_occurrence, &earlier_occurrence]
            .into_iter()
            .map(|mutant| {
                let site = SiteIdentity::from_mutant(mutant);
                (site.normalized_text, site.occurrence)
            })
            .collect();
        expected.sort();
        let mut hints = GeneralizedHints::empty_supported();

        census.persist_clusters(&[later_text, later_occurrence, earlier_occurrence], &[binary], &mut hints);

        let order: Vec<(String, u32)> = hints
            .reach
            .iter()
            .map(|cluster| (cluster.site.normalized_text.clone(), cluster.site.occurrence))
            .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn durable_reach_clusters_are_bounded_and_preserve_untouched_sites() {
        let mut binary = binary("/t/a");
        binary.baseline = Duration::from_millis(10);
        let mut census = Census::default();
        let _previous = census.binaries.insert(
            binary.path.clone(),
            reach_from(
                vec!["tests::a".into(), "tests::b".into(), "tests::c".into(), "tests::d".into()],
                vec![(1, vec![0, 1, 2]), (2, vec![0])],
                vec![Duration::from_millis(1), Duration::ZERO, Duration::ZERO, Duration::ZERO],
            ),
        );
        let mut expensive = crate::fixtures::mutant();
        expensive.ordinal = 1;
        expensive.item_path = "subject::expensive".into();
        let mut useful = expensive.clone();
        useful.ordinal = 2;
        useful.item_path = "subject::useful".into();
        let untouched_site = SiteIdentity {
            file: "src/untouched.rs".into(),
            item: "untouched::item".to_owned(),
            mutator: "lit.true_to_false".to_owned(),
            normalized_text: "true".to_owned(),
            occurrence: 0,
        };
        let untouched = vec![
            Killer {
                package: "z-package".to_owned(),
                target: "z-tests".to_owned(),
                test: "tests::z".to_owned(),
            },
            Killer {
                package: "a-package".to_owned(),
                target: "a-tests".to_owned(),
                test: "tests::a".to_owned(),
            },
        ];
        let previous_expensive = vec![Killer {
            package: "old-package".to_owned(),
            target: "old-tests".to_owned(),
            test: "tests::old".to_owned(),
        }];
        let mut hints = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            test_sets: vec![untouched, previous_expensive],
            reach: vec![
                ReachCluster {
                    site: untouched_site.clone(),
                    test_set: 0,
                },
                ReachCluster {
                    site: SiteIdentity {
                        file: "src/missing-set.rs".into(),
                        ..untouched_site.clone()
                    },
                    test_set: 99,
                },
                ReachCluster {
                    site: SiteIdentity::from_mutant(&expensive),
                    test_set: 1,
                },
            ],
            ..GeneralizedHints::empty_supported()
        };

        census.persist_clusters(&[expensive, useful.clone()], &[binary], &mut hints);

        assert!(hints.reach.iter().any(|cluster| cluster.site == untouched_site));
        let untouched_set = hints
            .reach
            .iter()
            .find(|cluster| cluster.site == untouched_site)
            .and_then(|cluster| hints.test_sets.get(cluster.test_set as usize))
            .expect("the untouched set remains interned");
        assert_eq!(untouched_set[0].package, "a-package");
        assert!(hints.reach.iter().any(|cluster| cluster.site == SiteIdentity::from_mutant(&useful)));
        let previous = hints
            .reach
            .iter()
            .find(|cluster| cluster.site.item == "subject::expensive")
            .and_then(|cluster| hints.test_sets.get(cluster.test_set as usize))
            .expect("a site without usable replacement evidence keeps its prior hint");
        assert_eq!(previous[0].test, "tests::old");
    }

    #[test]
    fn durable_reach_clusters_discard_stale_and_unprofitable_records() {
        let mut binary = binary("/t/a");
        binary.baseline = Duration::from_millis(1);
        let mut reach = reach_from(
            vec!["tests::a".into(), "tests::b".into()],
            vec![(1, vec![0]), (99, vec![0])],
            vec![Duration::from_millis(1); 2],
        );
        let _invalid = reach.reached.insert(2, u32::MAX);
        let mut census = Census::default();
        let _previous = census.binaries.insert(binary.path.clone(), reach);
        let mut first = crate::fixtures::mutant();
        first.ordinal = 1;
        let mut invalid = first.clone();
        invalid.ordinal = 2;
        let mut hints = GeneralizedHints::empty_supported();

        census.persist_clusters(&[first, invalid], &[binary], &mut hints);

        assert!(hints.reach.is_empty(), "stale, malformed, and full-cost records are not persisted");
        assert!(hints.test_sets.is_empty());
    }

    #[test]
    fn taking_a_census_skips_unlistable_binaries_and_work_that_cannot_repay() {
        let directory = crate::testing::workdir("census-admission-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8 root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let missing = TestBinary {
            path: root.join("missing-test-binary"),
            ..binary("missing")
        };
        let targets = std::iter::once((missing.path.clone(), HashSet::from_iter([1]))).collect();
        let mut events = crate::testing::Recorder::default();

        let census = take(&work, &[missing], &targets, Duration::ZERO, 1, Stall::NONE, &mut events);

        assert_eq!(census.len(), 0);
        assert_eq!(census.walked, 0);
        assert_eq!(census.listing_launches(), 1);
        assert_eq!(census.listed_binaries(), 0);
        assert!(!census.walk_admitted());
        assert!(
            !census.walk_declined(),
            "failed enumeration is an incomplete census, not an economic decline"
        );
        assert_eq!(
            events.phases,
            vec![
                ("Optimizing".to_owned(), "1 test binary".to_owned()),
                (String::new(), String::new()),
            ]
        );

        let (_directory, work) =
            crate::testing::helper_workspace("census-declined-", &["when-arg:--list|print:tests::one: test", "exit:0"]);
        let binary = crate::testing::helper();
        let targets = std::iter::once((binary.path.clone(), HashSet::from_iter([1]))).collect();
        let declined = take(
            &work,
            &[binary],
            &targets,
            Duration::ZERO,
            1,
            Stall::NONE,
            &mut crate::testing::Recorder::default(),
        );

        assert_eq!(declined.listed_binaries(), 1);
        assert!(!declined.walk_admitted());
        assert!(declined.walk_declined());
    }

    #[test]
    fn taking_a_census_reports_each_unlistable_binary_and_the_empty_result() {
        #[derive(Default)]
        struct CensusEvents {
            phases: Vec<(String, String)>,
            progress: Vec<(usize, usize, String)>,
        }

        impl Events for CensusEvents {
            fn phase(&mut self, verb: &str, detail: &str) {
                self.phases.push((verb.to_owned(), detail.to_owned()));
            }

            fn phase_progress(&mut self, completed: usize, total: usize, unit: &str) {
                self.progress.push((completed, total, unit.to_owned()));
            }

            fn mutant(&mut self, _mutant: &Mutant) {}
        }

        let directory = crate::testing::workdir("census-progress-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8 root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let binaries = [
            TestBinary {
                path: root.join("missing-a"),
                ..binary("missing-a")
            },
            TestBinary {
                path: root.join("missing-b"),
                ..binary("missing-b")
            },
        ];
        let targets = binaries
            .iter()
            .map(|binary| (binary.path.clone(), HashSet::from_iter([1])))
            .collect();
        let mut events = CensusEvents::default();

        let census = take(&work, &binaries, &targets, Duration::from_secs(1), 1, Stall::NONE, &mut events);

        assert_eq!(census.len(), 0);
        assert_eq!(census.walked(), 0);
        assert_eq!(census.listing_launches(), 2);
        assert_eq!(census.listed_binaries(), 0);
        assert!(!census.walk_admitted());
        assert_eq!(
            events.progress,
            vec![
                (0, 2, "test binaries".to_owned()),
                (1, 2, "test binaries".to_owned()),
                (2, 2, "test binaries".to_owned()),
            ]
        );
        assert_eq!(
            events.phases,
            vec![
                ("Optimizing".to_owned(), "2 test binaries".to_owned()),
                (String::new(), ", 0 of 2 binaries mapped, over 0 samples".to_owned()),
            ]
        );
    }

    #[test]
    fn taking_a_census_reports_and_persists_one_complete_sample() {
        #[derive(Default)]
        struct CensusEvents {
            phases: Vec<(String, String)>,
            progress: Vec<(usize, usize, String)>,
        }

        impl Events for CensusEvents {
            fn phase(&mut self, verb: &str, detail: &str) {
                self.phases.push((verb.to_owned(), detail.to_owned()));
            }

            fn phase_progress(&mut self, completed: usize, total: usize, unit: &str) {
                self.progress.push((completed, total, unit.to_owned()));
            }

            fn mutant(&mut self, _mutant: &Mutant) {}
        }

        const SCRIPT: &[&str] = &[
            "when-arg:--list|print:tests::z: test",
            "when-arg:--list|print:tests::a: test",
            "when-arg:--list|print:tests::m: test",
            "when-arg:--list|print:tests::n: test",
            "when-arg:--list|print:tests::q: test",
            "when-arg:tests::a|write-le:GAMMA_CENSUS|7|4294967293",
            "when-arg:tests::z|write-le:GAMMA_CENSUS|7|4294967293",
            "exit:0",
        ];
        let _serial = WALK_TEST.lock().expect("the census walk test lock is not poisoned");
        let (_directory, mut work) = crate::testing::helper_workspace("census-take-complete-", SCRIPT);
        isolate_census_base(&mut work);
        let mut binary = crate::testing::helper();
        binary.baseline = Duration::from_secs(1);
        binary.budget = Some(Duration::from_secs(1));
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);
        let mut events = CensusEvents::default();

        let census = take(
            &work,
            core::slice::from_ref(&binary),
            &targets,
            Duration::from_secs(2),
            1,
            Stall::NONE,
            &mut events,
        );

        assert_eq!(census.walked(), 2);
        assert_eq!(census.len(), 1);
        assert_eq!(census.listing_launches(), 1);
        assert_eq!(census.listed_binaries(), 1);
        assert!(!census.estimated_cost().is_zero());
        assert!(census.walk_admitted());
        assert_eq!(census.selection(&binary, 7), CensusSelection::Whole);
        assert_eq!(
            events.progress,
            vec![(0, 1, "test binary".to_owned()), (1, 1, "test binary".to_owned())]
        );
        assert_eq!(events.phases[0], ("Optimizing".to_owned(), "1 test binary".to_owned()));
        assert_eq!(
            events.phases[1],
            (String::new(), ", 1 of 1 binary mapped, over 2 samples".to_owned())
        );
    }

    #[test]
    fn walking_skips_a_listed_binary_without_requested_sites() {
        let directory = crate::testing::workdir("census-unrequested-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8 root");
        let work = Workspace::adopt(root.join("workspace"), root.join("target"));
        let binary = binary("/t/a");
        let mut completed = 0;

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, vec!["tests::a".into()], MIN_SCOUT_WAIT)],
            &HashMap::default(),
            1,
            Stall::NONE,
            |_work, _binary, _names, _path, _stall| panic!("an unrequested binary must not be sampled"),
            || false,
            || completed += 1,
        );

        assert!(mapped.is_empty());
        assert_eq!(walked, 0);
        assert_eq!(completed, 1);
    }

    #[test]
    fn walking_uses_the_census_directory_and_fails_closed_when_it_is_not_a_directory() {
        let directory = crate::testing::workdir("census-directory-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8 root");
        let work = Workspace::adopt(root.join("workspace"), root.join("target"));
        let binary = binary("/t/a");
        let targets = HashMap::from_iter([(binary.path.clone(), HashSet::from_iter([7]))]);

        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, vec!["tests::a".into()], MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, _names, path, _stall| {
                assert_eq!(path.parent().and_then(Utf8Path::file_name), Some("census"));
                assert_eq!(path.file_name(), Some("0.bin"));
                Some((vec![7], Duration::ZERO))
            },
            || false,
            || {},
        );
        assert_eq!(walked, 1);
        assert_eq!(mapped.len(), 1);

        fs::remove_dir_all(work.base().join("census")).expect("the first census directory is removable");
        fs::write(work.base().join("census"), b"not a directory").expect("the blocking file is writable");
        let (mapped, walked) = walk_with(
            &work,
            vec![(&binary, vec!["tests::a".into()], MIN_SCOUT_WAIT)],
            &targets,
            1,
            Stall::NONE,
            |_work, _binary, _names, _path, _stall| panic!("a failed census directory must stop before sampling"),
            || false,
            || {},
        );
        assert!(mapped.is_empty());
        assert_eq!(walked, 0);
    }

    fn isolate_census_base(work: &mut Workspace) {
        let root = work.root.join("workspace");
        fs::create_dir_all(root.as_std_path()).expect("the isolated census workspace is creatable");
        work.root = root;
    }

    #[test]
    fn an_unrequested_binary_does_not_stop_later_requested_work() {
        let directory = crate::testing::workdir("census-unrequested-first-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8 root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let first = binary("/t/first");
        let second = binary("/t/second");
        let targets = HashMap::from_iter([(second.path.clone(), HashSet::from_iter([7]))]);
        let mut completed = 0;

        let (mapped, walked) = walk_with(
            &work,
            vec![
                (&first, vec!["tests::first".into()], MIN_SCOUT_WAIT),
                (&second, vec!["tests::second".into()], MIN_SCOUT_WAIT),
            ],
            &targets,
            1,
            Stall::NONE,
            |_work, binary, _names, _path, _stall| {
                assert_eq!(binary.path, second.path);
                Some((vec![7], Duration::from_millis(3)))
            },
            || false,
            || completed += 1,
        );

        assert_eq!(walked, 1);
        assert_eq!(completed, 2);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].0, second.path);
    }

    #[test]
    fn durable_test_sets_are_sorted_before_indices_are_assigned() {
        let binary = binary("/t/a");
        let mut census = Census::default();
        let _previous = census.binaries.insert(
            binary.path.clone(),
            reach_from(
                vec!["tests::z".into(), "tests::a".into()],
                vec![(1, vec![0]), (2, vec![1])],
                vec![Duration::from_millis(1); 2],
            ),
        );
        let mut z = crate::fixtures::mutant();
        z.ordinal = 1;
        z.item_path = "subject::z".into();
        let mut a = z.clone();
        a.ordinal = 2;
        a.item_path = "subject::a".into();
        let mut hints = GeneralizedHints::empty_supported();

        census.persist_clusters(&[z, a], &[binary], &mut hints);

        assert_eq!(hints.test_sets[0][0].test, "tests::a");
        for cluster in &hints.reach {
            assert_eq!(
                hints.test_sets[cluster.test_set as usize][0].test,
                if cluster.site.item == "subject::a" {
                    "tests::a"
                } else {
                    "tests::z"
                }
            );
        }
    }

    /// Streaming task resolution via cumulative offsets matches the old materialized tasks.
    #[test]
    fn streaming_cursor_resolves_tasks_correctly() {
        // Simulate 3 binaries with [2, 3, 1] tests.
        let sizes = [2_usize, 3, 1];
        let offsets: Vec<usize> = sizes
            .iter()
            .scan(0_usize, |acc, &size| {
                let start = *acc;
                *acc += size;
                Some(start)
            })
            .collect();
        let total: usize = sizes.iter().sum();

        // Resolve each flat index and check the result.
        let mut resolved = Vec::new();
        for at in 0..total {
            let binary_at = match offsets.binary_search(&at) {
                Ok(exact) => exact,
                Err(after) => after.saturating_sub(1),
            };
            let test_at = at - offsets[binary_at];
            resolved.push((binary_at, test_at));
        }

        assert_eq!(resolved, vec![(0, 0), (0, 1), (1, 0), (1, 1), (1, 2), (2, 0)]);
    }
}
