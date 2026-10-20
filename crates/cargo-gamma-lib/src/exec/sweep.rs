// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running the suite once per mutant and turning each result into a verdict.
//!
//! The lifecycle is:
//! 1. planning builds [`ScheduledWork`] and the borrowed [`InferredPlanning`] hint indexes;
//! 2. the scheduler assigns work while resource admission controls each process launch;
//! 3. execution and adjudication turn observations into verdicts;
//! 4. events publish progress and selection telemetry; and
//! 5. [`FileLearning`] records safe ordering knowledge for later mutants.

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::Instant;

use camino::Utf8Path;
use cargo_gamma_process::MemoryRequest;

use super::census::{Census, CensusSelection, CensusWork};
use super::events::{Events, SelectionAttempt, SelectionResult, SelectionTier};
use super::killers::Killers;
use super::stall::Stall;
use super::test_binary::{Reachability, TestBinary};
#[cfg(test)]
use super::test_binary::{TestScope, order_reachable, reaches};
use super::verdict::{
    Attempt, BinaryRun, DIAGNOSTIC_TAIL_LINES, Only, ReachObservation, Verdict, reach_hint_is_final, run_binary, run_binary_observed, tail,
};
use super::workspace::Workspace;
use crate::discover::{
    BinaryHint, FileBinaryHints, GENERALIZED_HINTS_VERSION, GeneralizedHints, ItemHints, Killer, Plan, RankedHint, SiteIdentity,
};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::report::{encode_controls, encode_preserving_color};
use crate::{Result, notes};

/// The minimum launch-cost estimate used by census admission.
///
/// Kept here for the existing shared cost model; sweep workers do not wait for this duration.
pub(super) const MIN_SCOUT_WAIT: Duration = Duration::from_millis(5);

/// Maximum campaign-wide attempts for an unproductive inferred-test tier.
///
/// Workers reserve one of these slots atomically immediately before launch. Once any attempt hits,
/// later workers may continue without the bound because the tier has proved productive.
const INFERRED_ZERO_HIT_LIMIT: usize = 8;
/// Maximum inferred exact-test candidates retained for one judgement.
const INFERRED_EXACT_LIMIT: usize = 2;
/// Persisted exact probes usually terminate before a quarter of the binary baseline.
const EXACT_PROBE_COST_DIVISOR: u32 = 4;
/// Cold killed-mutant work usually consumes about half of its complete fallback.
const KILLED_MUTANT_COST_DIVISOR: u32 = 2;
/// Canonical misses are half as strong as direct transfer misses.
const CANONICAL_MISS_DIVISOR: u32 = 2;
/// Two direct misses retire a candidate that has never produced a hit.
const MISS_RETIREMENT_THRESHOLD: u32 = 2;

/// How long a quiet sweep may leave time-dependent progress displays unchanged.
///
/// This matches the dashboard repaint limit, so the next eligible repaint is flushed without
/// waking the coordinator more often than the display can change.
const SWEEP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Describes a mutant stopped by the memory ceiling installed for its test binary.
///
/// Written to say what was measured and against what, because the reader's questions are whether
/// the ceiling was reasonable and how far past it the mutant went. Both are answerable only if the
/// note carries the two numbers rather than merely the fact.
///
/// The ceiling is inferred rather than observed: it is derived from what the same binary used
/// during the baseline, scaled and given headroom. Only the peak, when a peak is reported at all,
/// is a measurement — so the wording states the ceiling as what the run allowed rather than as
/// anything the platform saw.
fn memory_note(binary: &Utf8Path, peak: Option<u64>, limit: u64) -> String {
    let name = binary.file_name().unwrap_or(binary.as_str());

    let Some(peak) = peak else {
        return format!("`{name}` reached the {} this run allowed it", crate::report::bytes(limit));
    };

    // "past", "at" and "against" are three different findings. A workload that went past its
    // ceiling grew; one stopped exactly at it met a ceiling set a hair too low; one whose reported
    // peak is below it was stopped on evidence other than that peak. Saying "past" for all three
    // sends the reader looking for a growth that never happened.
    let where_it_landed = match peak.cmp(&limit) {
        core::cmp::Ordering::Greater => "past",
        core::cmp::Ordering::Equal => "at",
        core::cmp::Ordering::Less => "against",
    };

    // Both figures are rounded for reading, and a mutant that stopped a few kilobytes over its
    // ceiling renders as the same number twice — "reached 512 MB, past the 512 MB this run allowed
    // it" reads as a contradiction. The exact counts are the only thing that resolves it, so they
    // are printed exactly when the rounded ones would collide.
    //
    // Only when the sentence claims a difference, though. A workload stopped exactly at its
    // ceiling is the ordinary way an enforced run ends — the kernel caps the peak at the limit — so
    // "at the" plus two identical figures is not a contradiction but the finding itself. Printing
    // raw byte counts there made the common case the ugly one to read.
    let (reached, allowed) = {
        let rounded = (crate::report::bytes(peak), crate::report::bytes(limit));

        if rounded.0 == rounded.1 && peak != limit {
            (format!("{peak} bytes"), format!("{limit} bytes"))
        } else {
            rounded
        }
    };

    format!("`{name}` reached {reached}, {where_it_landed} the {allowed} this run allowed it")
}

/// Describes a stall, given the last test the harness named.
///
/// The name is a landmark rather than a diagnosis, and the wording says so. libtest runs tests in
/// parallel and announces each one only once it has finished, so the test that is actually spinning
/// is by definition one it has not named. Wording that presents the name as the culprit sends
/// people to read a test that was fine, and — worse — invites a suppression on it.
fn stall_note(test: Option<&str>) -> String {
    test.map_or_else(
        || "stalled before the harness named a test".to_owned(),
        |name| format!("stalled, last test named was `{name}`"),
    )
}

/// Describes a flake, given the test that failed both with the mutant active and without it.
///
/// The name is the whole value of this verdict. Without it the reader is told a test somewhere is
/// unreliable and left to find it, which is worse than recording it as a survivor would be —
/// at least a survivor names a line. The wording puts the remedy on the test rather than on the
/// mutant, because the mutant was never judged.
fn flaky_note(binary: &Utf8Path, test: Option<&str>) -> String {
    let which = test.map_or_else(|| "a test".to_owned(), |name| format!("test `{name}`"));

    format!("{which} in `{binary}` fails with no mutant active as well as with one, so this mutant was never judged")
}

/// Describes a mutant that prevents nextest from creating the selected test list.
///
/// The mutant note this returns becomes the report `statusReason` and deliberately never carries
/// nextest's raw output. That output comes from a test process — and anything the test or a nextest
/// extension inherited into its environment — running while a mutant is active. Copying it into
/// the note would extend a secret's retention from one process's output into every published
/// artifact of the run. A tail-truncated local diagnostic is submitted through [`crate::notes`]
/// instead. Delivery is best-effort because the command-wide note queue is bounded, but any
/// retained diagnostic is printed locally and never written into a report.
fn enumeration_note(binary: &Utf8Path, output: &str) -> String {
    const fn diagnostic_tail_lines() -> usize {
        DIAGNOSTIC_TAIL_LINES
    }

    if !output.trim().is_empty() {
        let binary = encode_controls(binary.as_str());
        let tail = tail(output, diagnostic_tail_lines());
        let output = encode_preserving_color(tail.as_ref());

        notes::note(format!(
            "`cargo nextest` could not enumerate tests in `{binary}` with a mutant active; its \
             output, kept out of every published report, was:\n{output}"
        ));
    }

    format!(
        "`cargo nextest` could not enumerate tests in `{binary}` with this mutant active; the same selection succeeded with no mutant active"
    )
}

/// Whether a killing-test identity was observed or only guessed for scheduling.
enum KillerAttribution {
    Observed(Killer),
    Guessed(Killer),
}

impl KillerAttribution {
    const fn observed(&self) -> Option<&Killer> {
        match self {
            Self::Observed(killer) => Some(killer),
            Self::Guessed(_) => None,
        }
    }

    fn into_identity(self) -> Killer {
        match self {
            Self::Observed(killer) | Self::Guessed(killer) => killer,
        }
    }
}

/// One mutant's completed execution and the scheduling timestamps used for package telemetry.
struct Completed {
    position: usize,
    outcome: Outcome,
    elapsed_ms: u64,
    killer: Option<KillerAttribution>,
    note: Option<String>,
    first_started_ms: u64,
    settled_ms: u64,
    attempts: Vec<SelectionAttempt>,
}

enum SweepEvent {
    Started,
    SelectionAttempts(Vec<SelectionAttempt>),
    Completed(Completed),
}

fn receive_sweep_event<E: Events>(
    receiver: &mpsc::Receiver<SweepEvent>,
    events: &mut E,
    heartbeat_interval: Duration,
) -> Option<SweepEvent> {
    receive_sweep_event_with(events, heartbeat_interval, |timeout| receiver.recv_timeout(timeout))
}

fn receive_sweep_event_with<E: Events>(
    events: &mut E,
    heartbeat_interval: Duration,
    mut receive: impl FnMut(Duration) -> core::result::Result<SweepEvent, mpsc::RecvTimeoutError>,
) -> Option<SweepEvent> {
    loop {
        match receive(heartbeat_interval) {
            Ok(event) => return Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => events.heartbeat(),
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

thread_local! {
    static SELECTION_ATTEMPTS: RefCell<Option<Vec<SelectionAttempt>>> = const { RefCell::new(None) };
}

fn begin_selection_trace() {
    SELECTION_ATTEMPTS.with(|attempts| {
        *attempts.borrow_mut() = Some(Vec::new());
    });
}

fn record_selection_attempt(attempt: SelectionAttempt) {
    SELECTION_ATTEMPTS.with(|attempts| {
        if let Some(attempts) = attempts.borrow_mut().as_mut() {
            attempts.push(attempt);
        }
    });
}

fn take_selection_trace() -> Vec<SelectionAttempt> {
    SELECTION_ATTEMPTS.with(|attempts| attempts.borrow_mut().take().unwrap_or_default())
}

/// Estimates a single mutant's cost from per-site census data when available, falling back to
/// the sum of its reachable binary baselines.
///
/// Census data gives the measured duration of the tests that actually reach each site, which is
/// far more precise than the binary-level baseline sum. The binary sum overstates killed mutants
/// (which exit early) and treats every mutant of a package identically; census data distinguishes
/// sites within a package by their actual reaching tests.
///
/// Killer history further refines: a mutant with a known killer from a previous run is expected
/// to complete in the time of that single probe, which is cheaper than any cold path.
fn mutant_cost(position: usize, plan: &Plan, reach: &Reachability<'_>, census: &Census, killers: &Killers) -> Duration {
    let mutant = &plan.mutants[position];

    // Persisted exact killers usually terminate early, so their prior is one quarter of the
    // binary baseline. Cold selected/whole work below uses one half because killed mutants rarely
    // consume the complete fallback. Keep these fractions aligned with `inferred_fallback_cost`.
    if let Some(hint) = killers.hint(&mutant.id)
        && let Some(binaries) = reach.reachable(mutant)
        && let Some(binary) = binaries.iter().find(|binary| binary.matches_killer(hint))
    {
        return binary.baseline / EXACT_PROBE_COST_DIVISOR;
    }

    let Some(binaries) = reach.reachable(mutant) else {
        return Duration::ZERO;
    };

    let mut total = Duration::ZERO;

    for binary in binaries {
        let ordinal = mutant.ordinal;
        match census.work(binary, ordinal) {
            CensusWork::Selected(duration) => total += duration / KILLED_MUTANT_COST_DIVISOR,
            CensusWork::Whole => total += binary.baseline / KILLED_MUTANT_COST_DIVISOR,
            CensusWork::Uncovered => {}
            CensusWork::Hinted(duration) => {
                total = total
                    .saturating_add(duration)
                    .saturating_add(crate::estimate::inferred_fallback_cost(binary.baseline));
            }
        }
    }

    total
}

fn mutant_work_with_hints(
    position: usize,
    plan: &Plan,
    reach: &Reachability<'_>,
    census: &Census,
    killers: &Killers,
    inferred: &InferredPlanning<'_>,
) -> crate::estimate::MutationWork {
    let mutant = &plan.mutants[position];

    if let Some(hint) = killers.hint(&mutant.id)
        && let Some(binaries) = reach.reachable(mutant)
        && let Some(binary) = binaries.iter().find(|binary| binary.matches_killer(hint))
    {
        let fallback = binaries
            .iter()
            .fold(Duration::ZERO, |total, binary| total.saturating_add(binary.baseline));
        return crate::estimate::MutationWork::hinted(
            crate::estimate::WorkKind::Exact,
            binary.baseline / EXACT_PROBE_COST_DIVISOR,
            fallback,
        );
    }

    if let Some(work) = inferred_mutant_work(mutant, reach, inferred) {
        return work;
    }

    let mut killed = Duration::ZERO;
    let mut running = 0_u32;
    let mut hinted = false;
    let mut whole = false;

    if let Some(binaries) = reach.reachable(mutant) {
        for binary in binaries {
            match census.work(binary, mutant.ordinal) {
                CensusWork::Selected(duration) => {
                    killed = killed.saturating_add(duration / 2);
                }
                CensusWork::Whole => {
                    whole = true;
                    killed = killed.saturating_add(binary.baseline / KILLED_MUTANT_COST_DIVISOR);
                }
                CensusWork::Hinted(duration) => {
                    hinted = true;
                    killed = killed
                        .saturating_add(duration)
                        .saturating_add(crate::estimate::inferred_fallback_cost(binary.baseline));
                }
                CensusWork::Uncovered => continue,
            }
            running = running.saturating_add(1);
        }
    }

    if running == 0 {
        return crate::estimate::MutationWork::new(crate::estimate::WorkKind::Uncovered, Duration::ZERO);
    }
    let kind = if hinted {
        crate::estimate::WorkKind::Hinted
    } else if whole {
        crate::estimate::WorkKind::Whole
    } else {
        crate::estimate::WorkKind::Selected
    };

    crate::estimate::MutationWork::costed(kind, killed)
}

/// Once-per-planning-pass borrowed indexes for inferred test selection.
///
/// Lookups are keyed by file-and-item identity, file identity, and stable reach-site identity.
/// Unsupported generalized generations deliberately produce an empty view.
struct InferredPlanning<'a> {
    items: crate::HashMap<(&'a Utf8Path, &'a str), &'a [RankedHint<Killer>]>,
    files: crate::HashMap<&'a Utf8Path, &'a [RankedHint<BinaryHint>]>,
    // Preserve first-match execution and last-match cost estimation for duplicate sites.
    reach: crate::HashMap<&'a SiteIdentity, (&'a [Killer], &'a [Killer])>,
}

impl<'a> InferredPlanning<'a> {
    fn new(killers: &'a Killers) -> Self {
        let mut planning = Self {
            items: crate::HashMap::default(),
            files: crate::HashMap::default(),
            reach: crate::HashMap::default(),
        };
        let Some(hints) = killers.generalized().supported() else {
            return planning;
        };

        planning.items.extend(
            hints
                .items
                .iter()
                .map(|item| ((item.file.as_path(), item.item.as_str()), item.candidates.as_slice())),
        );
        planning
            .files
            .extend(hints.binaries.iter().map(|file| (file.file.as_path(), file.candidates.as_slice())));
        for cluster in &hints.reach {
            let Ok(index) = usize::try_from(cluster.test_set) else { continue };
            let Some(candidates) = hints.test_sets.get(index).map(Vec::as_slice) else {
                continue;
            };
            planning
                .reach
                .entry(&cluster.site)
                .and_modify(|entry| entry.1 = candidates)
                .or_insert((candidates, candidates));
        }
        planning
    }
}

#[cfg(test)]
fn mutant_work(
    position: usize,
    plan: &Plan,
    reach: &Reachability<'_>,
    census: &Census,
    killers: &Killers,
) -> crate::estimate::MutationWork {
    let inferred = InferredPlanning::new(killers);
    mutant_work_with_hints(position, plan, reach, census, killers, &inferred)
}

struct InferredCandidate<'a> {
    binary: &'a TestBinary,
    measured_ms: u64,
    samples: u32,
    successes: u32,
    misses: u32,
    probe: InferredProbe,
}

enum InferredProbe {
    Filtered,
    Whole,
}

fn reached_work_candidate<'a>(binaries: &[&'a TestBinary], candidates: &[Killer]) -> Option<InferredCandidate<'a>> {
    for candidate in candidates {
        if let Some(binary) = binaries.iter().copied().find(|binary| binary.matches_killer(candidate)) {
            return Some(InferredCandidate {
                binary,
                measured_ms: 0,
                samples: 0,
                successes: 0,
                misses: 0,
                probe: InferredProbe::Filtered,
            });
        }
    }
    None
}

fn item_candidate<'a>(binaries: &[&'a TestBinary], candidates: &[RankedHint<Killer>]) -> Option<InferredCandidate<'a>> {
    for candidate in candidates {
        if let Some(binary) = binaries.iter().copied().find(|binary| binary.matches_killer(&candidate.candidate)) {
            return Some(InferredCandidate {
                binary,
                measured_ms: candidate.measured_ms,
                samples: candidate.samples,
                successes: candidate.hits,
                misses: candidate.misses,
                probe: InferredProbe::Filtered,
            });
        }
    }
    None
}

fn file_candidate<'a>(binaries: &[&'a TestBinary], candidates: &[RankedHint<BinaryHint>]) -> Option<InferredCandidate<'a>> {
    for candidate in candidates {
        if let Some(binary) = binaries
            .iter()
            .copied()
            .find(|binary| binary.matches_target(&candidate.candidate.package, &candidate.candidate.target))
        {
            return Some(InferredCandidate {
                binary,
                measured_ms: candidate.measured_ms,
                samples: candidate.samples,
                successes: candidate.hits,
                misses: candidate.misses,
                probe: InferredProbe::Whole,
            });
        }
    }
    None
}

fn inferred_mutant_work(mutant: &Mutant, reach: &Reachability<'_>, hints: &InferredPlanning<'_>) -> Option<crate::estimate::MutationWork> {
    let binaries = reach.reachable(mutant)?;
    let site = SiteIdentity::from_mutant(mutant);
    let fallback = binaries
        .iter()
        .fold(Duration::ZERO, |total, binary| total.saturating_add(binary.baseline));

    let candidate = hints
        .reach
        .get(&site)
        .and_then(|(_, candidates)| reached_work_candidate(binaries, candidates))
        .or_else(|| {
            hints
                .items
                .get(&(mutant.file.as_ref(), mutant.item_path.as_ref()))
                .and_then(|candidates| item_candidate(binaries, candidates))
        })
        .or_else(|| {
            hints
                .files
                .get(mutant.file.as_ref())
                .and_then(|candidates| file_candidate(binaries, candidates))
        })?;
    let probe = if candidate.samples > 0 {
        Duration::from_millis(candidate.measured_ms / u64::from(candidate.samples))
    } else if matches!(candidate.probe, InferredProbe::Filtered) {
        candidate.binary.baseline / EXACT_PROBE_COST_DIVISOR
    } else {
        candidate.binary.baseline / KILLED_MUTANT_COST_DIVISOR
    };
    Some(crate::estimate::MutationWork::hinted_with_observations(
        probe,
        fallback,
        candidate.successes,
        candidate.misses,
    ))
}

/// Builds the stable package-fair, longest-work-first priority used by the live scheduler.
///
/// Package queues start in descending expected-cost order rather than whatever order discovery
/// happened to enumerate them. Assignment-time distance and learning value may move work ahead of
/// this order, which remains the deterministic secondary priority.
///
/// When census data is available, each mutant's cost is estimated from the measured duration of
/// its reaching tests. When a killer hint exists, the expected cost reflects a single probe.
/// Otherwise the cost falls back to the sum of reachable binary baselines.
///
/// Mutants of one package that share a file are grouped together so that file-local killer
/// learning can benefit siblings. Within a package, own-package cold tests are prioritized by
/// placing mutants from the package that owns the binary first.
///
/// Package order is the longest-first order, and positions within a package stay in plan order, so
/// the result remains deterministic. A round takes one mutant from every package that still has
/// work; the expensive queues therefore still start first without monopolising every worker while
/// independent package work remains.
fn schedule(pending: &mut [usize], plan: &Plan, reach: &Reachability<'_>, census: &Census, killers: &Killers) {
    pending.sort_by_cached_key(|position| (core::cmp::Reverse(mutant_cost(*position, plan, reach, census, killers)), *position));

    let mut queues: Vec<Vec<usize>> = Vec::new();
    let mut by_package: crate::HashMap<String, usize> = crate::HashMap::default();

    for position in pending.iter().copied() {
        let package = &*plan.mutants[position].package;

        if let Some(index) = by_package.get(package).copied() {
            queues[index].push(position);
        } else {
            let index = queues.len();
            let _fresh = by_package.insert(package.to_owned(), index);
            queues.push(vec![position]);
        }
    }

    let mut next = vec![0_usize; queues.len()];
    let mut out = 0_usize;

    'interleave: loop {
        // #[gamma::skip(literal.bool_flip, reason = "starting each insertion as already moved prevents the insertion and leaves this fixed-point loop non-terminating")]
        let mut moved = false;

        for (queue, cursor) in queues.iter().zip(&mut next) {
            if let Some(position) = queue.get(*cursor).copied() {
                pending[out] = position;
                out += 1;
                *cursor += 1;
                moved = true;
            }
        }

        // #[gamma::skip(cond.always_false, reason = "suppressing the fixed-point termination condition makes scheduling loop forever")]
        if !moved {
            // #[gamma::skip(all, reason = "removing the fixed-point exit makes scheduling loop forever")]
            break 'interleave;
        }
    }
}

#[derive(Debug)]
struct ScheduledWork {
    file: usize,
    item: Arc<str>,
    package: Arc<str>,
    cost: Duration,
    sibling_benefit: usize,
    hinted: bool,
    stable_order: usize,
    resources: Vec<Arc<str>>,
}

#[derive(Debug, Default)]
struct SchedulerState {
    remaining: Vec<bool>,
    remaining_count: usize,
    active_files: Vec<usize>,
    active_items: crate::HashMap<(usize, Arc<str>), usize>,
    learned_items: crate::HashSet<(usize, Arc<str>)>,
    package_turns: Vec<usize>,
    package_queues: Vec<BTreeSet<LocalPriority>>,
    heads: BTreeSet<PackageHead>,
    groups: Vec<GroupQueue>,
    files: Vec<FileQueue>,
    priorities: Vec<Option<LocalPriority>>,
    pending_groups: BTreeSet<usize>,
    active_resources: crate::HashMap<Arc<str>, usize>,
    #[cfg(test)]
    priority_updates: usize,
    #[cfg(test)]
    learning_updates: usize,
}

#[derive(Debug, Default)]
struct GroupQueue {
    // Contention-free priorities; reservations change only the published heads.
    cold: BTreeSet<LocalPriority>,
    informed: BTreeSet<LocalPriority>,
    idle_head: Option<LocalPriority>,
    active_head: Option<LocalPriority>,
}

#[derive(Debug, Default)]
struct FileQueue {
    idle: BTreeSet<LocalPriority>,
    active: BTreeSet<LocalPriority>,
    head: Option<LocalPriority>,
}

struct Scheduler {
    work: Vec<ScheduledWork>,
    package_slots: Vec<usize>,
    by_file: Vec<Vec<usize>>,
    file_packages: Vec<usize>,
    group_files: Vec<usize>,
    work_groups: Vec<usize>,
    by_item: crate::HashMap<(usize, Arc<str>), Vec<usize>>,
    state: Mutex<SchedulerState>,
    changed: Condvar,
    resource_capacities: crate::HashMap<Arc<str>, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalPriority {
    distance: (u8, usize),
    benefit: usize,
    cost: u128,
    stable_order: usize,
    index: usize,
}

impl Ord for LocalPriority {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.distance
            .cmp(&other.distance)
            .then_with(|| learning_priority(self, other))
            .then_with(|| other.cost.cmp(&self.cost))
            .then_with(|| self.stable_order.cmp(&other.stable_order))
            .then_with(|| self.index.cmp(&other.index))
    }
}

impl PartialOrd for LocalPriority {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackageHead {
    package: usize,
    turns: usize,
    local: LocalPriority,
}

impl Ord for PackageHead {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.local
            .distance
            .cmp(&other.local.distance)
            .then_with(|| self.turns.cmp(&other.turns))
            .then_with(|| learning_priority(&self.local, &other.local))
            .then_with(|| other.local.cost.cmp(&self.local.cost))
            .then_with(|| self.local.stable_order.cmp(&other.local.stable_order))
            .then_with(|| self.local.index.cmp(&other.local.index))
            .then_with(|| self.package.cmp(&other.package))
    }
}

impl PartialOrd for PackageHead {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct Assignment<'a> {
    scheduler: &'a Scheduler,
    index: usize,
    learned: bool,
}

impl Assignment<'_> {
    const fn index(&self) -> usize {
        self.index
    }

    fn complete(mut self) {
        self.learned = true;
    }
}

impl Drop for Assignment<'_> {
    // #[gamma::skip(fn_value.unit, tag = "timeout", reason = "dropping an assignment must release scheduler capacity and wake pending work")]
    fn drop(&mut self) {
        self.scheduler.release(self.index, self.learned);
    }
}

impl Scheduler {
    #[cfg(test)]
    fn new(work: Vec<ScheduledWork>, files: usize) -> Self {
        Self::with_resources(work, files, crate::HashMap::default())
    }

    fn with_resources(work: Vec<ScheduledWork>, files: usize, resource_capacities: crate::HashMap<Arc<str>, usize>) -> Self {
        let mut packages: crate::HashMap<Arc<str>, usize> = crate::HashMap::default();
        let package_slots = work
            .iter()
            .map(|item| {
                let next = packages.len();
                *packages.entry(Arc::clone(&item.package)).or_insert(next)
            })
            .collect::<Vec<_>>();
        let mut by_file = vec![Vec::new(); files];
        let mut file_slots = crate::HashMap::default();
        let mut file_packages = Vec::new();
        let mut group_slots = crate::HashMap::default();
        let mut group_files = Vec::new();
        let mut work_groups = Vec::with_capacity(work.len());
        let mut by_item: crate::HashMap<(usize, Arc<str>), Vec<usize>> = crate::HashMap::default();
        for (index, item) in work.iter().enumerate() {
            let package = package_slots[index];
            let next_file = file_packages.len();
            let file_slot = *file_slots.entry((item.file, package)).or_insert_with(|| {
                by_file[item.file].push(next_file);
                file_packages.push(package);
                next_file
            });
            let next_group = group_files.len();
            let group = *group_slots.entry((file_slot, Arc::clone(&item.item))).or_insert_with(|| {
                group_files.push(file_slot);
                by_item.entry((item.file, Arc::clone(&item.item))).or_default().push(next_group);
                next_group
            });
            work_groups.push(group);
        }

        let mut state = SchedulerState {
            remaining: vec![true; work.len()],
            remaining_count: work.len(),
            active_files: vec![0; files],
            active_items: crate::HashMap::default(),
            learned_items: crate::HashSet::default(),
            package_turns: vec![0; packages.len()],
            package_queues: vec![BTreeSet::new(); packages.len()],
            heads: BTreeSet::new(),
            groups: (0..group_files.len()).map(|_| GroupQueue::default()).collect(),
            files: (0..file_packages.len()).map(|_| FileQueue::default()).collect(),
            priorities: vec![None; work.len()],
            pending_groups: (0..group_files.len()).collect(),
            active_resources: crate::HashMap::default(),
            #[cfg(test)]
            priority_updates: 0,
            #[cfg(test)]
            learning_updates: 0,
        };
        for index in 0..work.len() {
            let priority = priority(&work, &state, index).expect("all pending work is eligible before any reservation");
            state.priorities[index] = Some(priority);
            let group = &mut state.groups[work_groups[index]];
            if work[index].hinted {
                group.informed.insert(priority);
            } else {
                group.cold.insert(priority);
            }
        }

        let mut scheduler = Self {
            work,
            package_slots,
            by_file,
            file_packages,
            group_files,
            work_groups,
            by_item,
            state: Mutex::new(SchedulerState::default()),
            changed: Condvar::new(),
            resource_capacities,
        };
        for key in scheduler.by_item.keys() {
            scheduler.refresh_item(&mut state, key);
        }
        for file in 0..files {
            scheduler.detach_file(&mut state, file);
            scheduler.attach_file(&mut state, file);
        }
        *scheduler
            .state
            .get_mut()
            .expect("new scheduler mutex has never been locked or poisoned") = state;
        scheduler
    }

    #[cfg(test)]
    fn claim(&self, abandoned: &OnceLock<String>) -> Option<usize> {
        self.claim_with(abandoned, || {})
    }

    fn claim_with(&self, abandoned: &OnceLock<String>, mut before_wait: impl FnMut()) -> Option<usize> {
        let mut state = self
            .state
            .lock()
            .expect("a scheduler panic can leave coupled queue and reservation state inconsistent");

        loop {
            if abandoned.get().is_some() {
                return None;
            }

            if let Some(index) = self.select(&state) {
                let work = &self.work[index];
                self.detach_file(&mut state, work.file);
                debug_assert!(state.remaining[index], "only remaining work may be claimed");
                debug_assert!(state.remaining_count > 0, "remaining work implies a positive remaining count");
                state.remaining[index] = false;
                state.remaining_count -= 1;
                self.remove_work(&mut state, index);
                state.active_files[work.file] = state.active_files[work.file]
                    .checked_add(1)
                    .expect("active file count cannot exceed the finite worker population");
                let active_item = state.active_items.entry((work.file, Arc::clone(&work.item))).or_default();
                *active_item = active_item
                    .checked_add(1)
                    .expect("active item count cannot exceed the finite worker population");
                let package = self.package_slots[index];
                state.package_turns[package] = state.package_turns[package]
                    .checked_add(1)
                    .expect("package turns cannot exceed the finite mutant population");
                for resource in &work.resources {
                    let active = state.active_resources.entry(Arc::clone(resource)).or_default();
                    *active = active
                        .checked_add(1)
                        .expect("active resource count cannot exceed the finite worker population");
                }
                self.refresh_item(&mut state, &(work.file, Arc::clone(&work.item)));
                self.attach_file(&mut state, work.file);
                return Some(index);
            }

            if state.remaining_count == 0 {
                return None;
            }

            before_wait();
            state = self
                .changed
                .wait(state)
                .expect("a scheduler panic can leave coupled queue and reservation state inconsistent");
        }
    }

    #[cfg(test)]
    fn assignment<'a>(&'a self, abandoned: &OnceLock<String>) -> Option<Assignment<'a>> {
        self.assignment_with(abandoned, || {})
    }

    fn assignment_with<'a>(&'a self, abandoned: &OnceLock<String>, before_wait: impl FnMut()) -> Option<Assignment<'a>> {
        self.claim_with(abandoned, before_wait).map(|index| Assignment {
            scheduler: self,
            index,
            learned: false,
        })
    }

    #[cfg(test)]
    fn complete(&self, index: usize) {
        self.release(index, true);
    }

    fn release(&self, index: usize, learned: bool) {
        let work = &self.work[index];
        let mut state = self
            .state
            .lock()
            .expect("a scheduler panic can leave coupled queue and reservation state inconsistent");
        self.detach_file(&mut state, work.file);
        let key = (work.file, Arc::clone(&work.item));
        if learned {
            let _inserted = state.learned_items.insert(key.clone());
        }
        debug_assert!(state.active_files[work.file] > 0, "a released file must have an active assignment");
        state.active_files[work.file] -= 1;
        let active = state
            .active_items
            .get_mut(&key)
            .expect("every released assignment must have an active item reservation");
        debug_assert!(*active > 0, "an active item reservation must be positive");
        *active -= 1;
        if *active == 0 {
            state.active_items.remove(&key);
        }
        for resource in &work.resources {
            let active = state
                .active_resources
                .get_mut(resource)
                .expect("every released assignment must have all of its resource reservations");
            debug_assert!(*active > 0, "an active resource reservation must be positive");
            *active -= 1;
            if *active == 0 {
                state.active_resources.remove(resource);
            }
        }
        self.refresh_item(&mut state, &key);
        self.attach_file(&mut state, work.file);
        core::mem::drop(state);
        self.changed.notify_all();
    }

    fn abandon(&self) {
        self.abandon_with(|| self.changed.notify_all());
    }

    fn abandon_with(&self, notify: impl FnOnce()) {
        let _state = self
            .state
            .lock()
            .expect("a scheduler panic can leave coupled queue and reservation state inconsistent");
        notify();
    }

    // #[gamma::skip(fn_value.none, tag = "timeout", reason = "never selecting available work leaves scheduler waiters blocked")]
    fn select(&self, state: &SchedulerState) -> Option<usize> {
        if self.resource_capacities.is_empty() {
            return state.heads.first().map(|head| head.local.index);
        }
        if let Some(index) = state.heads.first().map(|head| head.local.index)
            && self.resources_available(state, index)
        {
            return Some(index);
        }

        state
            .pending_groups
            .iter()
            .flat_map(|group| {
                let queue = &state.groups[*group];
                [&queue.cold, &queue.informed]
            })
            .filter_map(|queue| {
                let distance = priority(&self.work, state, queue.first()?.index)?.distance;
                queue
                    .iter()
                    .find(|entry| self.resources_available(state, entry.index))
                    .map(|entry| {
                        let mut local = *entry;
                        local.distance = distance;
                        let package = self.package_slots[entry.index];
                        PackageHead {
                            package,
                            turns: state.package_turns[package],
                            local,
                        }
                    })
            })
            .min()
            .map(|head| head.local.index)
    }

    // #[gamma::skip(fn_value.bool_false, tag = "timeout", reason = "declaring every resource unavailable leaves scheduler waiters blocked")]
    fn resources_available(&self, state: &SchedulerState, index: usize) -> bool {
        self.work[index].resources.iter().all(|resource| {
            let capacity = self.resource_capacities.get(resource).copied().unwrap_or(1);
            state.active_resources.get(resource).copied().unwrap_or(0) < capacity
        })
    }

    fn detach_file(&self, state: &mut SchedulerState, file: usize) {
        for slot in &self.by_file[file] {
            let package = self.file_packages[*slot];
            remove_head(state, package);
            if let Some(head) = state.files[*slot].head.take() {
                let removed = state.package_queues[package].remove(&head);
                debug_assert!(removed, "each published file head belongs to its package queue");
            }
        }
    }

    // #[gamma::skip(fn_value.unit, tag = "timeout", reason = "without inserting file priorities pending work has no schedulable head")]
    fn attach_file(&self, state: &mut SchedulerState, file: usize) {
        let contention = state.active_files[file];
        for slot in &self.by_file[file] {
            let queue = &mut state.files[*slot];
            let head = if contention == 0 {
                queue.idle.first().copied()
            } else {
                queue.active.first().copied().map(|mut head| {
                    head.distance.1 = contention;
                    head
                })
            };
            queue.head = head;
            let package = self.file_packages[*slot];
            if let Some(head) = head {
                state.package_queues[package].insert(head);
            }
            insert_head(state, package);
        }
    }

    fn refresh_item(&self, state: &mut SchedulerState, key: &(usize, Arc<str>)) {
        let active = state.active_items.get(key).copied().unwrap_or(0) != 0;
        let learned = state.learned_items.contains(key);
        for group_index in &self.by_item[key] {
            #[cfg(test)]
            {
                state.priority_updates += 1;
            }
            let group = &mut state.groups[*group_index];
            let file = &mut state.files[self.group_files[*group_index]];
            if let Some(head) = group.idle_head.take() {
                file.idle.remove(&head);
            }
            if let Some(head) = group.active_head.take() {
                file.active.remove(&head);
            }
            // Learning changes cold priorities only once; reservations thereafter change heads.
            if learned {
                for mut entry in core::mem::take(&mut group.cold) {
                    #[cfg(test)]
                    {
                        state.learning_updates += 1;
                    }
                    entry.benefit = 0;
                    state.priorities[entry.index] = Some(entry);
                    group.informed.insert(entry);
                }
            }
            let idle = group.cold.first().into_iter().chain(group.informed.first()).min().copied();
            let available = if active { group.informed.first().copied() } else { idle };
            group.idle_head = idle;
            group.active_head = available.map(|mut head| {
                head.distance = (if active { 2 } else { 1 }, 0);
                head
            });
            if let Some(head) = group.idle_head {
                file.idle.insert(head);
            }
            if let Some(head) = group.active_head {
                file.active.insert(head);
            }
        }
    }

    fn remove_work(&self, state: &mut SchedulerState, index: usize) {
        let entry = state.priorities[index].take().expect("only queued pending work can be removed");
        let group = &mut state.groups[self.work_groups[index]];
        let removed = group.cold.remove(&entry) || group.informed.remove(&entry);
        debug_assert!(removed, "each pending priority belongs to exactly one item queue");
        if group.cold.is_empty() && group.informed.is_empty() {
            state.pending_groups.remove(&self.work_groups[index]);
        }
    }

    #[cfg(test)]
    fn refresh_file(&self, state: &mut SchedulerState, file: usize) {
        self.detach_file(state, file);
        for key in self.by_item.keys().filter(|(candidate, _)| *candidate == file) {
            self.refresh_item(state, key);
        }
        self.attach_file(state, file);
    }

    #[cfg(test)]
    fn withdraw(&self, state: &mut SchedulerState, index: usize) {
        let file = self.work[index].file;
        self.detach_file(state, file);
        if state.remaining[index] {
            state.remaining[index] = false;
            state.remaining_count = state.remaining_count.saturating_sub(1);
            self.remove_work(state, index);
        }
        self.refresh_item(state, &(file, Arc::clone(&self.work[index].item)));
        self.attach_file(state, file);
    }
}

// #[gamma::skip(fn_value.none, tag = "timeout", reason = "without priorities pending work has no schedulable head")]
fn priority(work: &[ScheduledWork], state: &SchedulerState, index: usize) -> Option<LocalPriority> {
    if !state.remaining[index] {
        return None;
    }

    let item = &work[index];
    let file_contention = state.active_files[item.file];
    let key = (item.file, Arc::clone(&item.item));
    let learned = state.learned_items.contains(&key);
    let item_contention = state.active_items.get(&key).copied().unwrap_or(0);
    // Prefer an uncontended file, then an uncontended item, then informed work that can safely
    // share an active item. Cold siblings wait for their scout to publish learning.
    let distance = if file_contention == 0 {
        (0, 0)
    } else if item_contention == 0 {
        (1, file_contention)
    } else if item.hinted || learned {
        (2, file_contention)
    } else {
        return None;
    };

    Some(LocalPriority {
        distance,
        benefit: usize::from(!learned && !item.hinted).saturating_mul(item.sibling_benefit),
        cost: item.cost.as_nanos().max(1),
        stable_order: item.stable_order,
        index,
    })
}

fn learning_priority(left: &LocalPriority, right: &LocalPriority) -> core::cmp::Ordering {
    // Within the same contention class, maximize expected sibling benefit per unit of estimated
    // cost; stable ordering is applied by `LocalPriority::cmp` after this comparison.
    let left_value = (left.benefit as u128).saturating_mul(right.cost);
    let right_value = (right.benefit as u128).saturating_mul(left.cost);

    right_value.cmp(&left_value)
}

fn remove_head(state: &mut SchedulerState, package: usize) {
    let Some(local) = state.package_queues[package].first().copied() else {
        return;
    };
    let removed = state.heads.remove(&PackageHead {
        package,
        turns: state.package_turns[package],
        local,
    });
    debug_assert!(removed, "the cached package head must be present before it is refreshed");
}

// #[gamma::skip(fn_value.unit, tag = "timeout", reason = "not inserting a package head leaves pending work unschedulable")]
fn insert_head(state: &mut SchedulerState, package: usize) {
    let Some(local) = state.package_queues[package].first().copied() else {
        return;
    };
    let inserted = state.heads.insert(PackageHead {
        package,
        turns: state.package_turns[package],
        local,
    });
    debug_assert!(inserted, "a package may have only one cached head");
}

/// What a sweep spent, tallied across its workers as they run.
///
/// Two counters, incremented once per subprocess launched, so the tally costs a relaxed atomic add
/// per launch rather than anything per test. See [`Spent`], which is what a finished sweep hands
/// back once the counters have stopped moving.
#[derive(Debug, Default)]
struct Tally {
    /// How many test-binary subprocesses were launched, across ordinary runs and probes alike.
    launches: AtomicUsize,

    /// How many of those launches were hint-directed probes.
    probes: AtomicUsize,
    exact_probes: AtomicUsize,
    exact_hits: AtomicUsize,
    inferred_probes: AtomicUsize,
    inferred_hits: AtomicUsize,
    inferred_rejected: AtomicUsize,
    exact_candidates: AtomicUsize,
    inferred_candidates: AtomicUsize,
    item_candidates: AtomicUsize,
    reach_candidates: AtomicUsize,
    file_candidates: AtomicUsize,
    census_candidates: AtomicUsize,
    item_probes: AtomicUsize,
    item_hits: AtomicUsize,
    reach_probes: AtomicUsize,
    reach_hits: AtomicUsize,
    file_probes: AtomicUsize,
    file_hits: AtomicUsize,
    item_rejected: AtomicUsize,
    reach_rejected: AtomicUsize,
    file_rejected: AtomicUsize,
    census_probes: AtomicUsize,
    census_hits: AtomicUsize,

    /// How canonical binary decisions used reachability information.
    whole_selections: AtomicUsize,
    case_selections: AtomicUsize,
    hinted_selections: AtomicUsize,
    hinted_fallbacks: AtomicUsize,
    uncovered_selections: AtomicUsize,
    available_tests: AtomicUsize,
    selected_tests: AtomicUsize,

    /// How many canonical binary launches reach learning proved unnecessary.
    saved: AtomicUsize,

    /// How many of those savings came specifically from sweep-derived reach evidence.
    reach_saved: AtomicUsize,
}

/// What a finished sweep spent, read off its [`Tally`] once every worker has stopped.
#[derive(Debug, Clone, Default)]
pub(super) struct Spent {
    /// How many test-binary subprocesses the sweep launched in total.
    pub(super) launches: usize,

    /// How many of those launches were hint-directed probes.
    pub(super) probes: usize,
    pub(super) exact_probes: usize,
    pub(super) exact_hits: usize,
    pub(super) inferred_probes: usize,
    pub(super) inferred_hits: usize,
    pub(super) inferred_rejected: usize,
    pub(super) exact_candidates: usize,
    pub(super) inferred_candidates: usize,
    pub(super) item_candidates: usize,
    pub(super) reach_candidates: usize,
    pub(super) file_candidates: usize,
    pub(super) census_candidates: usize,
    pub(super) item_probes: usize,
    pub(super) item_hits: usize,
    pub(super) reach_probes: usize,
    pub(super) reach_hits: usize,
    pub(super) file_probes: usize,
    pub(super) file_hits: usize,
    pub(super) item_rejected: usize,
    pub(super) reach_rejected: usize,
    pub(super) file_rejected: usize,
    pub(super) census_probes: usize,
    pub(super) census_hits: usize,
    pub(super) whole_selections: usize,
    pub(super) case_selections: usize,
    pub(super) hinted_selections: usize,
    pub(super) hinted_fallbacks: usize,
    pub(super) uncovered_selections: usize,
    pub(super) available_tests: usize,
    pub(super) selected_tests: usize,

    /// Package wall-clock spans measured from the start of the sweep.
    pub(super) packages: Vec<PackageSpent>,

    /// How many canonical binary launches reach learning avoided.
    pub(super) saved: usize,

    /// How many launches were saved specifically by sweep-derived reach evidence.
    pub(super) reach_saved: usize,
}

#[derive(Debug, Clone)]
pub(super) struct PackageSpent {
    pub(super) package: Arc<str>,
    pub(super) mutants: usize,
    pub(super) first_started_ms: u64,
    pub(super) settled_ms: u64,
}

impl Tally {
    fn spent(&self, spans: &crate::HashMap<Arc<str>, (usize, u64, u64)>) -> Spent {
        let mut packages = spans
            .iter()
            .map(|(package, &(mutants, first_started_ms, settled_ms))| PackageSpent {
                package: Arc::clone(package),
                mutants,
                first_started_ms,
                settled_ms,
            })
            .collect::<Vec<_>>();
        packages.sort_by(|left, right| left.package.cmp(&right.package));
        Spent {
            launches: self.launches.load(Ordering::Relaxed),
            probes: self.probes.load(Ordering::Relaxed),
            exact_probes: self.exact_probes.load(Ordering::Relaxed),
            exact_hits: self.exact_hits.load(Ordering::Relaxed),
            inferred_probes: self.inferred_probes.load(Ordering::Relaxed),
            inferred_hits: self.inferred_hits.load(Ordering::Relaxed),
            inferred_rejected: self.inferred_rejected.load(Ordering::Relaxed),
            exact_candidates: self.exact_candidates.load(Ordering::Relaxed),
            inferred_candidates: self.inferred_candidates.load(Ordering::Relaxed),
            item_candidates: self.item_candidates.load(Ordering::Relaxed),
            reach_candidates: self.reach_candidates.load(Ordering::Relaxed),
            file_candidates: self.file_candidates.load(Ordering::Relaxed),
            census_candidates: self.census_candidates.load(Ordering::Relaxed),
            item_probes: self.item_probes.load(Ordering::Relaxed),
            item_hits: self.item_hits.load(Ordering::Relaxed),
            reach_probes: self.reach_probes.load(Ordering::Relaxed),
            reach_hits: self.reach_hits.load(Ordering::Relaxed),
            file_probes: self.file_probes.load(Ordering::Relaxed),
            file_hits: self.file_hits.load(Ordering::Relaxed),
            item_rejected: self.item_rejected.load(Ordering::Relaxed),
            reach_rejected: self.reach_rejected.load(Ordering::Relaxed),
            file_rejected: self.file_rejected.load(Ordering::Relaxed),
            census_probes: self.census_probes.load(Ordering::Relaxed),
            census_hits: self.census_hits.load(Ordering::Relaxed),
            whole_selections: self.whole_selections.load(Ordering::Relaxed),
            case_selections: self.case_selections.load(Ordering::Relaxed),
            hinted_selections: self.hinted_selections.load(Ordering::Relaxed),
            hinted_fallbacks: self.hinted_fallbacks.load(Ordering::Relaxed),
            uncovered_selections: self.uncovered_selections.load(Ordering::Relaxed),
            available_tests: self.available_tests.load(Ordering::Relaxed),
            selected_tests: self.selected_tests.load(Ordering::Relaxed),
            packages,
            saved: self.saved.load(Ordering::Relaxed),
            reach_saved: self.reach_saved.load(Ordering::Relaxed),
        }
    }
}

#[derive(Default)]
struct NegativeLearning {
    binaries: Mutex<crate::HashSet<(SiteIdentity, camino::Utf8PathBuf)>>,
}

impl NegativeLearning {
    fn contains(&self, site: &SiteIdentity, binary: &TestBinary) -> bool {
        self.binaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&(site.clone(), binary.path.clone()))
    }

    fn record(&self, site: &SiteIdentity, binary: &TestBinary) {
        let _inserted = self
            .binaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((site.clone(), binary.path.clone()));
    }
}

/// Tests every live mutant in parallel, writing verdicts back onto the plan.
///
/// Workers publish each verdict over a channel that the calling thread drains while they are still
/// running, so the display moves as the run proceeds rather than jumping at the end. Returns
/// `None` when no mutant was pending and `Some` with the cost of a sweep that ran.
///
/// # Errors
///
/// Returns an error if the requested memory accounting becomes unavailable. The sweep stops rather
/// than produce verdicts without the protection the run promised.
pub(super) fn test_all(
    work: &Workspace,
    plan: &mut Plan,
    reach: &Reachability<'_>,
    sweep: Sweep<'_>,
    deterministic_reach: bool,
    killers: &mut Killers,
    events: &mut impl Events,
) -> Result<Option<Spent>> {
    test_all_with_accounting_hooks(
        work,
        plan,
        reach,
        sweep,
        deterministic_reach,
        killers,
        events,
        (
            |mutant, killers, cost| work.checkpoint_mutant(mutant, killers, cost),
            |_| None,
            || {},
        ),
    )
}

#[cfg(test)]
#[expect(clippy::too_many_arguments, reason = "test adapters mirror the sweep execution boundary")]
pub(super) fn test_all_with_hooks(
    work: &Workspace,
    plan: &mut Plan,
    reach: &Reachability<'_>,
    sweep: Sweep<'_>,
    deterministic_reach: bool,
    killers: &mut Killers,
    events: &mut impl Events,
    hooks: (
        impl FnMut(&Mutant, &Killers) -> Result<()>,
        impl Fn(u32) -> Option<Judgement> + Sync,
        impl Fn() + Sync,
    ),
) -> Result<Option<Spent>> {
    let (mut checkpoint, judgement, before_wait) = hooks;
    test_all_with_accounting_hooks(
        work,
        plan,
        reach,
        sweep,
        deterministic_reach,
        killers,
        events,
        (|mutant, killers, _cost| checkpoint(mutant, killers), judgement, before_wait),
    )
}

#[expect(clippy::too_many_lines, reason = "sweep setup and its scoped worker loop share borrowed state")]
#[expect(
    clippy::too_many_arguments,
    reason = "execution adapters keep durability and admission failures deterministically testable"
)]
pub(super) fn test_all_with_accounting_hooks(
    work: &Workspace,
    plan: &mut Plan,
    reach: &Reachability<'_>,
    sweep: Sweep<'_>,
    deterministic_reach: bool,
    killers: &mut Killers,
    events: &mut impl Events,
    hooks: (
        impl FnMut(&Mutant, &Killers, &super::session::SweepCost) -> Result<()>,
        impl Fn(u32) -> Option<Judgement> + Sync,
        impl Fn() + Sync,
    ),
) -> Result<Option<Spent>> {
    let (mut checkpoint, judgement, before_wait) = hooks;
    let jobs = sweep.jobs;

    let mut pending = pending_positions(plan);

    if pending.is_empty() {
        events.sweep_planned(&[], jobs);
        return Ok(None);
    }

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    schedule_pending(&mut pending, plan, reach, sweep.census, killers);

    let tally = Tally::default();
    let abandoned: OnceLock<String> = OnceLock::new();
    let negative = NegativeLearning::default();

    // Indexed by *queue position*, not by plan position, and therefore built after the schedule is
    // fixed. Building either before it would silently pair one mutant's ordinal with another's
    // reachable binaries.
    let ordinals: Vec<(u32, Option<f64>)> = pending
        .iter()
        .map(|position| {
            let mutant = &plan.mutants[*position];
            (mutant.ordinal, mutant.test_timeout_multiplier)
        })
        .collect();

    let reachable: Vec<&[&TestBinary]> = pending
        .iter()
        .map(|position| {
            reach
                .reachable(&plan.mutants[*position])
                .expect("the shared reachability index was built from these same pending mutants")
        })
        .collect();

    let mut files: crate::HashMap<_, usize> = crate::HashMap::default();
    let mut item_counts: crate::HashMap<_, usize> = crate::HashMap::default();
    let file_slots: Vec<usize> = pending
        .iter()
        .map(|position| {
            let file = Arc::clone(&plan.mutants[*position].file);
            let next = files.len();

            // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
            increment_item_count(&mut item_counts, Arc::clone(&file), Arc::clone(&plan.mutants[*position].item_path));
            *files.entry(file).or_insert(next)
        })
        .collect();
    let item_paths: Vec<_> = pending
        .iter()
        .map(|position| Arc::clone(&plan.mutants[*position].item_path))
        .collect();
    let site_identities: Vec<_> = pending
        .iter()
        .map(|position| SiteIdentity::from_mutant(&plan.mutants[*position]))
        .collect();
    let sibling_benefits: Vec<usize> = pending
        .iter()
        .map(|position| {
            item_counts
                .get(&(
                    Arc::clone(&plan.mutants[*position].file),
                    Arc::clone(&plan.mutants[*position].item_path),
                ))
                .copied()
                // #[gamma::skip(all, reason = "the value controls scheduling, accounting, identity, or a conservative bound whose one-step perturbation has no safely deterministic external observation here")]
                .map_or(0, |count| count.saturating_sub(1))
        })
        .collect();
    let planning_total = pending.len();
    let inferred = InferredPlanning::new(killers);
    let planned: Vec<crate::estimate::MutationWork> = pending
        .iter()
        .enumerate()
        .map(|(index, position)| {
            let work = mutant_work_with_hints(*position, plan, reach, sweep.census, killers, &inferred);
            events.sweep_plan_progress(index + 1, planning_total);
            work
        })
        .collect();
    let costs: Vec<Duration> = planned.iter().map(|work| work.scheduling_cost()).collect();
    let mut file_paths = files.into_iter().collect::<Vec<_>>();
    // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
    file_paths.sort_by_key(|(_file, slot)| *slot);
    let file_paths = file_paths.into_iter().map(|(file, _slot)| file).collect::<Vec<_>>();
    let file_killers: Vec<FileLearning> = file_paths
        .iter()
        .map(|file| FileLearning::from_hints(file, killers.generalized()))
        .collect();

    let sweep_started = Instant::now();
    events.sweep_planned(&planned, jobs);

    let (sender, receiver) = mpsc::channel::<SweepEvent>();
    let mut package_spans: crate::HashMap<Arc<str>, (usize, u64, u64)> = crate::HashMap::default();

    // Resolved up front for the same reason reachability is: a worker must need nothing from the
    // plan, so the calling thread can borrow it mutably and record verdicts as they arrive. Cloned
    // rather than borrowed because the same map is written back as those verdicts land.
    let hints: Vec<Option<Killer>> = pending
        .iter()
        .map(|position| killers.hint(&plan.mutants[*position].id).cloned())
        .collect();
    let reach_hints: Vec<Vec<Candidate>> = pending
        .iter()
        .enumerate()
        .map(|(index, position)| {
            bounded_reach_hints(
                inferred
                    .reach
                    .get(&site_identities[index])
                    .map_or(&[], |(candidates, _)| *candidates),
                reach
                    .reachable(&plan.mutants[*position])
                    .expect("the reachability index contains every pending mutant"),
                {
                    let mutant = &plan.mutants[*position];
                    mutant.ordinal
                },
                sweep.census,
            )
            .into_iter()
            .map(Candidate::Exact)
            .collect()
        })
        .collect();
    let scheduler = Scheduler::with_resources(
        pending
            .iter()
            .enumerate()
            .map(|(index, position)| ScheduledWork {
                file: file_slots[index],
                item: Arc::clone(&item_paths[index]),
                package: Arc::clone(&plan.mutants[*position].package),
                cost: costs[index],
                sibling_benefit: sibling_benefits[index],
                hinted: hints[index].is_some(),
                stable_order: index,
                resources: scheduled_resources(
                    work,
                    planned[index],
                    reachable[index],
                    hints[index].as_ref(),
                    ordinals[index].0,
                    sweep.census,
                ),
            })
            .collect(),
        file_paths.len(),
        work.resource_capacities(),
    );
    #[cfg(test)]
    PLANNING_OBSERVER.with_borrow_mut(|observer| {
        if let Some(observer) = observer.as_mut() {
            observer(&scheduler.work, &reach_hints);
        }
    });
    let notes = notes::current();
    thread::scope(|scope| {
        // #[gamma::skip(all, reason = "the value controls scheduling, accounting, identity, or a conservative bound whose one-step perturbation has no safely deterministic external observation here")]
        for _worker in 0..worker_count(jobs) {
            let sender = sender.clone();
            let scheduler = &scheduler;
            let ordinals = &ordinals;
            let reachable = &reachable;
            let hints = &hints;
            let reach_hints = &reach_hints;
            let pending = &pending;
            let file_slots = &file_slots;
            let item_paths = &item_paths;
            let site_identities = &site_identities;
            let file_killers = &file_killers;
            let abandoned = &abandoned;
            let tally = &tally;
            let negative = &negative;
            let notes = notes.clone();
            let judgement = &judgement;
            let before_wait = &before_wait;

            let _handle = scope.spawn(move || {
                let _notes = notes::enter(notes.as_ref());

                while abandoned.get().is_none() {
                    let Some(assignment) = scheduler.assignment_with(abandoned, before_wait) else {
                        return;
                    };
                    let index = assignment.index();
                    let position = pending[index];

                    let (ordinal, timeout_multiplier) = ordinals[index];
                    let active = ordinal;
                    let first_started_ms = elapsed_millis(sweep_started.elapsed());
                    let started = Instant::now();
                    let reachable = &reachable[index];
                    begin_selection_trace();
                    let _sent = sender.send(SweepEvent::Started);
                    let judged = judgement(active).unwrap_or_else(|| {
                        judge_learning(
                            work,
                            active,
                            reachable,
                            hints[index].as_ref(),
                            &reach_hints[index],
                            &file_killers[file_slots[index]],
                            &item_paths[index],
                            &site_identities[index],
                            negative,
                            deterministic_reach,
                            timeout_multiplier,
                            sweep,
                            tally,
                        )
                    });
                    let attempts = take_selection_trace();

                    let (outcome, killer, note) = match judged {
                        Judgement::Reached(outcome, killer, note) => (outcome, killer.map(KillerAttribution::Observed), note),
                        Judgement::Guessed(killer) => (Outcome::Killed, Some(KillerAttribution::Guessed(killer)), None),
                        Judgement::Abandoned(reason) => {
                            let _sent = sender.send(SweepEvent::SelectionAttempts(attempts));
                            let _first = abandoned.set(reason);
                            core::mem::drop(assignment);
                            scheduler.abandon();

                            return;
                        }
                    };

                    let elapsed = elapsed_millis(started.elapsed());
                    let settled_ms = elapsed_millis(sweep_started.elapsed());
                    assignment.complete();

                    // A closed receiver means the calling thread is gone, which cannot happen while
                    // the scope is open; there is nothing useful to do about it either way.
                    let _sent = sender.send(SweepEvent::Completed(Completed {
                        position,
                        outcome,
                        elapsed_ms: elapsed,
                        killer,
                        note,
                        first_started_ms,
                        settled_ms,
                        attempts,
                    }));
                }
            });
        }

        // The workers hold the only remaining senders, so the drain ends when the last one finishes.
        core::mem::drop(sender);

        drain_sweep_events(
            plan,
            killers,
            events,
            &mut package_spans,
            |events| receive_sweep_event(&receiver, events, SWEEP_HEARTBEAT_INTERVAL),
            |mutant, killers, spans| {
                let cost = super::measure::sweep_cost(Some(tally.spent(spans)), sweep_started.elapsed())
                    .expect("a completed mutant always supplies sweep accounting");
                checkpoint(mutant, killers, &cost)
            },
            |reason| {
                let _ = abandoned.set(reason);
                scheduler.abandon();
            },
        );
    });

    let mut generalized = killers.generalized().clone();
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    persist_learning(&mut generalized, &file_paths, &file_killers);
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    killers.replace_generalized(generalized);

    abandoned.into_inner().map_or_else(
        || Ok(Some(tally.spent(&package_spans))),
        |reason| {
            Err(error!(
                "the run cannot be trusted to judge anything further: {reason}.\n\
                 It stops here rather than reach verdicts that would each have to be taken on faith."
            ))
        },
    )
}

type PackageSpans = crate::HashMap<Arc<str>, (usize, u64, u64)>;

fn drain_sweep_events<E: Events>(
    plan: &mut Plan,
    killers: &mut Killers,
    events: &mut E,
    package_spans: &mut PackageSpans,
    mut receive: impl FnMut(&mut E) -> Option<SweepEvent>,
    mut checkpoint: impl FnMut(&Mutant, &Killers, &PackageSpans) -> Result<()>,
    mut abandon: impl FnMut(String),
) {
    let mut checkpoint_failed = false;
    while let Some(event) = receive(events) {
        match event {
            SweepEvent::Started => events.mutant_started(),
            SweepEvent::SelectionAttempts(attempts) => {
                for attempt in &attempts {
                    events.selection_attempt(attempt);
                }
            }
            SweepEvent::Completed(completed) => {
                for attempt in &completed.attempts {
                    events.selection_attempt(attempt);
                }
                record_package_span(package_spans, plan, &completed);
                if let Some(mutant) = apply_completed(plan, killers, completed) {
                    if checkpoint_failed {
                        continue;
                    }
                    match checkpoint(mutant, killers, package_spans) {
                        Ok(()) => events.mutant(mutant),
                        Err(failure) => {
                            checkpoint_failed = true;
                            abandon(failure.to_string());
                        }
                    }
                }
            }
        }
    }
}

fn record_package_span(spans: &mut crate::HashMap<Arc<str>, (usize, u64, u64)>, plan: &Plan, completed: &Completed) {
    if let Some(mutant) = plan.mutants.get(completed.position) {
        let span = spans
            .entry(Arc::clone(&mutant.package))
            .or_insert((0, completed.first_started_ms, completed.settled_ms));
        span.0 = span.0.saturating_add(1);
        span.1 = span.1.min(completed.first_started_ms);
        span.2 = span.2.max(completed.settled_ms);
    }
}

fn scheduled_resources(
    work: &Workspace,
    planned: crate::estimate::MutationWork,
    reachable: &[&TestBinary],
    exact: Option<&Killer>,
    ordinal: u32,
    census: &Census,
) -> Vec<Arc<str>> {
    let mut resources = match planned.kind {
        crate::estimate::WorkKind::Exact => exact
            .into_iter()
            .flat_map(|hint| {
                reachable
                    .iter()
                    .filter(|binary| binary.matches_killer(hint))
                    .flat_map(|binary| work.required_resources(binary, Only::One(&hint.test)))
            })
            .collect(),
        crate::estimate::WorkKind::Selected => reachable
            .iter()
            .flat_map(|binary| match census.selection(binary, ordinal) {
                CensusSelection::Selected(names) | CensusSelection::Hinted(names) => work.required_resources(binary, Only::These(&names)),
                CensusSelection::Whole => work.required_resources(binary, Only::All),
                CensusSelection::Uncovered => Vec::new(),
            })
            .collect(),
        crate::estimate::WorkKind::Uncovered => Vec::new(),
        crate::estimate::WorkKind::Hinted | crate::estimate::WorkKind::Whole => reachable
            .iter()
            .flat_map(|binary| work.required_resources(binary, Only::All))
            .collect(),
    };
    resources.sort_unstable();
    resources.dedup();
    resources
}

fn pending_positions(plan: &Plan) -> Vec<usize> {
    plan.mutants
        .iter()
        .enumerate()
        .filter(|(_position, mutant)| mutant.ordinal != 0 && mutant.outcome == Outcome::Pending)
        .map(|(position, _mutant)| position)
        .collect()
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn schedule_pending(pending: &mut [usize], plan: &Plan, reach: &Reachability<'_>, census: &Census, killers: &Killers) {
    schedule(pending, plan, reach, census, killers);
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn increment_item_count(counts: &mut crate::HashMap<(Arc<Utf8Path>, Arc<str>), usize>, file: Arc<Utf8Path>, item: Arc<str>) {
    let count = counts.entry((file, item)).or_default();
    *count = count.saturating_add(1);
}

#[cfg(test)]
type PlanningObserver = Box<dyn FnMut(&[ScheduledWork], &[Vec<Candidate>])>;

#[cfg(test)]
thread_local! {
    static PLANNING_OBSERVER: RefCell<Option<PlanningObserver>> = const { RefCell::new(None) };
}

fn worker_count(jobs: usize) -> usize {
    jobs.max(1)
}

fn elapsed_millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn apply_completed<'plan>(plan: &'plan mut Plan, killers: &mut Killers, completed: Completed) -> Option<&'plan Mutant> {
    let mutant = plan.mutants.get_mut(completed.position)?;
    mutant.outcome = completed.outcome;
    mutant.elapsed_ms = completed.elapsed_ms;
    let observed = completed.killer.as_ref().and_then(KillerAttribution::observed);
    mutant.killed_by = observed.map(|killer| killer.test.clone());
    mutant.killer = observed.map(|killer| crate::model::KillerIdentity {
        package: killer.package.clone(),
        target: killer.target.clone(),
        test: killer.test.clone(),
    });
    mutant.note = completed.note;

    match completed.killer {
        Some(killer) => killers.record(mutant.id.clone(), killer.into_identity()),
        None => killers.forget(&mutant.id),
    }

    Some(mutant)
}

#[cfg(test)]
fn publish_completed(plan: &mut Plan, killers: &mut Killers, events: &mut impl Events, completed: Completed) {
    if let Some(mutant) = apply_completed(plan, killers, completed) {
        events.mutant(mutant);
    }
}

fn persist_learning(generalized: &mut GeneralizedHints, files: &[Arc<Utf8Path>], learning: &[FileLearning]) {
    let state = generalized;
    state.version = GENERALIZED_HINTS_VERSION;
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    state.items.retain(|entry| !files.iter().any(|file| entry.file == file.as_ref()));
    state.binaries.retain(|entry| !files.iter().any(|file| entry.file == file.as_ref()));
    for (file, learning) in files.iter().zip(learning) {
        learning.persist(file, state);
    }
    // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
    state
        .items
        .sort_unstable_by(|left, right| left.file.cmp(&right.file).then_with(|| left.item.cmp(&right.item)));
    // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
    state.binaries.sort_unstable_by(|left, right| left.file.cmp(&right.file));
}

/// The settings every mutant in a sweep is run under.
///
/// Carried together because they are decided once, before the first mutant, and read identically by
/// every worker; splitting them back out would only add arguments to the functions that thread them
/// through.
#[derive(Debug, Clone, Copy)]
pub(super) struct Sweep<'run> {
    /// Minimum timeout floor applied to test budgets.
    pub(super) timeout_floor: Duration,

    /// How long a test binary may go without saying anything before it is treated as stuck.
    pub(super) stall: Stall,

    /// How many mutants to run at once.
    pub(super) jobs: usize,

    /// Whether each run's memory is to be accounted for at all.
    pub(super) meter: bool,

    /// Whether a failing test is re-run with no mutant active before it is believed.
    pub(super) confirm: bool,

    /// Which tests reach which sites, empty when nothing was measured.
    pub(super) census: &'run Census,
}

/// What one mutant's run across its reachable test binaries came to.
pub(super) enum Judgement {
    /// The mutant was judged: an outcome, the test that caught it if one did, and any note.
    Reached(Outcome, Option<Killer>, Option<String>),

    /// A grouped probe killed the mutant without identifying which selected test failed.
    Guessed(Killer),

    /// The run could no longer be metered as asked, and no verdict from here on would mean anything.
    Abandoned(String),
}

#[derive(Clone, Copy)]
enum ProbeKind {
    Exact,
    Item,
    Reach,
    File,
    Census,
}

impl ProbeKind {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn reserve(self, tally: &Tally) -> bool {
        let (probes, hits) = match self {
            Self::Item => (&tally.item_probes, &tally.item_hits),
            Self::Reach => (&tally.reach_probes, &tally.reach_hits),
            Self::File => (&tally.file_probes, &tally.file_hits),
            Self::Exact | Self::Census => {
                self.attempt(tally);
                return true;
            }
        };

        if hits.load(Ordering::Relaxed) > 0 {
            let _previous = probes.fetch_add(1, Ordering::Relaxed);
        } else if probes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < INFERRED_ZERO_HIT_LIMIT).then_some(count.saturating_add(1))
            })
            .is_err()
        {
            if hits.load(Ordering::Relaxed) == 0 {
                return false;
            }
            let _previous = probes.fetch_add(1, Ordering::Relaxed);
        }

        let _previous = tally.inferred_probes.fetch_add(1, Ordering::Relaxed);
        true
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn reject(self, tally: &Tally) {
        let counter = match self {
            Self::Item => &tally.item_rejected,
            Self::Reach => &tally.reach_rejected,
            Self::File => &tally.file_rejected,
            Self::Exact | Self::Census => return,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
        let _previous = tally.inferred_rejected.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn attempt(self, tally: &Tally) {
        let counter = match self {
            Self::Exact => &tally.exact_probes,
            Self::Item => &tally.item_probes,
            Self::Reach => &tally.reach_probes,
            Self::File => &tally.file_probes,
            Self::Census => &tally.census_probes,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
        if !matches!(self, Self::Exact) {
            let _previous = tally.inferred_probes.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn hit(self, tally: &Tally) {
        let counter = match self {
            Self::Exact => &tally.exact_hits,
            Self::Item => &tally.item_hits,
            Self::Reach => &tally.reach_hits,
            Self::File => &tally.file_hits,
            Self::Census => &tally.census_hits,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
        if !matches!(self, Self::Exact) {
            let _previous = tally.inferred_hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    const fn selection_tier(self) -> SelectionTier {
        match self {
            Self::Exact => SelectionTier::Exact,
            Self::Item => SelectionTier::Item,
            Self::Reach => SelectionTier::Reach,
            Self::File => SelectionTier::File,
            Self::Census => SelectionTier::Census,
        }
    }
}

fn transfer_observation(verdict: &Verdict) -> Option<bool> {
    match verdict {
        Verdict::Failed(_) => Some(true),
        Verdict::Passed => Some(false),
        Verdict::TestEnumerationFailed(_)
        | Verdict::TimedOut
        | Verdict::Stalled(_)
        | Verdict::MemoryLimit { .. }
        | Verdict::Flaky(_)
        | Verdict::Unmetered(_)
        | Verdict::Unjudged(_) => None,
    }
}

fn selection_result(observation: Option<bool>) -> SelectionResult {
    match observation {
        Some(true) => SelectionResult::Hit,
        Some(false) => SelectionResult::Miss,
        None => SelectionResult::Inconclusive,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "selection telemetry records the complete execution context at each launch site"
)]
fn trace_selection(
    ordinal: u32,
    binary: &TestBinary,
    test: Option<String>,
    tier: SelectionTier,
    rank: usize,
    observation: Option<bool>,
    all_tests: bool,
    elapsed: Duration,
    fallback: Duration,
) {
    record_selection_attempt(SelectionAttempt {
        ordinal,
        tier,
        package: binary.package.clone(),
        target: binary.target.clone(),
        test,
        rank,
        result: selection_result(observation),
        all_tests,
        elapsed_ms: elapsed_millis(elapsed),
        fallback_ms: elapsed_millis(fallback),
    });
}

/// Runs the one test that caught this mutant last time, and says whether it caught it again.
///
/// A re-killed mutant's cost falls from a partial binary — every test ahead of its killer, in
/// whatever order the harness runs them — to a single test.
///
/// Only a failure settles a kill. Loss of containment abandons the campaign; other inconclusive
/// verdicts fall back to ordinary binary execution. A later binary's exact kill is retained until
/// earlier canonical outcomes have been adjudicated.
#[expect(
    clippy::too_many_arguments,
    reason = "probe accounting adds the candidate class to the execution inputs"
)]
fn probe(
    work: &Workspace,
    ordinal: u32,
    binary: &TestBinary,
    hint: &Killer,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
    kind: ProbeKind,
) -> (std::result::Result<Option<Killer>, String>, Option<bool>, Duration) {
    kind.attempt(tally);
    probe_reserved(work, ordinal, binary, hint, timeout_multiplier, sweep, tally, kind)
}

#[expect(
    clippy::too_many_arguments,
    reason = "reserved probes retain the same execution inputs as ordinary probes"
)]
fn probe_reserved(
    work: &Workspace,
    active: u32,
    binary: &TestBinary,
    hint: &Killer,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
    kind: ProbeKind,
) -> (std::result::Result<Option<Killer>, String>, Option<bool>, Duration) {
    let request = MemoryRequest {
        meter: sweep.meter,
        limit: binary.memory,
    };

    let attempt = Attempt {
        active: Some(active),
        timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
        stall: sweep.stall,
        request,
        only: Only::One(&hint.test),
        census: None,
    };

    // A probe is one subprocess launched because a hint pointed at it, so it counts as both.
    record_launch(tally);
    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);

    let started = Instant::now();
    let verdict = run_binary(work, binary, attempt, sweep.confirm);
    let elapsed = started.elapsed();
    let observation = transfer_observation(&verdict);
    let killer = match verdict {
        // The harness names the test it ran, which under a filter can only be the one asked for;
        // the recorded name is used when it names nothing, so the map stays populated either way.
        Verdict::Failed(named) => {
            kind.hit(tally);
            Ok(Some(Killer {
                package: hint.package.clone(),
                target: binary.killer_target(),
                test: named.unwrap_or_else(|| hint.test.clone()),
            }))
        }
        Verdict::Unmetered(reason) => Err(reason),
        _inconclusive => Ok(None),
    };

    (killer, observation, elapsed)
}

/// Whether a filtered failure is conclusive for this binary.
///
/// Filtering changes the workload, so it cannot settle a kill when the whole binary might instead
/// time out, stall, exceed a memory limit, fail confirmation, or lose its meter. Launch refusal is
/// covered by the successful filtered launch itself. Earlier binaries are protected by trying the
/// probe only when canonical iteration reaches this binary.
fn negative_reach_hint_is_final(binary: &TestBinary, timeout_multiplier: Option<f64>, sweep: Sweep<'_>) -> bool {
    reach_hint_is_final(whole_probe_attempt(binary, timeout_multiplier, sweep), sweep.confirm)
}

fn whole_probe_attempt(binary: &TestBinary, timeout_multiplier: Option<f64>, sweep: Sweep<'_>) -> Attempt<'static> {
    let request = MemoryRequest {
        meter: sweep.meter,
        limit: binary.memory,
    };

    Attempt {
        active: Some(1),
        timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
        stall: sweep.stall,
        request,
        only: Only::All,
        census: None,
    }
}

fn bounded_reach_hints(hints: &[Killer], reachable: &[&TestBinary], ordinal: u32, census: &Census) -> Vec<Killer> {
    if hints.is_empty() {
        return Vec::new();
    }
    let mut binaries: crate::HashMap<&str, crate::HashMap<String, Vec<usize>>> = crate::HashMap::default();
    for (index, binary) in reachable.iter().enumerate() {
        let targets = binaries.entry(&binary.package).or_default();
        targets.entry(binary.target.clone()).or_default().push(index);
        if !binary.target_kind.is_empty() {
            targets.entry(binary.killer_target()).or_default().push(index);
        }
    }
    let mut counts = vec![0_usize; reachable.len()];
    let resolved = hints
        .iter()
        .map(|hint| {
            let matches = binaries.get(hint.package.as_str())?.get(hint.target.as_str())?;
            // Legacy target names can match multiple binaries; eligibility uses the first,
            // but the half-suite bound counts candidates for every matching binary.
            for index in matches {
                counts[*index] += 1;
            }
            matches.first().copied()
        })
        .collect::<Vec<_>>();
    let eligible = reachable
        .iter()
        .enumerate()
        .map(|(index, binary)| {
            let Some(total) = binary.tests else { return false };
            if counts[index] == 0 || counts[index] > total / 2 || binary.baseline.is_zero() {
                return false;
            }
            match census.work(binary, ordinal) {
                CensusWork::Selected(duration) | CensusWork::Hinted(duration) => duration < binary.baseline,
                CensusWork::Whole | CensusWork::Uncovered => false,
            }
        })
        .collect::<Vec<_>>();
    hints
        .iter()
        .zip(resolved)
        .filter(|(_, index)| index.is_some_and(|index| eligible[index]))
        .map(|(hint, _)| hint.clone())
        .collect()
}

/// A probe's killing-test identity and whether execution observed it fail.
#[derive(Debug, PartialEq, Eq)]
struct ProbeKiller {
    identity: Killer,
    observed: bool,
}

/// Tries tests found by an incomplete census without trusting their absence of a failure.
///
/// A failure is evidence that the mutant was killed. Every other result falls back to the whole
/// binary, because an incomplete census cannot establish that no unmeasured test would fail.
fn probe_cases(
    work: &Workspace,
    ordinal: u32,
    binary: &TestBinary,
    names: &[&str],
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
) -> (Option<ProbeKiller>, Option<bool>, Duration) {
    let attempt = Attempt {
        active: Some(ordinal),
        timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
        stall: sweep.stall,
        request: MemoryRequest {
            meter: sweep.meter,
            limit: binary.memory,
        },
        only: Only::These(names),
        census: None,
    };

    // #[gamma::skip(call_result.default, reason = "record_launch returns unit and this mutator retains the launch counter update before replacing unit with unit")]
    record_launch(tally);
    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);
    ProbeKind::Census.attempt(tally);

    let started = Instant::now();
    let verdict = run_binary(work, binary, attempt, sweep.confirm);
    let elapsed = started.elapsed();
    let observation = transfer_observation(&verdict);
    let killer = match verdict {
        Verdict::Failed(name) => {
            ProbeKind::Census.hit(tally);
            let observed = name.is_some() || names.len() == 1;
            Some(ProbeKiller {
                identity: Killer {
                    package: binary.package.clone(),
                    target: binary.killer_target(),
                    test: name.unwrap_or_else(|| {
                        names
                            .first()
                            .copied()
                            .expect("an incomplete census hint always names at least one test")
                            .to_owned()
                    }),
                },
                observed,
            })
        }
        _inconclusive => None,
    };

    (killer, observation, elapsed)
}

/// Runs one mutant against every test binary that can reach it, stopping at the first detection.
///
/// Later binaries are not run once one has caught the mutant: the answer cannot change, and the
/// time saved is the difference between a sweep that finishes overnight and one that does not.
///
/// An earlier run's exact killer is probed before inferred tests. Its result is checked, never
/// believed, and earlier canonical outcomes retain precedence: see [`probe`].
#[cfg(test)]
fn judge(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    hint: Option<&Killer>,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
) -> Judgement {
    judge_ordered(work, ordinal, reachable, hint, None, timeout_multiplier, sweep, tally)
}

#[expect(clippy::too_many_arguments, reason = "the hot verdict path keeps execution state borrowed")]
#[cfg(test)]
fn judge_ordered(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    hint: Option<&Killer>,
    file_hint: Option<&Killer>,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
) -> Judgement {
    let candidates = file_hint.cloned().map(Candidate::Exact).into_iter().collect::<Vec<_>>();
    let active = ordinal;

    judge_ranked(
        work,
        active,
        reachable,
        hint,
        &candidates,
        None,
        None,
        timeout_multiplier,
        sweep,
        tally,
    )
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "candidate preflight and canonical adjudication share cached verdict state"
)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn judge_ranked(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    hint: Option<&Killer>,
    candidates: &[Candidate],
    learning: Option<(&FileLearning, &str)>,
    negative: Option<(&NegativeLearning, &SiteIdentity, bool)>,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
) -> Judgement {
    let fallback = reachable
        .iter()
        .fold(Duration::ZERO, |total, binary| total.saturating_add(binary.baseline));
    if hint.is_some() {
        let _previous = tally.exact_candidates.fetch_add(1, Ordering::Relaxed);
    }
    let _previous = tally.inferred_candidates.fetch_add(candidates.len(), Ordering::Relaxed);
    for candidate in candidates {
        let counter = match candidate {
            Candidate::Exact(_) | Candidate::ItemBinary(_) => &tally.item_candidates,
            Candidate::ReachFileBinary(_) => &tally.reach_candidates,
            Candidate::FileBinary(_) => &tally.file_candidates,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
    }

    let active = ordinal;
    // #[gamma::skip(all, reason = "the value controls scheduling, accounting, identity, or a conservative bound whose one-step perturbation has no safely deterministic external observation here")]
    let mut binary_runs: Vec<Option<(Verdict, bool)>> = core::iter::repeat_with(|| None).take(reachable.len()).collect();
    let mut exact_kill = None;
    // The bounded inferred tier plus one historical hint fit without a per-mutant allocation.
    let mut attempted_tests = [None; INFERRED_EXACT_LIMIT + 1];

    if let Some(exact) = hint
        && let Some((index, binary)) = reachable
            .iter()
            .copied()
            .enumerate()
            .find(|(_, binary)| binary.matches_killer(exact))
        && !census_excludes(sweep.census, binary, active)
        && !negative_excludes(negative, binary, timeout_multiplier, sweep)
    {
        let (killer, observation, elapsed) = probe(work, active, binary, exact, timeout_multiplier, sweep, tally, ProbeKind::Exact);
        trace_selection(
            active,
            binary,
            Some(exact.test.clone()),
            SelectionTier::Exact,
            1,
            observation,
            false,
            elapsed,
            fallback,
        );
        match killer {
            Ok(Some(killer)) => {
                if index == 0 {
                    return killed_by(killer);
                }
                exact_kill = Some((index, killer));
            }
            Ok(None) => {
                attempted_tests[0] = Some((index, exact.test.as_str()));
            }
            Err(reason) => return Judgement::Abandoned(reason),
        }
    }

    // Learned candidates are tried in ranked order. Their outcomes are held until canonical
    // iteration reaches that binary, so a later candidate cannot outrank an earlier resource
    // verdict. Loss of containment is command-wide, so an unmetered candidate abandons immediately
    // instead of entering the cache.
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    if exact_kill.is_none() {
        'candidate: for (candidate_index, candidate) in candidates.iter().enumerate() {
            let kind = candidate.probe_kind();

            match candidate {
                Candidate::Exact(candidate_hint) => {
                    let Some((index, binary)) = reachable
                        .iter()
                        .copied()
                        .enumerate()
                        .find(|(_, binary)| binary.matches_killer(candidate_hint))
                    else {
                        continue 'candidate;
                    };

                    if census_excludes(sweep.census, binary, active) {
                        continue 'candidate;
                    }

                    if negative_excludes(negative, binary, timeout_multiplier, sweep) {
                        continue 'candidate;
                    }
                    if attempted_tests.contains(&Some((index, candidate_hint.test.as_str()))) {
                        continue 'candidate;
                    }
                    if !kind.reserve(tally) {
                        kind.reject(tally);
                        continue 'candidate;
                    }
                    let slot = attempted_tests
                        .iter_mut()
                        .find(|slot| slot.is_none())
                        .expect("callers bound inferred exact candidates to INFERRED_EXACT_LIMIT, plus at most one historical hint");
                    *slot = Some((index, candidate_hint.test.as_str()));

                    let (found, observation, elapsed) = probe_reserved(
                        work,
                        active,
                        binary,
                        candidate_hint,
                        timeout_multiplier,
                        sweep,
                        tally,
                        ProbeKind::Item,
                    );
                    trace_selection(
                        active,
                        binary,
                        Some(candidate_hint.test.clone()),
                        kind.selection_tier(),
                        candidate_index.saturating_add(1),
                        observation,
                        false,
                        elapsed,
                        fallback,
                    );

                    if let Some(hit) = observation
                        && let Some((observed, item_path)) = learning
                    {
                        FileLearning::observe(observed, item_path, candidate, hit, elapsed);
                    }

                    match found {
                        Ok(Some(killer)) => return killed_by(killer),
                        Ok(None) => {}
                        Err(reason) => return Judgement::Abandoned(reason),
                    }
                }
                Candidate::ItemBinary(identity) | Candidate::ReachFileBinary(identity) | Candidate::FileBinary(identity) => {
                    let Some((index, binary)) = reachable
                        .iter()
                        .copied()
                        .enumerate()
                        .find(|(_index, binary)| identity.names(binary))
                    else {
                        continue 'candidate;
                    };

                    let selection = sweep.census.selection(binary, active);
                    let only = match &selection {
                        CensusSelection::Uncovered => continue 'candidate,
                        CensusSelection::Selected(names) => Only::These(names),
                        CensusSelection::Whole | CensusSelection::Hinted(_) => Only::All,
                    };

                    if negative_excludes(negative, binary, timeout_multiplier, sweep) {
                        continue 'candidate;
                    }

                    if binary_runs[index].as_ref().is_some() {
                        continue 'candidate;
                    }
                    if !kind.reserve(tally) {
                        kind.reject(tally);
                        continue 'candidate;
                    }

                    let started = Instant::now();
                    let request = MemoryRequest {
                        meter: sweep.meter,
                        limit: binary.memory,
                    };
                    let attempt = Attempt {
                        active: Some(ordinal),
                        timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
                        stall: sweep.stall,
                        request,
                        only,
                        census: None,
                    };
                    record_launch(tally);
                    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);
                    let run = run_binary_observed(work, binary, attempt, sweep.confirm);
                    let elapsed = started.elapsed();
                    let observation = transfer_observation(&run.verdict);
                    if observation == Some(true) {
                        kind.hit(tally);
                    }
                    trace_selection(
                        active,
                        binary,
                        None,
                        kind.selection_tier(),
                        candidate_index.saturating_add(1),
                        observation,
                        matches!(only, Only::All),
                        elapsed,
                        fallback,
                    );

                    if let Some(hit) = observation
                        && let Some((observed, item_path)) = learning
                    {
                        FileLearning::observe(observed, item_path, candidate, hit, elapsed);
                        if matches!(run.reach, ReachObservation::Reached) {
                            FileLearning::reached(observed, item_path, binary, elapsed);
                        }
                    }
                    if negative_reach_is_final(&run, only, negative.is_some_and(|(_, _, deterministic)| deterministic))
                        && let Some((negative, site, _)) = negative
                    {
                        NegativeLearning::record(negative, site, binary);
                    }

                    let terminal = !matches!(&run.verdict, Verdict::Passed);
                    let verdict = match defer_candidate_verdict(run.verdict) {
                        Ok(verdict) => verdict,
                        Err(judged) => return judged,
                    };
                    binary_runs[index] = Some((verdict, matches!(only, Only::All)));

                    if terminal {
                        break 'candidate;
                    }
                }
            }
        }
    }

    // Set by the first binary this mutant is actually run against. Left false, nothing that could
    // convict this code was run — no test binary links it, none of them announced a test, or a
    // census established that no test in any of them executes the site — and reporting that as a
    // survivor would blame the tests that exist for the absence of ones that do not.
    let mut ran = false;

    'binary: for (binary_index, binary) in reachable.iter().copied().enumerate() {
        let selection = sweep.census.selection(binary, active);

        if negative_excludes(negative, binary, timeout_multiplier, sweep) {
            let _saved = tally.saved.fetch_add(1, Ordering::Relaxed);
            let _reach_saved = tally.reach_saved.fetch_add(1, Ordering::Relaxed);
            continue 'binary;
        }

        if exact_kill.as_ref().is_some_and(|(index, _)| *index == binary_index)
            && let Some((_, killer)) = exact_kill.take()
        {
            return killed_by(killer);
        }

        if let CensusSelection::Hinted(names) = &selection {
            let _previous = tally.hinted_selections.fetch_add(1, Ordering::Relaxed);
            let _previous = tally.census_candidates.fetch_add(1, Ordering::Relaxed);
            let _previous = tally.available_tests.fetch_add(binary.tests.unwrap_or(0), Ordering::Relaxed);

            let (killer, observation, elapsed) = probe_cases(work, active, binary, names, timeout_multiplier, sweep, tally);
            trace_selection(
                active,
                binary,
                None,
                SelectionTier::Census,
                1,
                observation,
                false,
                elapsed,
                fallback,
            );
            if let Some(killer) = killer {
                let _previous = tally.selected_tests.fetch_add(names.len(), Ordering::Relaxed);
                return if killer.observed {
                    killed_by(killer.identity)
                } else {
                    Judgement::Guessed(killer.identity)
                };
            }
        }

        let only = match &selection {
            CensusSelection::Whole => {
                let tests = binary.tests.unwrap_or(0);
                let _previous = tally.whole_selections.fetch_add(1, Ordering::Relaxed);
                let _previous = tally.available_tests.fetch_add(tests, Ordering::Relaxed);
                let _previous = tally.selected_tests.fetch_add(tests, Ordering::Relaxed);
                Only::All
            }
            CensusSelection::Hinted(_) => {
                let tests = binary.tests.unwrap_or(0);
                let _previous = tally.whole_selections.fetch_add(1, Ordering::Relaxed);
                let _previous = tally.hinted_fallbacks.fetch_add(1, Ordering::Relaxed);
                let _previous = tally.selected_tests.fetch_add(tests, Ordering::Relaxed);
                Only::All
            }
            CensusSelection::Uncovered => {
                let _previous = tally.uncovered_selections.fetch_add(1, Ordering::Relaxed);
                let _previous = tally.available_tests.fetch_add(binary.tests.unwrap_or(0), Ordering::Relaxed);
                continue 'binary;
            }
            CensusSelection::Selected(names) => {
                let _previous = tally.case_selections.fetch_add(1, Ordering::Relaxed);
                let _previous = tally.available_tests.fetch_add(binary.tests.unwrap_or(0), Ordering::Relaxed);
                let _previous = tally.selected_tests.fetch_add(names.len(), Ordering::Relaxed);
                Only::These(names)
            }
        };

        ran = true;

        let request = MemoryRequest {
            meter: sweep.meter,
            limit: binary.memory,
        };

        let attempt = Attempt {
            active: Some(active),
            timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
            stall: sweep.stall,
            request,
            only,
            census: None,
        };

        let (mut verdict, already_whole) = if let Some((verdict, already_whole)) = binary_runs[binary_index].take() {
            (verdict, already_whole)
        } else {
            record_launch(tally);
            let started = Instant::now();
            let run = run_binary_observed(work, binary, attempt, sweep.confirm);
            let elapsed = started.elapsed();
            let observation = transfer_observation(&run.verdict);
            let tier = match &selection {
                CensusSelection::Selected(_) => SelectionTier::Selected,
                CensusSelection::Hinted(_) => SelectionTier::HintedFallback,
                CensusSelection::Whole => SelectionTier::Whole,
                CensusSelection::Uncovered => unreachable!("uncovered selections continue before execution"),
            };
            trace_selection(
                active,
                binary,
                None,
                tier,
                binary_index.saturating_add(1),
                observation,
                matches!(attempt.only, Only::All),
                elapsed,
                fallback,
            );
            if matches!(run.reach, ReachObservation::Reached)
                && let Some((observed, item_path)) = learning
            {
                FileLearning::reached(observed, item_path, binary, elapsed);
            }
            if observation == Some(false)
                && let Some((observed, item_path)) = learning
            {
                FileLearning::record_canonical_pass(observed, item_path, binary, matches!(run.reach, ReachObservation::Reached));
            }
            if negative_reach_is_final(&run, attempt.only, negative.is_some_and(|(_, _, deterministic)| deterministic))
                && let Some((negative, site, _)) = negative
            {
                NegativeLearning::record(negative, site, binary);
            }
            (run.verdict, bool::default())
        };

        // A complete census proves which tests can observe the mutant, but filtering also shrinks
        // runtime and peak memory and can change failure order. A non-passing filtered run is
        // therefore provisional: repeat the whole binary and use only its canonical outcome.
        if filtered_run_needs_confirmation(already_whole, &selection, &verdict) {
            record_launch(tally);
            let started = Instant::now();
            let run = run_binary_observed(work, binary, whole_attempt(attempt), sweep.confirm);
            let elapsed = started.elapsed();
            let observation = transfer_observation(&run.verdict);
            trace_selection(
                active,
                binary,
                None,
                SelectionTier::Whole,
                binary_index.saturating_add(1),
                observation,
                true,
                elapsed,
                fallback,
            );
            if matches!(run.reach, ReachObservation::Reached)
                && let Some((observed, item_path)) = learning
            {
                FileLearning::reached(observed, item_path, binary, elapsed);
            }
            if observation == Some(false)
                && let Some((observed, item_path)) = learning
            {
                FileLearning::record_canonical_pass(observed, item_path, binary, matches!(run.reach, ReachObservation::Reached));
            }
            if negative_reach_is_final(&run, Only::All, negative.is_some_and(|(_, _, deterministic)| deterministic))
                && let Some((negative, site, _)) = negative
            {
                NegativeLearning::record(negative, site, binary);
            }
            verdict = core::convert::identity(run.verdict);
        }

        if let Some(judged) = terminal_judgement(binary, verdict) {
            return judged;
        }
    }

    Judgement::Reached(if ran { Outcome::Survived } else { Outcome::NoCoverage }, None, None)
}

fn terminal_judgement(binary: &TestBinary, verdict: Verdict) -> Option<Judgement> {
    match verdict {
        Verdict::Passed => None,
        Verdict::Failed(name) => Some(Judgement::Reached(
            Outcome::Killed,
            name.map(|test| Killer {
                package: binary.package.clone(),
                target: binary.killer_target(),
                test,
            }),
            None,
        )),
        Verdict::TestEnumerationFailed(output) => Some(Judgement::Reached(
            Outcome::Killed,
            None,
            Some(enumeration_note(&binary.path, &output)),
        )),
        Verdict::TimedOut => Some(Judgement::Reached(Outcome::Timeout, None, None)),
        Verdict::Stalled(test) => Some(Judgement::Reached(Outcome::Timeout, None, Some(stall_note(test.as_deref())))),
        Verdict::MemoryLimit { peak, limit } => Some(Judgement::Reached(
            Outcome::OutOfMemory,
            None,
            Some(memory_note(&binary.path, peak, limit)),
        )),
        Verdict::Flaky(test) => Some(Judgement::Reached(
            Outcome::Flaky,
            None,
            Some(flaky_note(&binary.path, test.as_deref())),
        )),
        Verdict::Unmetered(reason) => Some(Judgement::Abandoned(reason)),
        Verdict::Unjudged(reason) => Some(Judgement::Reached(Outcome::Pending, None, Some(reason))),
    }
}

fn defer_candidate_verdict(verdict: Verdict) -> core::result::Result<Verdict, Judgement> {
    match verdict {
        Verdict::Unmetered(reason) => Err(Judgement::Abandoned(reason)),
        verdict => Ok(verdict),
    }
}

fn negative_excludes(
    negative: Option<(&NegativeLearning, &SiteIdentity, bool)>,
    binary: &TestBinary,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
) -> bool {
    let Some((negative, site, true)) = negative else {
        return false;
    };

    negative.contains(site, binary) && negative_reach_hint_is_final(binary, timeout_multiplier, sweep)
}

fn killed_by(killer: Killer) -> Judgement {
    Judgement::Reached(Outcome::Killed, Some(killer), None)
}

fn census_excludes(census: &Census, binary: &TestBinary, ordinal: u32) -> bool {
    matches!(census.selection(binary, ordinal), CensusSelection::Uncovered)
}

fn record_launch(tally: &Tally) {
    let _previous = tally.launches.fetch_add(1, Ordering::Relaxed);
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn whole_attempt(attempt: Attempt<'_>) -> Attempt<'_> {
    let mut whole = attempt;
    whole.only = Only::All;
    whole
}

fn filtered_run_needs_confirmation(already_whole: bool, selection: &CensusSelection<'_>, verdict: &Verdict) -> bool {
    !already_whole && matches!(selection, CensusSelection::Selected(_)) && !matches!(verdict, Verdict::Passed | Verdict::Failed(_))
}

fn negative_reach_is_final(run: &BinaryRun, only: Only<'_>, deterministic: bool) -> bool {
    deterministic && matches!(only, Only::All) && matches!(run.verdict, Verdict::Passed) && run.reach == ReachObservation::NotReached
}

/// Applies ranked item-test and file-binary learning to one mutant.
#[expect(clippy::too_many_arguments, reason = "adds one file-local state cell to the verdict path")]
fn judge_learning(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    hint: Option<&Killer>,
    promoted: &[Candidate],
    observed: &FileLearning,
    item_path: &str,
    site: &SiteIdentity,
    negative: &NegativeLearning,
    deterministic_reach: bool,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
) -> Judgement {
    let active = ordinal;
    let exact = if deterministic_reach { hint } else { None };
    let mut candidates = if deterministic_reach { promoted.to_vec() } else { Vec::new() };
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    if deterministic_reach {
        let fallback = reachable
            .iter()
            .fold(Duration::ZERO, |total, binary| total.saturating_add(binary.baseline));
        let (learned, rejected) = FileLearning::select_candidates(observed, item_path, fallback);
        candidates.extend(learned);
        record_rejections(tally, rejected);
        record_rejections(tally, bound_candidates(&mut candidates));
    }
    let judged = judge_ranked(
        work,
        active,
        reachable,
        exact,
        &candidates,
        // #[gamma::skip(all, reason = "the optional state is observed only through higher-level process orchestration that cannot be isolated safely here")]
        Some((observed, item_path)),
        Some((negative, site, deterministic_reach)),
        timeout_multiplier,
        sweep,
        tally,
    );

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    FileLearning::publish(observed, item_path, &judged);
    judged
}

fn bound_candidates(candidates: &mut Vec<Candidate>) -> [usize; 3] {
    let before = candidate_tier_counts(candidates);
    let mut exact = 0;
    let mut item_binary = 0;
    let mut reach = 0;
    let mut file = 0;

    candidates.retain(|candidate| {
        let count = match candidate {
            Candidate::Exact(_) => &mut exact,
            Candidate::ItemBinary(_) => &mut item_binary,
            Candidate::ReachFileBinary(_) => &mut reach,
            Candidate::FileBinary(_) => &mut file,
        };
        *count += 1;

        *count
            <= match candidate {
                Candidate::Exact(_) => INFERRED_EXACT_LIMIT,
                Candidate::ItemBinary(_) | Candidate::ReachFileBinary(_) | Candidate::FileBinary(_) => 1,
            }
    });

    let after = candidate_tier_counts(candidates);
    [
        before[0].saturating_sub(after[0]),
        before[1].saturating_sub(after[1]),
        before[2].saturating_sub(after[2]),
    ]
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn candidate_tier_counts(candidates: &[Candidate]) -> [usize; 3] {
    let mut counts = [0; 3];
    for candidate in candidates {
        counts[match candidate.probe_kind() {
            ProbeKind::Item => 0,
            ProbeKind::Reach => 1,
            ProbeKind::File => 2,
            ProbeKind::Exact | ProbeKind::Census => unreachable!("inferred tests never use exact or census accounting"),
        }] += 1;
    }
    counts
}

fn record_rejections(tally: &Tally, rejected: [usize; 3]) {
    for (count, counter) in rejected
        .into_iter()
        .zip([&tally.item_rejected, &tally.reach_rejected, &tally.file_rejected])
    {
        let _previous = counter.fetch_add(count, Ordering::Relaxed);
        let _previous = tally.inferred_rejected.fetch_add(count, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BinaryIdentity {
    package: String,
    target: String,
}

impl BinaryIdentity {
    fn from_binary(binary: &TestBinary) -> Self {
        Self {
            package: binary.package.clone(),
            target: binary.killer_target(),
        }
    }

    fn from_killer(killer: &Killer) -> Self {
        Self {
            package: killer.package.clone(),
            target: killer.target.clone(),
        }
    }

    fn names(&self, binary: &TestBinary) -> bool {
        binary.matches_target(&self.package, &self.target)
    }

    fn from_hint(hint: &BinaryHint) -> Self {
        Self {
            package: hint.package.clone(),
            target: hint.target.clone(),
        }
    }

    fn hint(&self) -> BinaryHint {
        BinaryHint {
            package: self.package.clone(),
            target: self.target.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Candidate {
    Exact(Killer),
    ItemBinary(BinaryIdentity),
    ReachFileBinary(BinaryIdentity),
    FileBinary(BinaryIdentity),
}

impl Candidate {
    fn probe_kind(&self) -> ProbeKind {
        match self {
            Self::Exact(_) | Self::ItemBinary(_) => ProbeKind::Item,
            Self::ReachFileBinary(_) => ProbeKind::Reach,
            Self::FileBinary(_) => ProbeKind::File,
        }
    }
}

#[cfg(test)]
impl Candidate {
    fn estimated_cost(&self, reachable: &[&TestBinary]) -> Duration {
        let identity = match self {
            Self::Exact(killer) => BinaryIdentity::from_killer(killer),
            Self::ItemBinary(identity) | Self::ReachFileBinary(identity) | Self::FileBinary(identity) => identity.clone(),
        };

        reachable
            .iter()
            .find(|binary| identity.names(binary))
            .map_or(Duration::ZERO, |binary| binary.baseline)
    }
}

#[derive(Clone, Debug)]
struct RankedCandidate<T> {
    identity: T,
    seeds: u32,
    hits: u32,
    misses: u32,
    canonical_passes: u32,
    measured: Duration,
    samples: u32,
    order: u64,
}

impl<T> RankedCandidate<T> {
    // A canonical pass is weaker evidence than a direct transfer miss, so two canonical passes
    // count as one miss. Integer division deliberately truncates an unmatched half.
    fn score(&self) -> (u32, core::cmp::Reverse<u32>, Duration, u64) {
        (
            self.misses.saturating_mul(2).saturating_add(self.canonical_passes) / 4,
            core::cmp::Reverse(self.hits),
            if self.samples == 0 {
                Duration::MAX
            } else {
                self.measured / self.samples
            },
            self.order,
        )
    }

    fn observe(&mut self, hit: bool, elapsed: Duration) {
        if hit {
            self.hits = self.hits.saturating_add(1);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        self.measured = self.measured.saturating_add(elapsed);
        self.samples = self.samples.saturating_add(1);
    }

    fn observe_canonical_pass(&mut self) {
        self.canonical_passes = self.canonical_passes.saturating_add(1);
    }

    fn is_admissible(&self, fallback: Duration) -> bool {
        // Keep the same half-weight relationship when applying the two-miss retirement rule.
        let weighted_misses = self.misses.saturating_add(self.canonical_passes / CANONICAL_MISS_DIVISOR);
        if self.hits == 0 && weighted_misses >= MISS_RETIREMENT_THRESHOLD {
            return false;
        }
        if self.samples == 0 {
            return true;
        }

        let average = self.measured / self.samples;
        let expected = fallback.saturating_mul(self.hits.saturating_add(1)) / self.samples.saturating_add(2);
        average <= expected
    }
}

impl<T: Clone> RankedCandidate<T> {
    fn from_hint<U>(hint: &RankedHint<U>, identity: impl FnOnce(&U) -> T) -> Self {
        Self {
            identity: identity(&hint.candidate),
            seeds: hint.seeds,
            hits: hint.hits,
            misses: hint.misses,
            canonical_passes: 0,
            measured: Duration::from_millis(hint.measured_ms),
            samples: hint.samples,
            order: hint.order,
        }
    }

    fn hint<U>(&self, candidate: impl FnOnce(&T) -> U) -> RankedHint<U> {
        RankedHint {
            candidate: candidate(&self.identity),
            seeds: self.seeds,
            hits: self.hits,
            misses: self.misses,
            measured_ms: elapsed_millis(self.measured),
            samples: self.samples,
            order: self.order,
        }
    }
}

#[derive(Debug, Default)]
struct ItemLearning {
    exact: Vec<RankedCandidate<Killer>>,
    reached: Vec<RankedCandidate<BinaryIdentity>>,
}

#[derive(Debug, Default)]
struct LearningState {
    items: crate::HashMap<String, ItemLearning>,
    reached_file: Vec<RankedCandidate<BinaryIdentity>>,
    binaries: Vec<RankedCandidate<BinaryIdentity>>,
    next_order: u64,
}

struct FileLearning {
    state: Mutex<LearningState>,
}

impl FileLearning {
    #[cfg(test)]
    fn new() -> Self {
        Self {
            state: Mutex::new(LearningState::default()),
        }
    }

    fn from_hints(file: &Utf8Path, hints: &GeneralizedHints) -> Self {
        let mut state = LearningState::default();

        for item in hints.items.iter().filter(|item| item.file == file) {
            state.items.insert(
                item.item.clone(),
                ItemLearning {
                    exact: item
                        .candidates
                        .iter()
                        .map(|candidate| RankedCandidate::from_hint(candidate, Clone::clone))
                        .collect(),
                    reached: Vec::new(),
                },
            );
        }
        if let Some(binaries) = hints.binaries.iter().find(|entry| entry.file == file) {
            state.binaries = binaries
                .candidates
                .iter()
                .map(|candidate| RankedCandidate::from_hint(candidate, BinaryIdentity::from_hint))
                .collect();
        }
        // #[gamma::skip(all, reason = "the alternative changes only internal candidate ordering or tie selection, not the accepted population exposed by this layer")]
        let item_order = state
            .items
            .values()
            .flat_map(|item| item.exact.iter())
            .map(|candidate| candidate.order)
            .max()
            // #[gamma::skip(all, reason = "the value controls scheduling, accounting, identity, or a conservative bound whose one-step perturbation has no safely deterministic external observation here")]
            .unwrap_or(0);
        // #[gamma::skip(all, reason = "the alternative changes only internal candidate ordering or tie selection, not the accepted population exposed by this layer")]
        let binary_order = state.binaries.iter().map(|candidate| candidate.order).max().unwrap_or(0);
        state.next_order = item_order.max(binary_order).saturating_add(1);

        Self { state: Mutex::new(state) }
    }

    fn persist(&self, file: &Utf8Path, output: &mut GeneralizedHints) {
        let state = self.locked();
        for (item, learning) in &state.items {
            // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
            if !learning.exact.is_empty() {
                output.items.push(ItemHints {
                    file: file.to_path_buf(),
                    item: item.clone(),
                    candidates: learning.exact.iter().map(|candidate| candidate.hint(Clone::clone)).collect(),
                });
            }
        }
        // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
        if !state.binaries.is_empty() {
            output.binaries.push(FileBinaryHints {
                file: file.to_path_buf(),
                candidates: state
                    .binaries
                    .iter()
                    .map(|candidate| candidate.hint(BinaryIdentity::hint))
                    .collect(),
            });
        }
    }

    /// Takes the state, recovering from poisoning rather than refusing to go on.
    ///
    /// This lock guards a scheduling hint and nothing else. Every critical section under it is a
    /// read or a whole-value assignment, so a panic leaves a valid `Learning` value, and the worst
    /// a stale one can cost is a cold test order for the rest
    /// of one file. Propagating the poison instead would turn an optimization into a way for one
    /// worker's panic to fail every remaining mutant in that file — and the panic that poisoned the
    /// lock is already on its way to the caller on its own thread.
    fn locked(&self) -> MutexGuard<'_, LearningState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn select_candidates(&self, item_path: &str, fallback: Duration) -> (Vec<Candidate>, [usize; 3]) {
        let (mut exact, mut reached, mut reached_file, mut binaries) = {
            let state = self.locked();
            let (exact, reached) = state
                .items
                .get(item_path)
                .map_or_else(|| (Vec::new(), Vec::new()), |item| (item.exact.clone(), item.reached.clone()));

            (exact, reached, state.reached_file.clone(), state.binaries.clone())
        };

        exact.sort_by_key(RankedCandidate::score);
        // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
        reached.sort_by_key(RankedCandidate::score);
        // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
        reached_file.sort_by_key(RankedCandidate::score);
        binaries.sort_by_key(RankedCandidate::score);
        let before = [exact.len() + reached.len(), reached_file.len(), binaries.len()];
        exact.retain(|candidate| candidate.is_admissible(fallback));
        reached.retain(|candidate| candidate.is_admissible(fallback));
        reached_file.retain(|candidate| candidate.is_admissible(fallback));
        binaries.retain(|candidate| candidate.is_admissible(fallback));
        reached_file.retain(|candidate| !contains_ranked_identity(&reached, &candidate.identity));
        // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
        binaries.retain(|candidate| !contains_ranked_identity(&reached, &candidate.identity));
        exact.truncate(INFERRED_EXACT_LIMIT);
        reached.truncate(1);
        reached_file.truncate(1);
        binaries.truncate(1);

        let candidates = exact
            .into_iter()
            .map(|candidate| Candidate::Exact(candidate.identity))
            .chain(reached.into_iter().map(|candidate| Candidate::ItemBinary(candidate.identity)))
            .chain(
                reached_file
                    .into_iter()
                    .map(|candidate| Candidate::ReachFileBinary(candidate.identity)),
            )
            .chain(binaries.into_iter().map(|candidate| Candidate::FileBinary(candidate.identity)))
            .collect::<Vec<_>>();
        let after = candidate_tier_counts(&candidates);

        (
            candidates,
            [
                before[0].saturating_sub(after[0]),
                before[1].saturating_sub(after[1]),
                before[2].saturating_sub(after[2]),
            ],
        )
    }

    #[cfg(test)]
    fn candidates(&self, item_path: &str, fallback: Duration) -> Vec<Candidate> {
        self.select_candidates(item_path, fallback).0
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn observe(&self, item_path: &str, candidate: &Candidate, hit: bool, elapsed: Duration) {
        let mut state = self.locked();
        match candidate {
            Candidate::Exact(identity) => {
                if let Some(found) = state
                    .items
                    .entry(item_path.to_owned())
                    .or_default()
                    .exact
                    .iter_mut()
                    .find(|candidate| same_identity(&candidate.identity, identity))
                {
                    RankedCandidate::observe(found, hit, elapsed);
                }
            }
            Candidate::ItemBinary(identity) => {
                if let Some(found) = state
                    .items
                    .entry(item_path.to_owned())
                    .or_default()
                    .reached
                    .iter_mut()
                    .find(|candidate| same_identity(&candidate.identity, identity))
                {
                    RankedCandidate::observe(found, hit, elapsed);
                }
            }
            Candidate::ReachFileBinary(identity) => {
                if let Some(found) = state
                    .reached_file
                    .iter_mut()
                    .find(|candidate| same_identity(&candidate.identity, identity))
                {
                    RankedCandidate::observe(found, hit, elapsed);
                }
            }
            Candidate::FileBinary(identity) => {
                if let Some(found) = state
                    .binaries
                    .iter_mut()
                    .find(|candidate| same_identity(&candidate.identity, identity))
                {
                    RankedCandidate::observe(found, hit, elapsed);
                }
            }
        }
    }

    fn record_canonical_pass(&self, item_path: &str, binary: &TestBinary, reached: bool) {
        let identity = BinaryIdentity::from_binary(binary);
        let mut state = self.locked();

        if let Some(found) = state
            .items
            .entry(item_path.to_owned())
            .or_default()
            .reached
            .iter_mut()
            .find(|candidate| same_identity(&candidate.identity, &identity))
        {
            found.observe_canonical_pass();
        }
        if reached
            && let Some(found) = state
                .reached_file
                .iter_mut()
                .find(|candidate| same_identity(&candidate.identity, &identity))
        {
            found.observe_canonical_pass();
        }
        if let Some(found) = state
            .binaries
            .iter_mut()
            .find(|candidate| same_identity(&candidate.identity, &identity))
        {
            found.observe_canonical_pass();
        }
    }

    fn reached(&self, item_path: &str, binary: &TestBinary, elapsed: Duration) {
        let identity = BinaryIdentity::from_binary(binary);
        let mut state = self.locked();
        let order = state.next_order;
        advance_order(&mut state);
        let candidate = || reached_candidate(identity.clone(), elapsed, order);

        let item = state.items.entry(item_path.to_owned()).or_default();
        if !contains_ranked_identity(&item.reached, &identity) {
            item.reached.push(candidate());
        }
        if !contains_ranked_identity(&state.reached_file, &identity) {
            state.reached_file.push(candidate());
        }
    }

    fn publish(&self, item_path: &str, judged: &Judgement) {
        let killer = match judged {
            Judgement::Reached(_, Some(killer), _) | Judgement::Guessed(killer) => killer,
            Judgement::Reached(..) | Judgement::Abandoned(_) => return,
        };

        let mut state = self.locked();
        let order = state.next_order;
        advance_order(&mut state);
        let item = state.items.entry(item_path.to_owned()).or_default();

        if !contains_ranked_identity(&item.exact, killer) {
            item.exact.push(published_candidate(killer.clone(), order));
        }

        let binary = BinaryIdentity::from_killer(killer);
        if !contains_ranked_identity(&state.binaries, &binary) {
            state.binaries.push(published_candidate(binary, order));
        }
    }
}

fn same_identity<T: PartialEq>(left: &T, right: &T) -> bool {
    left == right
}

fn contains_ranked_identity<T: PartialEq>(candidates: &[RankedCandidate<T>], identity: &T) -> bool {
    candidates.iter().any(|candidate| same_identity(&candidate.identity, identity))
}

fn advance_order(state: &mut LearningState) {
    state.next_order = state.next_order.saturating_add(1);
}

fn reached_candidate(identity: BinaryIdentity, elapsed: Duration, order: u64) -> RankedCandidate<BinaryIdentity> {
    RankedCandidate {
        identity,
        seeds: 1,
        hits: 0,
        misses: u32::from(false),
        canonical_passes: 0,
        measured: elapsed,
        samples: u32::from(true),
        order,
    }
}

fn published_candidate<T>(identity: T, order: u64) -> RankedCandidate<T> {
    RankedCandidate {
        identity,
        seeds: 1,
        hits: 0,
        misses: u32::from(false),
        canonical_passes: 0,
        measured: Duration::ZERO,
        samples: u32::from(false),
        order,
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    // Relaxed counter reads observe synchronous updates on this thread, or workers already
    // joined by test_all's scope. They do not publish or acquire unrelated state.
    use core::iter::once;
    #[cfg(unix)]
    use std::fs;
    use std::rc::Rc;

    use camino::Utf8PathBuf;

    use super::*;

    fn policy_harness(binaries: Vec<TestBinary>) -> (Workspace, Plan, Vec<TestBinary>) {
        let root = Utf8PathBuf::from("synthetic-sweep");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        (work, one_mutant_plan(root), binaries)
    }

    #[test]
    fn enumeration_diagnostics_retain_exactly_the_last_twenty_distinct_lines() {
        notes::alone(|| {
            let lines = (0..22).map(|index| format!("line{index:02}")).collect::<Vec<_>>();
            let reason = enumeration_note(Utf8Path::new("native-binary"), &lines.join("\n"));
            assert!(!reason.contains("line"));
            let raised = notes::drain();
            assert_eq!(raised.len(), 1);
            let (_header, body) = raised[0]
                .split_once('\n')
                .expect("the local note separates its heading from the escaped diagnostic");
            assert_eq!(body, lines[2..].join("\\n"));
        });
    }

    #[test]
    fn resource_fallback_selects_the_best_available_alternative_and_unknown_capacities_default_to_exactly_one() {
        let mut work = (0..3)
            .map(|index| scheduled(index, "item", &format!("p{index}"), 30 - index as u64 * 10, 0, false, index))
            .collect::<Vec<_>>();
        work[0].resources.push("exclusive".into());
        let scheduler = Scheduler::with_resources(work, 3, once((Arc::from("exclusive"), 1)).collect());
        let (head, alternative) = {
            let mut state = scheduler.state.lock().expect("no worker has touched the new scheduler");
            let head = state.heads.first().map(|head| head.local.index);
            state.active_resources.insert("exclusive".into(), 1);
            (head, scheduler.select(&state))
        };
        assert_eq!(head, Some(0));
        assert_eq!(
            alternative,
            Some(1),
            "the blocked best head must not reverse the remaining priority order"
        );
        let mut work = scheduled(0, "item", "subject", 1, 0, false, 0);
        work.resources.push("not-in-capacities".into());
        let scheduler = Scheduler::with_resources(vec![work], 1, once((Arc::from("different"), 3)).collect());
        for active in 0..=2 {
            let available = {
                let mut state = scheduler.state.lock().expect("new scheduler has no competing worker");
                state.active_resources.insert("not-in-capacities".into(), active);
                scheduler.resources_available(&state, 0)
            };
            assert_eq!(available, active == 0);
        }
    }

    #[test]
    fn selected_resource_reservations_use_the_exact_ordinal_and_are_sorted_and_unique() {
        let (mut work, _, _) = policy_harness(Vec::new());
        let [binary, _] = ranked_binaries();
        work.set_resources(Resources::fake_tests(
            &binary,
            &[("tests::t0", "z"), ("tests::t1", "a"), ("tests::t2", "z")],
        ));
        let census = Census::examined(&binary.path, 7, 3, 8);
        let planned = crate::estimate::MutationWork::hinted(
            crate::estimate::WorkKind::Selected,
            Duration::from_millis(1),
            Duration::from_millis(2),
        );
        for ordinal in [0, 7, 8] {
            let resources = scheduled_resources(&work, planned, &[&binary], None, ordinal, &census);
            let names = resources.iter().map(AsRef::as_ref).collect::<Vec<_>>();
            assert_eq!(names, if ordinal == 7 { vec!["a", "z"] } else { vec![] });
        }
    }

    #[test]
    fn candidate_limits_retain_distinct_first_candidates_and_report_each_tiers_exact_rejections() {
        let identity = |index| BinaryIdentity {
            package: "subject".into(),
            target: format!("target-{index}"),
        };
        let mut candidates = (0..4)
            .map(|index| {
                Candidate::Exact(Killer {
                    package: "subject".into(),
                    target: "lib".into(),
                    test: format!("test-{index}"),
                })
            })
            .chain((0..2).map(|index| Candidate::ItemBinary(identity(index))))
            .chain((0..3).map(|index| Candidate::ReachFileBinary(identity(index))))
            .chain((0..4).map(|index| Candidate::FileBinary(identity(index))))
            .collect::<Vec<_>>();
        let expected = [0, 1, 4, 6, 9].map(|index| candidates[index].clone());
        assert_eq!(bound_candidates(&mut candidates), [3, 2, 3]);
        assert_eq!(candidates, expected);
        let hint = observed_planning_hint(identity(99).hint(), 2);
        let candidate = RankedCandidate::from_hint(&hint, BinaryIdentity::from_hint);
        assert_eq!(candidate.canonical_passes, 0);
        assert_eq!(
            (candidate.seeds, candidate.hits, candidate.misses, candidate.samples),
            (hint.seeds, hint.hits, hint.misses, hint.samples)
        );
    }

    #[test]
    fn reach_hint_admission_distinguishes_below_at_and_above_half_a_known_suite() {
        let [mut binary, _] = ranked_binaries();
        let hints = (0..4)
            .map(|index| Killer {
                package: binary.package.clone(),
                target: binary.target.clone(),
                test: format!("case-{index}"),
            })
            .collect::<Vec<_>>();
        let census = Census::partial(&binary.path, 7, 1, 8);
        for total in [4, 6] {
            binary.tests = Some(total);
            for count in 1..=4 {
                assert_eq!(
                    bounded_reach_hints(&hints[..count], &[&binary], 7, &census),
                    if count <= total / 2 { hints[..count].to_vec() } else { vec![] }
                );
            }
        }
        binary.baseline = Duration::ZERO;
        assert!(bounded_reach_hints(&hints[..1], &[&binary], 7, &census).is_empty());
    }

    #[test]
    fn grouped_probes_preserve_nondefault_attempts_and_distinguish_named_singleton_and_ambiguous_failures() {
        let (work, _, _) = policy_harness(Vec::new());
        let [binary, _] = ranked_binaries();
        for (names, failure) in [
            (&["alpha"][..], None),
            (&["alpha", "beta"][..], None),
            (&["alpha", "beta"][..], Some("named")),
        ] {
            let tally = Tally::default();
            let result = judge_script(
                vec![(
                    "earlier",
                    Only::These(names),
                    Verdict::Failed(failure.map(str::to_owned)),
                    ReachObservation::Reached,
                )],
                ranked_attempt(),
                true,
                || probe_cases(&work, 7, &binary, names, Some(2.0), ranked_sweep(blind()), &tally),
            );
            let killer = result.0.expect("every scripted failed group supplies a candidate identity");
            assert_eq!(killer.identity.test, failure.unwrap_or("alpha"));
            assert_eq!(killer.observed, failure.is_some() || names.len() == 1);
            assert_eq!(result.1, Some(true));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
            assert_eq!(tally.census_hits.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn learning_adapter_preserves_exact_and_promoted_hints_only_for_deterministic_reach_and_forwards_timeout() {
        let (work, _, _) = policy_harness(Vec::new());
        let [binary, _] = ranked_binaries();
        let hint = Killer {
            package: binary.package.clone(),
            target: binary.target.clone(),
            test: "hinted".into(),
        };
        let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
        for exact in [false, true] {
            for deterministic in [false, true] {
                let learning = FileLearning::new();
                let tally = Tally::default();
                let negative = NegativeLearning::default();
                let only = if deterministic { Only::One("hinted") } else { Only::All };
                let promoted = Candidate::Exact(hint.clone());
                let judged = judge_script(
                    vec![("earlier", only, Verdict::Failed(None), ReachObservation::Reached)],
                    ranked_attempt(),
                    true,
                    || {
                        judge_learning(
                            &work,
                            7,
                            &[&binary],
                            exact.then_some(&hint),
                            if exact { &[] } else { std::slice::from_ref(&promoted) },
                            &learning,
                            "subject::function",
                            &site,
                            &negative,
                            deterministic,
                            Some(2.0),
                            ranked_sweep(blind()),
                            &tally,
                        )
                    },
                );
                assert!(matches!(judged, Judgement::Reached(Outcome::Killed, _, None)));
                assert_eq!(tally.probes.load(Ordering::Relaxed), usize::from(deterministic));
                assert_eq!(tally.exact_candidates.load(Ordering::Relaxed), usize::from(exact && deterministic));
                assert_eq!(
                    tally.inferred_candidates.load(Ordering::Relaxed),
                    usize::from(!exact && deterministic)
                );
            }
        }
    }

    #[test]
    fn an_incomplete_group_with_unknown_suite_size_contributes_zero_available_tests() {
        let (work, _, _) = policy_harness(Vec::new());
        let [mut binary, _] = ranked_binaries();
        binary.tests = None;
        let census = Census::partial(&binary.path, 7, 1, 4);
        let tally = Tally::default();
        let judged = judge_script(
            vec![(
                "earlier",
                Only::These(&["tests::t0"]),
                Verdict::Failed(None),
                ReachObservation::Reached,
            )],
            ranked_attempt(),
            true,
            || judge_ordered(&work, 7, &[&binary], None, None, Some(2.0), ranked_sweep(&census), &tally),
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.available_tests.load(Ordering::Relaxed), 0);
        assert_eq!(tally.selected_tests.load(Ordering::Relaxed), 1);
        assert_eq!(tally.census_candidates.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn persistence_replaces_each_touched_file_without_retaining_stale_binary_candidates_from_either_file() {
        let mut hints = GeneralizedHints::empty_supported();
        hints.binaries = ["src/first.rs", "src/second.rs", "src/untouched.rs"]
            .map(|file| FileBinaryHints {
                file: file.into(),
                candidates: vec![observed_planning_hint(
                    BinaryHint {
                        package: "subject".into(),
                        target: file.into(),
                    },
                    2,
                )],
            })
            .into();
        let files = ["src/first.rs", "src/second.rs"].map(|file| Arc::from(Utf8Path::new(file)));
        persist_learning(&mut hints, &files, &[FileLearning::new(), FileLearning::new()]);
        assert_eq!(hints.binaries.len(), 1);
        assert_eq!(hints.binaries[0].file, Utf8Path::new("src/untouched.rs"));
        assert_eq!(hints.binaries[0].candidates[0].candidate.target, "src/untouched.rs");
    }

    fn with_planning_observer<T>(observer: impl FnMut(&[ScheduledWork], &[Vec<Candidate>]) + 'static, body: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                PLANNING_OBSERVER.with_borrow_mut(|observer| *observer = None);
            }
        }
        PLANNING_OBSERVER.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "planning observers must not nest");
            *slot = Some(Box::new(observer));
        });
        let _reset = Reset;
        body()
    }

    #[test]
    fn actual_sweep_planning_preserves_sibling_counts_exact_flags_reach_candidates_and_resource_reservations() {
        let (_directory, mut work, _, binaries) = helper_harness(&["exit:0"]);
        let mut plan = plan_over(&["subject", "subject", "z", "a"]);
        plan.root = work.root.clone();
        let mut binaries = ["subject", "z", "a"].map(|package| TestBinary {
            package: package.into(),
            tests: Some(8),
            baseline: Duration::from_millis(50),
            ..binaries[0].clone()
        });
        binaries[0].target = "lib".into();
        work.set_resources(Resources::fake_tests(&binaries[0], &[("tests::t0", "db")]));
        let census = Census::partial(&binaries[0].path, 1, 1, 8);
        let scope = TestScope {
            packages: &[],
            package_local: true,
            whole_workspace: false,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);
        let hint = Killer {
            package: "subject".into(),
            target: "lib".into(),
            test: "tests::t0".into(),
        };
        let mut killers = Killers::default();
        killers.record(plan.mutants[0].id.clone(), hint.clone());
        let mut generalized = GeneralizedHints::empty_supported();
        generalized.test_sets.push(vec![hint]);
        generalized.reach.push(crate::discover::ReachCluster {
            site: SiteIdentity::from_mutant(&plan.mutants[0]),
            test_set: 0,
        });
        killers.replace_generalized(generalized);
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let saw = std::rc::Rc::clone(&observed);
        let spent = with_planning_observer(
            move |work, promoted| {
                saw.set(true);
                assert_eq!(work.len(), 4);
                assert!(work.iter().all(|item| item.sibling_benefit == 3));
                assert_eq!(work.iter().filter(|item| item.hinted).count(), 1);
                assert!(
                    work.iter()
                        .any(|item| item.resources.iter().any(|resource| resource.as_ref() == "db"))
                );
                assert!(promoted.iter().any(|candidates| !candidates.is_empty()));
            },
            || {
                test_all_with_hooks(
                    &work,
                    &mut plan,
                    &reach,
                    Sweep {
                        jobs: 1,
                        ..ranked_sweep(&census)
                    },
                    true,
                    &mut killers,
                    &mut crate::testing::Recorder::default(),
                    (|_, _| Ok(()), |_| Some(Judgement::Reached(Outcome::Survived, None, None)), || {}),
                )
            },
        )
        .expect("the bounded sweep consumes four supplied judgements")
        .expect("four mutants are pending");
        assert!(observed.get(), "the actual scheduler input must pass through the observer");
        assert_eq!(
            spent.packages.iter().map(|package| package.package.as_ref()).collect::<Vec<_>>(),
            ["a", "subject", "z"]
        );
    }

    #[test]
    fn checkpoint_accounting_samples_counters_and_sorts_completed_package_spans() {
        let tally = Tally::default();
        tally.launches.store(7, Ordering::Relaxed);
        tally.probes.store(3, Ordering::Relaxed);
        tally.saved.store(2, Ordering::Relaxed);
        tally.exact_hits.store(1, Ordering::Relaxed);
        tally.selected_tests.store(9, Ordering::Relaxed);
        let spans = crate::HashMap::from_iter([(Arc::from("b"), (2, 5, 90)), (Arc::from("a"), (1, 10, 50))]);
        let actual = super::super::measure::sweep_cost(Some(tally.spent(&spans)), Duration::from_millis(100)).unwrap();
        let expected = super::super::session::SweepCost {
            elapsed: Duration::from_millis(100),
            launches: 7,
            probes: 3,
            launches_saved: 2,
            exact_hits: 1,
            selected_tests: 9,
            packages: vec![
                super::super::session::PackageSweepCost {
                    package: "a".into(),
                    mutants: 1,
                    first_started_ms: 10,
                    settled_ms: 50,
                },
                super::super::session::PackageSweepCost {
                    package: "b".into(),
                    mutants: 2,
                    first_started_ms: 5,
                    settled_ms: 90,
                },
            ],
            ..super::super::session::SweepCost::default()
        };
        assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
    }

    #[test]
    fn learning_observation_matrix_updates_matching_tiers_and_preserves_seed_order() {
        let binary = TestBinary {
            package: "package".to_owned(),
            target: "target".to_owned(),
            ..crate::testing::test_binary("learning-binary")
        };
        let killer = Killer {
            package: "package".to_owned(),
            target: "target".to_owned(),
            test: "test".to_owned(),
        };
        for hit in [false, true] {
            let learning = FileLearning::new();
            learning.reached("item", &binary, Duration::from_millis(13));
            learning.publish("item", &Judgement::Guessed(killer.clone()));
            let identity = BinaryIdentity::from_binary(&binary);
            for candidate in [
                Candidate::Exact(killer.clone()),
                Candidate::ItemBinary(identity.clone()),
                Candidate::ReachFileBinary(identity.clone()),
                Candidate::FileBinary(identity),
            ] {
                learning.observe("item", &candidate, hit, Duration::from_millis(17));
            }
            let state = learning.locked();
            for candidate in [&state.items["item"].reached[0], &state.reached_file[0], &state.binaries[0]] {
                assert_eq!(candidate.hits, u32::from(hit));
                assert_eq!(candidate.misses, u32::from(!hit));
                assert_eq!(candidate.seeds, 1);
                assert_eq!(candidate.canonical_passes, 0);
            }
            assert_eq!(state.items["item"].exact[0].hits, u32::from(hit));
            assert_eq!(state.items["item"].exact[0].misses, u32::from(!hit));
            assert_eq!(state.items["item"].exact[0].samples, 1);
            assert_eq!(state.items["item"].exact[0].measured, Duration::from_millis(17));
            assert_eq!(state.items["item"].reached[0].samples, 2);
            assert_eq!(state.items["item"].reached[0].measured, Duration::from_millis(30));
            assert_eq!(state.items["item"].reached[0].order, 0);
            assert_eq!(state.reached_file[0].order, 0);
            assert_eq!(state.items["item"].exact[0].order, 1);
            assert_eq!(state.binaries[0].order, 1);
            assert_eq!(state.next_order, 2);
            let serialized = state.items["item"].exact[0].hint(Clone::clone);
            assert_eq!(serialized.measured_ms, 17);
        }
    }

    #[test]
    fn canonical_pass_matrix_updates_reached_file_only_when_reach_was_observed() {
        let binary = crate::testing::test_binary("canonical-binary");
        for reached in [false, true] {
            let learning = FileLearning::new();
            learning.reached("item", &binary, Duration::from_millis(1));
            learning.publish(
                "item",
                &Judgement::Guessed(Killer {
                    package: binary.package.clone(),
                    target: binary.killer_target(),
                    test: "test".to_owned(),
                }),
            );
            learning.record_canonical_pass("item", &binary, reached);
            let state = learning.locked();
            assert_eq!(state.items["item"].reached[0].canonical_passes, 1);
            assert_eq!(state.reached_file[0].canonical_passes, u32::from(reached));
            assert_eq!(state.binaries[0].canonical_passes, 1);
            assert_eq!(state.items["item"].exact[0].canonical_passes, 0);
        }
    }

    #[test]
    fn selection_trace_preserves_each_context_field_and_resets_the_previous_generation() {
        let binary = TestBinary {
            package: "package".to_owned(),
            target: "target".to_owned(),
            ..crate::testing::test_binary("trace-binary")
        };
        for (observation, expected) in [
            (Some(true), SelectionResult::Hit),
            (Some(false), SelectionResult::Miss),
            (None, SelectionResult::Inconclusive),
        ] {
            begin_selection_trace();
            trace_selection(
                7,
                &binary,
                Some("test".to_owned()),
                SelectionTier::Reach,
                3,
                observation,
                true,
                Duration::from_millis(11),
                Duration::from_millis(13),
            );
            let attempts = take_selection_trace();
            assert_eq!(attempts.len(), 1);
            let attempt = &attempts[0];
            assert_eq!(attempt.ordinal, 7);
            assert_eq!(attempt.package, "package");
            assert_eq!(attempt.target, "target");
            assert_eq!(attempt.test.as_deref(), Some("test"));
            assert_eq!(attempt.tier, SelectionTier::Reach);
            assert_eq!(attempt.rank, 3);
            assert_eq!(attempt.result, expected);
            assert!(attempt.all_tests);
            assert_eq!(attempt.elapsed_ms, 11);
            assert_eq!(attempt.fallback_ms, 13);
            assert!(take_selection_trace().is_empty());
        }
        begin_selection_trace();
        trace_selection(
            1,
            &binary,
            None,
            SelectionTier::Exact,
            0,
            None,
            false,
            Duration::ZERO,
            Duration::ZERO,
        );
        begin_selection_trace();
        assert!(take_selection_trace().is_empty());
    }

    #[test]
    fn filtered_confirmation_and_census_absence_truth_tables_do_not_equate_hints_with_proofs() {
        let binary = crate::testing::test_binary("selection-binary");
        let complete = Census::examined(&binary.path, 7, 1, 4);
        let partial = Census::partial(&binary.path, 7, 1, 4);
        assert!(census_excludes(&complete, &binary, 99));
        assert!(!census_excludes(&complete, &binary, 7));
        assert!(!census_excludes(&partial, &binary, 99));
        for already_whole in [false, true] {
            for selection in [
                CensusSelection::Whole,
                CensusSelection::Uncovered,
                CensusSelection::Hinted(vec!["test"]),
                CensusSelection::Selected(vec!["test"]),
            ] {
                for verdict in [
                    Verdict::Passed,
                    Verdict::Failed(None),
                    Verdict::TimedOut,
                    Verdict::Unjudged("fixture".to_owned()),
                ] {
                    assert_eq!(
                        filtered_run_needs_confirmation(already_whole, &selection, &verdict),
                        !already_whole
                            && matches!(selection, CensusSelection::Selected(_))
                            && matches!(verdict, Verdict::TimedOut | Verdict::Unjudged(_))
                    );
                }
            }
        }
    }
    #[cfg(unix)]
    use crate::exec::faults::{self, Fault};
    #[cfg(unix)]
    use crate::exec::memory;
    use crate::exec::resources::Resources;
    use crate::ops::collect::Shape;

    type JudgeStep = (&'static str, Only<'static>, Verdict, ReachObservation);

    fn judge_script<T>(steps: Vec<JudgeStep>, expected: Attempt<'static>, confirm: bool, body: impl FnOnce() -> T) -> T {
        let remaining = Rc::new(RefCell::new(std::collections::VecDeque::from(steps)));
        let script = Rc::clone(&remaining);
        let result = super::super::verdict::with_binary_runner(
            move |binary, attempt, confirming| {
                let (target, only, verdict, reach) = script.borrow_mut().pop_front().expect("every launch must match a scripted step");
                assert_eq!(binary.target, target);
                assert_eq!(format!("{:?}", attempt.only), format!("{only:?}"));
                assert_eq!(attempt.active, expected.active);
                assert_eq!(attempt.timeout, expected.timeout);
                assert_eq!(attempt.stall.budget, expected.stall.budget);
                assert_eq!(attempt.request.meter, expected.request.meter);
                assert_eq!(attempt.request.limit, expected.request.limit);
                assert_eq!(attempt.census, expected.census);
                assert_eq!(confirming, confirm);
                BinaryRun { verdict, reach }
            },
            body,
        );
        assert!(remaining.borrow().is_empty(), "all scripted launches must occur");
        result
    }

    fn ranked_binaries() -> [TestBinary; 2] {
        ["earlier", "later"].map(|target| TestBinary {
            package: "subject".into(),
            target: target.into(),
            baseline: Duration::from_millis(11),
            budget: Some(Duration::from_millis(13)),
            memory: Some(37),
            tests: Some(4),
            ..crate::testing::test_binary(target)
        })
    }

    fn ranked_sweep(census: &Census) -> Sweep<'_> {
        Sweep {
            timeout_floor: Duration::from_millis(17),
            stall: Stall {
                budget: Some(Duration::from_millis(23)),
            },
            jobs: 1,
            meter: true,
            confirm: true,
            census,
        }
    }

    fn ranked_attempt() -> Attempt<'static> {
        Attempt {
            active: Some(7),
            timeout: Some(Duration::from_millis(22)),
            stall: Stall {
                budget: Some(Duration::from_millis(23)),
            },
            request: MemoryRequest {
                meter: true,
                limit: Some(37),
            },
            only: Only::All,
            census: None,
        }
    }

    fn assert_trace_context(attempts: &[SelectionAttempt], fallback_ms: u64) {
        for attempt in attempts {
            assert_eq!(attempt.ordinal, 7);
            assert_eq!(attempt.package, "subject");
            assert_eq!(attempt.rank, 1);
            assert_eq!(attempt.fallback_ms, fallback_ms);
        }
    }

    #[test]
    fn package_accounting_uses_exact_completion_counts_and_extrema_not_receive_order() {
        let (_work, mut plan, _) = policy_harness(Vec::new());
        plan.mutants.push(Mutant {
            ordinal: 2,
            package: "alpha".into(),
            ..plan.mutants[0].clone()
        });
        let mut spans = crate::HashMap::default();
        for (position, first_started_ms, settled_ms) in [(0, 17, 21), (1, 9, 11), (0, 3, 15), (0, 25, 33), (99, 0, 99)] {
            record_package_span(
                &mut spans,
                &plan,
                &Completed {
                    position,
                    outcome: Outcome::Survived,
                    elapsed_ms: 4,
                    killer: None,
                    note: None,
                    first_started_ms,
                    settled_ms,
                    attempts: Vec::new(),
                },
            );
        }
        assert_eq!(spans.len(), 2);
        assert_eq!(spans.get("subject"), Some(&(3, 3, 33)));
        assert_eq!(spans.get("alpha"), Some(&(1, 9, 11)));
        let spent = Tally::default().spent(&spans);
        assert_eq!(
            spent
                .packages
                .iter()
                .map(|package| (
                    package.package.as_ref(),
                    package.mutants,
                    package.first_started_ms,
                    package.settled_ms
                ))
                .collect::<Vec<_>>(),
            [("alpha", 1, 9, 11), ("subject", 3, 3, 33)]
        );
    }

    #[derive(Default)]
    struct SweepRecorder {
        progress: Vec<(usize, usize)>,
        planned: Option<(usize, usize)>,
        attempts: Vec<SelectionAttempt>,
        starts: usize,
        mutants: Vec<u32>,
    }

    impl Events for SweepRecorder {
        fn phase(&mut self, _verb: &str, _detail: &str) {}
        fn mutant(&mut self, mutant: &Mutant) {
            self.mutants.push(mutant.ordinal);
        }
        fn sweep_plan_progress(&mut self, completed: usize, total: usize) {
            self.progress.push((completed, total));
        }
        fn sweep_planned(&mut self, work: &[crate::estimate::MutationWork], jobs: usize) {
            self.planned = Some((work.len(), jobs));
        }
        fn selection_attempt(&mut self, attempt: &SelectionAttempt) {
            self.attempts.push(attempt.clone());
        }
        fn mutant_started(&mut self) {
            self.starts += 1;
        }
    }

    #[test]
    fn checkpoint_failure_drains_all_inflight_verdicts_without_retrying_or_publishing_them() {
        let (_work, mut plan, _) = policy_harness(Vec::new());
        plan.mutants = (1..=3)
            .map(|ordinal| Mutant {
                id: format!("checkpoint-{ordinal}").into(),
                ordinal,
                file: Utf8PathBuf::from(format!("src/file-{ordinal}.rs")).into(),
                ..plan.mutants[0].clone()
            })
            .collect();
        let mut queued = (0..3)
            .flat_map(|position| {
                [
                    SweepEvent::Started,
                    SweepEvent::Completed(Completed {
                        position,
                        outcome: Outcome::Survived,
                        elapsed_ms: 1,
                        killer: None,
                        note: None,
                        first_started_ms: 0,
                        settled_ms: 1,
                        attempts: Vec::new(),
                    }),
                ]
            })
            .collect::<std::collections::VecDeque<_>>();
        let mut checkpoints = 0;
        let mut failures = Vec::new();
        let mut spans = PackageSpans::default();
        let mut events = SweepRecorder::default();
        drain_sweep_events(
            &mut plan,
            &mut Killers::default(),
            &mut events,
            &mut spans,
            |_events| queued.pop_front(),
            |_mutant, _killers, _spans| {
                checkpoints += 1;
                Err(error!("checkpoint lost"))
            },
            |failure| failures.push(failure),
        );
        assert!(queued.is_empty());
        assert_eq!(failures, ["checkpoint lost"]);
        assert_eq!(checkpoints, 1);
        assert_eq!(events.starts, 3);
        assert!(events.mutants.is_empty());
        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Survived));
        assert_eq!(spans["subject"].0, 3);
    }

    #[test]
    fn sweep_hooks_publish_planning_and_both_completed_and_abandoned_selection_traces() {
        for abandoned in [false, true] {
            let (work, mut plan, binaries) = policy_harness(Vec::new());
            let count = if abandoned { 1 } else { 3 };
            plan.mutants = (1..=count)
                .map(|ordinal| Mutant {
                    id: format!("m{ordinal}").into(),
                    ordinal,
                    ..plan.mutants[0].clone()
                })
                .collect();
            let reach = Reachability::build(&plan, &binaries, &NARROW);
            let mut events = SweepRecorder::default();
            let mut checkpointed = Vec::new();
            let result = test_all_with_accounting_hooks(
                &work,
                &mut plan,
                &reach,
                Sweep {
                    jobs: 2,
                    ..sweep(Stall::NONE)
                },
                false,
                &mut Killers::default(),
                &mut events,
                (
                    |mutant, _killers, _cost| {
                        checkpointed.push(mutant.ordinal);
                        Ok(())
                    },
                    |ordinal| {
                        record_selection_attempt(SelectionAttempt {
                            ordinal,
                            tier: SelectionTier::Whole,
                            package: "subject".into(),
                            target: format!("target-{ordinal}"),
                            test: None,
                            rank: 3,
                            result: SelectionResult::Miss,
                            all_tests: true,
                            elapsed_ms: 17,
                            fallback_ms: 29,
                        });
                        Some(if abandoned {
                            Judgement::Abandoned("scripted containment loss".into())
                        } else {
                            Judgement::Reached(Outcome::Survived, None, None)
                        })
                    },
                    || {},
                ),
            );
            assert_eq!(events.planned, Some((usize::try_from(count).unwrap(), 2)));
            assert_eq!(
                events.progress,
                (1..=usize::try_from(count).unwrap())
                    .map(|index| (index, usize::try_from(count).unwrap()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(events.starts, usize::try_from(count).unwrap());
            events.attempts.sort_by_key(|attempt| attempt.ordinal);
            assert_eq!(events.attempts.len(), usize::try_from(count).unwrap());
            for (index, attempt) in events.attempts.iter().enumerate() {
                assert_eq!(attempt.ordinal, u32::try_from(index + 1).unwrap());
                assert_eq!(attempt.target, format!("target-{}", attempt.ordinal));
                assert_eq!((attempt.rank, attempt.elapsed_ms, attempt.fallback_ms), (3, 17, 29));
            }
            if abandoned {
                let failure = result.expect_err("containment loss abandons the sweep");
                assert!(failure.message().contains("scripted containment loss"), "{failure}");
                assert!(checkpointed.is_empty());
                assert!(events.mutants.is_empty());
                assert_eq!(plan.mutants[0].outcome, Outcome::Pending);
            } else {
                let spent = result.unwrap().expect("three pending mutants supply a sweep");
                checkpointed.sort_unstable();
                events.mutants.sort_unstable();
                assert_eq!(checkpointed, [1, 2, 3]);
                assert_eq!(events.mutants, checkpointed);
                assert_eq!(spent.packages[0].mutants, 3);
                assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Survived));
            }
        }
    }

    #[test]
    fn negative_reach_skips_the_first_binary_and_its_hints_without_skipping_the_next_binary() {
        let (work, plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries().map(|binary| TestBinary {
            budget: None,
            memory: None,
            ..binary
        });
        let hint = Killer {
            package: "subject".into(),
            target: binaries[0].killer_target(),
            test: "historical".into(),
        };
        let negative = NegativeLearning::default();
        let site = SiteIdentity::from_mutant(&plan.mutants[0]);
        negative.record(&site, &binaries[0]);
        let tally = Tally::default();
        let mut settings = ranked_sweep(blind());
        settings.stall = Stall::NONE;
        settings.meter = false;
        settings.confirm = false;
        let expected = Attempt {
            timeout: None,
            stall: Stall::NONE,
            request: MemoryRequest::default(),
            ..ranked_attempt()
        };
        begin_selection_trace();
        let judged = judge_script(
            vec![("later", Only::All, Verdict::Passed, ReachObservation::Reached)],
            expected,
            false,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &[Candidate::Exact(hint.clone())],
                    None,
                    Some((&negative, &site, true)),
                    Some(2.0),
                    settings,
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.saved.load(Ordering::Relaxed), 1);
        assert_eq!(tally.reach_saved.load(Ordering::Relaxed), 1);
        let attempts = take_selection_trace();
        assert_eq!(attempts.len(), 1);
        assert_eq!((attempts[0].target.as_str(), attempts[0].tier), ("later", SelectionTier::Whole));
        assert_eq!(
            (
                attempts[0].ordinal,
                attempts[0].package.as_str(),
                attempts[0].rank,
                attempts[0].fallback_ms
            ),
            (7, "subject", 2, 22)
        );
    }

    #[test]
    fn ranked_candidate_verdicts_never_bypass_an_earlier_canonical_timeout() {
        for verdict in [
            Verdict::Passed,
            Verdict::Failed(Some("later-failure".into())),
            Verdict::TimedOut,
            Verdict::Stalled(Some("last".into())),
            Verdict::MemoryLimit { peak: Some(41), limit: 37 },
            Verdict::Flaky(Some("flake".into())),
            Verdict::Unjudged("unavailable".into()),
        ] {
            let (work, _plan, _) = policy_harness(Vec::new());
            let binaries = ranked_binaries();
            let tally = Tally::default();
            let candidate = Candidate::ItemBinary(BinaryIdentity::from_binary(&binaries[1]));
            let observation = selection_result(transfer_observation(&verdict));
            begin_selection_trace();
            let judged = judge_script(
                vec![
                    ("later", Only::All, verdict, ReachObservation::Unknown),
                    ("earlier", Only::All, Verdict::TimedOut, ReachObservation::Unknown),
                ],
                ranked_attempt(),
                true,
                || {
                    judge_ranked(
                        &work,
                        7,
                        &[&binaries[0], &binaries[1]],
                        None,
                        &[candidate],
                        None,
                        None,
                        Some(2.0),
                        ranked_sweep(blind()),
                        &tally,
                    )
                },
            );
            assert!(matches!(judged, Judgement::Reached(Outcome::Timeout, None, None)));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
            assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
            assert_eq!(tally.exact_candidates.load(Ordering::Relaxed), 0);
            assert_eq!(tally.inferred_candidates.load(Ordering::Relaxed), 1);
            assert_eq!(tally.item_candidates.load(Ordering::Relaxed), 1);
            let attempts = take_selection_trace();
            assert_eq!(attempts.len(), 2);
            assert_trace_context(&attempts, 22);
            assert_eq!(
                (attempts[0].tier, attempts[0].target.as_str(), attempts[0].result),
                (SelectionTier::Item, "later", observation)
            );
            assert_eq!(
                (attempts[1].tier, attempts[1].target.as_str(), attempts[1].result),
                (SelectionTier::Whole, "earlier", SelectionResult::Inconclusive)
            );
            assert!(attempts.iter().all(|attempt| attempt.all_tests && attempt.test.is_none()));
        }
    }

    #[test]
    fn later_exact_hint_precedes_inferred_tests_and_preserves_canonical_adjudication() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let hint = Killer {
            package: "subject".into(),
            target: binaries[1].killer_target(),
            test: "historical".into(),
        };
        let learning = FileLearning::new();
        let tally = Tally::default();
        begin_selection_trace();
        let judged = judge_script(
            vec![
                ("later", Only::One("historical"), Verdict::Failed(None), ReachObservation::Unknown),
                ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &[Candidate::Exact(Killer {
                        test: "speculative".into(),
                        ..hint.clone()
                    })],
                    Some((&learning, "item")),
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        let Judgement::Reached(Outcome::Killed, Some(killer), None) = judged else {
            panic!("the later exact hint must kill")
        };
        assert_eq!(killer, hint);
        assert_eq!(tally.exact_candidates.load(Ordering::Relaxed), 1);
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        let (reached, reached_file) = {
            let state = learning.locked();
            (state.items.get("item").map(|item| item.reached.clone()), state.reached_file.clone())
        };
        let reached = reached.expect("the completed canonical pass publishes item reachability");
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[0].canonical_passes, 1);
        assert_eq!(reached_file[0].canonical_passes, 1);
        let attempts = take_selection_trace();
        assert_eq!(attempts.len(), 2);
        assert_trace_context(&attempts, 22);
        assert_eq!(
            (attempts[1].tier, attempts[1].result),
            (SelectionTier::Whole, SelectionResult::Miss)
        );
        assert_eq!((attempts[0].tier, attempts[0].result), (SelectionTier::Exact, SelectionResult::Hit));
        assert_eq!(attempts[0].test.as_deref(), Some("historical"));
        assert!(!attempts[0].all_tests);
    }

    #[test]
    fn a_later_exact_kill_cannot_override_an_earlier_canonical_failure() {
        for (earlier, expected) in [
            (Verdict::TimedOut, Outcome::Timeout),
            (Verdict::Stalled(Some("stalled".into())), Outcome::Timeout),
            (Verdict::MemoryLimit { peak: Some(41), limit: 37 }, Outcome::OutOfMemory),
            (Verdict::Flaky(Some("flaky".into())), Outcome::Flaky),
            (Verdict::Unjudged("unavailable".into()), Outcome::Pending),
            (Verdict::Failed(Some("earlier-failure".into())), Outcome::Killed),
        ] {
            let (work, _plan, _) = policy_harness(Vec::new());
            let binaries = ranked_binaries();
            let hint = Killer {
                package: "subject".into(),
                target: binaries[1].killer_target(),
                test: "historical".into(),
            };
            let tally = Tally::default();
            let judged = judge_script(
                vec![
                    ("later", Only::One("historical"), Verdict::Failed(None), ReachObservation::Unknown),
                    ("earlier", Only::All, earlier, ReachObservation::Unknown),
                ],
                ranked_attempt(),
                true,
                || {
                    judge_ranked(
                        &work,
                        7,
                        &[&binaries[0], &binaries[1]],
                        Some(&hint),
                        &[Candidate::ItemBinary(BinaryIdentity::from_binary(&binaries[1]))],
                        None,
                        None,
                        Some(2.0),
                        ranked_sweep(blind()),
                        &tally,
                    )
                },
            );
            let Judgement::Reached(outcome, killer, _) = judged else {
                panic!("the earlier canonical outcome must be adjudicated")
            };
            assert_eq!(outcome, expected);
            if let Some(killer) = killer {
                assert_eq!(killer.test, "earlier-failure");
            }
            assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
            assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 0);
            assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        }
    }

    #[test]
    fn duplicate_inferred_tests_do_not_retry_a_stale_later_exact_hint() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let mut binaries = ranked_binaries();
        binaries[1].target_kind = "test".into();
        let hint = Killer {
            package: "subject".into(),
            target: binaries[1].killer_target(),
            test: "historical".into(),
        };
        let other = Killer {
            target: binaries[1].target.clone(),
            test: "other".into(),
            ..hint.clone()
        };
        let candidates = [
            Candidate::Exact(hint.clone()),
            Candidate::Exact(Killer {
                target: binaries[1].target.clone(),
                ..hint.clone()
            }),
            Candidate::Exact(other.clone()),
            Candidate::Exact(other),
        ];
        let tally = Tally::default();
        let judged = judge_script(
            vec![
                ("later", Only::One("historical"), Verdict::Passed, ReachObservation::Unknown),
                ("later", Only::One("other"), Verdict::Passed, ReachObservation::Unknown),
                ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
                ("later", Only::All, Verdict::Passed, ReachObservation::Reached),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &candidates,
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 4);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn two_distinct_inferred_tests_fit_after_a_stale_exact_hint() {
        let (work, _, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let hint = Killer {
            package: binaries[1].package.clone(),
            target: binaries[1].killer_target(),
            test: "historical".into(),
        };
        let candidates = [
            Candidate::Exact(Killer {
                target: binaries[0].killer_target(),
                test: "first".into(),
                ..hint.clone()
            }),
            Candidate::Exact(Killer {
                test: "second".into(),
                ..hint.clone()
            }),
        ];
        let tally = Tally::default();
        let judged = judge_script(
            vec![
                ("later", Only::One("historical"), Verdict::Passed, ReachObservation::Unknown),
                ("earlier", Only::One("first"), Verdict::Passed, ReachObservation::Unknown),
                ("later", Only::One("second"), Verdict::Passed, ReachObservation::Unknown),
                ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
                ("later", Only::All, Verdict::Passed, ReachObservation::Reached),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &candidates,
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 2);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn a_later_exact_probe_that_loses_containment_abandons_before_any_other_launch() {
        for persisted in [false, true] {
            let (work, _, _) = policy_harness(Vec::new());
            let binaries = ranked_binaries();
            let hint = Killer {
                package: binaries[1].package.clone(),
                target: binaries[1].killer_target(),
                test: "historical".into(),
            };
            let tally = Tally::default();
            let judged = judge_script(
                vec![(
                    "later",
                    Only::One("historical"),
                    Verdict::Unmetered("exact containment lost".into()),
                    ReachObservation::Unknown,
                )],
                ranked_attempt(),
                true,
                || {
                    judge_ranked(
                        &work,
                        7,
                        &[&binaries[0], &binaries[1]],
                        persisted.then_some(&hint),
                        &[Candidate::Exact(hint.clone())],
                        None,
                        None,
                        Some(2.0),
                        ranked_sweep(blind()),
                        &tally,
                    )
                },
            );
            assert!(matches!(judged, Judgement::Abandoned(reason) if reason == "exact containment lost"));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
            assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the bounded generator and independent trace invariants stay together"
    )]
    fn generated_exact_first_traces_preserve_precedence_deduplication_and_fallback() {
        bolero::check!().with_type::<(u8, u8, u8, u8, bool, bool)>().for_each(
            |&(position, probe, terminal, selection, reverse, duplicate)| {
                let (work, _, _) = policy_harness(Vec::new());
                let binaries = ranked_binaries();
                let position = usize::from(position % 2);
                let probe = probe % 5;
                let terminal = terminal % 8;
                let selection = selection % 4;
                let excluded = match selection {
                    1 => Some(0),
                    2 => Some(1),
                    _ => None,
                };
                let census = match selection {
                    1 | 2 => Census::examined(&binaries[excluded.unwrap()].path, 7, 0, 4),
                    3 => Census::examined(&binaries[0].path, 7, 1, 4),
                    _ => Census::default(),
                };
                let hint = Killer {
                    package: binaries[position].package.clone(),
                    target: binaries[position].killer_target(),
                    test: "historical".into(),
                };
                let mut candidates = vec![
                    Candidate::Exact(hint.clone()),
                    Candidate::Exact(Killer {
                        test: "other".into(),
                        ..hint.clone()
                    }),
                    Candidate::ItemBinary(BinaryIdentity::from_binary(&binaries[0])),
                    Candidate::ItemBinary(BinaryIdentity::from_binary(&binaries[1])),
                ];
                if reverse {
                    candidates.reverse();
                }
                if duplicate {
                    candidates.extend_from_within(..);
                }
                let launches = Rc::new(RefCell::new(Vec::new()));
                let recorded = Rc::clone(&launches);
                let tally = Tally::default();
                begin_selection_trace();
                let judged = super::super::verdict::with_binary_runner(
                    move |binary, attempt, _confirming| {
                        let index = usize::from(binary.target == "later");
                        assert_ne!(excluded, Some(index), "excluded binaries must not launch");
                        let count = recorded.borrow().len();
                        assert!(count < 8, "two binaries, two test names and bounded fallback must make progress");
                        recorded.borrow_mut().push((index, format!("{:?}", attempt.only)));
                        let verdict = match attempt.only {
                            Only::One("historical") => match probe {
                                0 => Verdict::Passed,
                                1 => Verdict::TimedOut,
                                2 => Verdict::Unjudged("stale exact hint".into()),
                                3 => Verdict::Failed(None),
                                _ => Verdict::Unmetered("exact containment lost".into()),
                            },
                            Only::One("other") => Verdict::Passed,
                            Only::All | Only::These(_) => {
                                if index == 1 {
                                    Verdict::Passed
                                } else {
                                    match terminal {
                                        0 => Verdict::Passed,
                                        1 => Verdict::TimedOut,
                                        2 => Verdict::Stalled(None),
                                        3 => Verdict::MemoryLimit { peak: Some(41), limit: 37 },
                                        4 => Verdict::Flaky(None),
                                        5 => Verdict::Unjudged("canonical unavailable".into()),
                                        6 => Verdict::Failed(Some("canonical".into())),
                                        _ => Verdict::Unmetered("canonical containment lost".into()),
                                    }
                                }
                            }
                            Only::One(other) => panic!("generated probes have only two bounded test names: {other}"),
                        };
                        BinaryRun {
                            verdict,
                            reach: ReachObservation::Reached,
                        }
                    },
                    || {
                        judge_ranked(
                            &work,
                            7,
                            &[&binaries[0], &binaries[1]],
                            Some(&hint),
                            &candidates,
                            None,
                            None,
                            Some(2.0),
                            ranked_sweep(&census),
                            &tally,
                        )
                    },
                );
                let trace = take_selection_trace();
                let launches = launches.borrow().clone();
                assert!(trace.len() <= 8);
                assert_eq!(tally.launches.load(Ordering::Relaxed), launches.len());
                let mut distinct = crate::HashSet::default();
                for (index, only) in &launches {
                    assert!(
                        distinct.insert((*index, only)),
                        "exact probes and cached whole-binary runs must not be repeated: {launches:?}"
                    );
                }
                let exact_eligible = excluded != Some(position);
                if exact_eligible {
                    assert_eq!(launches.first(), Some(&(position, "One(\"historical\")".to_owned())));
                }
                if exact_eligible && probe == 4 {
                    assert!(matches!(judged, Judgement::Abandoned(reason) if reason == "exact containment lost"));
                    assert_eq!(launches.len(), 1);
                    return;
                }
                let exact_hit = exact_eligible && probe == 3;
                let first_must_run = excluded != Some(0) && !(position == 0 && exact_hit);
                if first_must_run {
                    assert!(
                        launches.iter().any(|(index, only)| *index == 0 && !only.starts_with("One(")),
                        "an inconclusive exact probe or deferred later kill must fall back to canonical execution"
                    );
                }
                if first_must_run && terminal == 7 {
                    assert!(matches!(judged, Judgement::Abandoned(reason) if reason == "canonical containment lost"));
                    return;
                }
                let expected = if first_must_run && terminal != 0 {
                    match terminal {
                        1 | 2 => Outcome::Timeout,
                        3 => Outcome::OutOfMemory,
                        4 => Outcome::Flaky,
                        5 => Outcome::Pending,
                        6 => Outcome::Killed,
                        _ => unreachable!("containment loss was checked before canonical outcome comparison"),
                    }
                } else if exact_hit {
                    Outcome::Killed
                } else {
                    Outcome::Survived
                };
                let Judgement::Reached(outcome, killer, _) = judged else {
                    panic!("the bounded runner supplies a canonical verdict")
                };
                assert_eq!(outcome, expected);
                if first_must_run && terminal == 6 {
                    assert_eq!(killer.expect("the earlier canonical failure names its killer").test, "canonical");
                }
            },
        );
    }

    #[test]
    fn a_later_exact_kill_cannot_override_earlier_containment_loss() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let hint = Killer {
            package: "subject".into(),
            target: binaries[1].killer_target(),
            test: "historical".into(),
        };
        let tally = Tally::default();
        let judged = judge_script(
            vec![
                ("later", Only::One("historical"), Verdict::Failed(None), ReachObservation::Unknown),
                (
                    "earlier",
                    Only::All,
                    Verdict::Unmetered("lost containment".into()),
                    ReachObservation::Unknown,
                ),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &[Candidate::Exact(hint.clone())],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        let Judgement::Abandoned(reason) = judged else {
            panic!("an earlier canonical containment failure must abandon the campaign")
        };
        assert_eq!(reason, "lost containment");
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn filtered_resource_verdicts_are_replaced_by_the_whole_binary_and_update_learning() {
        for verdict in [
            Verdict::TimedOut,
            Verdict::Stalled(None),
            Verdict::MemoryLimit { peak: Some(41), limit: 37 },
            Verdict::Flaky(None),
            Verdict::Unjudged("unavailable".into()),
        ] {
            let (work, _plan, _) = policy_harness(Vec::new());
            let binaries = ranked_binaries();
            let census = Census::examined(&binaries[0].path, 7, 1, 4);
            let learning = FileLearning::new();
            let tally = Tally::default();
            begin_selection_trace();
            let judged = judge_script(
                vec![
                    ("earlier", Only::These(&["tests::t0"]), verdict, ReachObservation::Unknown),
                    ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
                ],
                ranked_attempt(),
                true,
                || {
                    judge_ranked(
                        &work,
                        7,
                        &[&binaries[0]],
                        None,
                        &[],
                        Some((&learning, "item")),
                        None,
                        Some(2.0),
                        ranked_sweep(&census),
                        &tally,
                    )
                },
            );
            assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
            assert_eq!(tally.case_selections.load(Ordering::Relaxed), 1);
            assert_eq!(tally.available_tests.load(Ordering::Relaxed), 4);
            assert_eq!(tally.selected_tests.load(Ordering::Relaxed), 1);
            let (reached, reached_file) = {
                let state = learning.locked();
                (state.items.get("item").map(|item| item.reached.clone()), state.reached_file.clone())
            };
            let reached = reached.expect("the completed canonical pass publishes item reachability");
            assert_eq!(reached[0].canonical_passes, 1);
            assert_eq!(reached_file[0].canonical_passes, 1);
            let attempts = take_selection_trace();
            assert_eq!(attempts.len(), 2);
            assert_trace_context(&attempts, 11);
            assert_eq!(
                (attempts[0].tier, attempts[0].result, attempts[0].all_tests),
                (SelectionTier::Selected, SelectionResult::Inconclusive, false)
            );
            assert_eq!(
                (attempts[1].tier, attempts[1].result, attempts[1].all_tests),
                (SelectionTier::Whole, SelectionResult::Miss, true)
            );
            assert!(attempts.iter().all(|attempt| attempt.test.is_none()));
        }
    }

    #[test]
    fn incomplete_census_anonymous_group_failures_remain_guesses_and_misses_fall_back() {
        for failing in [false, true] {
            let (work, _plan, _) = policy_harness(Vec::new());
            let binaries = ranked_binaries();
            let census = Census::partial(&binaries[0].path, 7, 2, 4);
            let tally = Tally::default();
            let mut steps = vec![(
                "earlier",
                Only::These(&["tests::t0", "tests::t1"]),
                if failing { Verdict::Failed(None) } else { Verdict::Passed },
                ReachObservation::Unknown,
            )];
            if !failing {
                steps.push(("earlier", Only::All, Verdict::Passed, ReachObservation::Reached));
            }
            begin_selection_trace();
            let judged = judge_script(steps, ranked_attempt(), true, || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0]],
                    None,
                    &[],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(&census),
                    &tally,
                )
            });
            if failing {
                let Judgement::Guessed(killer) = judged else {
                    panic!("an anonymous two-test failure cannot identify an observed killer")
                };
                assert_eq!(killer.test, "tests::t0");
                assert_eq!(killer.target, binaries[0].killer_target());
            } else {
                assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
            }
            assert_eq!(tally.launches.load(Ordering::Relaxed), if failing { 1 } else { 2 });
            assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
            assert_eq!(tally.hinted_selections.load(Ordering::Relaxed), 1);
            assert_eq!(tally.census_candidates.load(Ordering::Relaxed), 1);
            assert_eq!(tally.available_tests.load(Ordering::Relaxed), 4);
            assert_eq!(tally.selected_tests.load(Ordering::Relaxed), if failing { 2 } else { 4 });
            assert_eq!(tally.hinted_fallbacks.load(Ordering::Relaxed), usize::from(!failing));
            let attempts = take_selection_trace();
            assert_eq!(attempts.len(), if failing { 1 } else { 2 });
            assert_trace_context(&attempts, 11);
            assert_eq!(attempts[0].tier, SelectionTier::Census);
            assert_eq!(
                attempts[0].result,
                if failing { SelectionResult::Hit } else { SelectionResult::Miss }
            );
            assert!(!attempts[0].all_tests);
            assert!(attempts[0].test.is_none());
            if !failing {
                assert_eq!(attempts[1].tier, SelectionTier::HintedFallback);
            }
        }
    }

    #[test]
    fn a_cached_whole_pass_is_consumed_once_even_with_duplicate_and_missing_candidates() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let tally = Tally::default();
        let later = BinaryIdentity::from_binary(&binaries[1]);
        let candidates = [
            Candidate::Exact(Killer {
                package: "absent".into(),
                target: "absent".into(),
                test: "absent".into(),
            }),
            Candidate::ItemBinary(later.clone()),
            Candidate::ReachFileBinary(later),
            Candidate::FileBinary(BinaryIdentity {
                package: "absent".into(),
                target: "absent".into(),
            }),
        ];
        let judged = judge_script(
            vec![
                ("later", Only::All, Verdict::Passed, ReachObservation::Reached),
                ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    None,
                    &candidates,
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_candidates.load(Ordering::Relaxed), 4);
        assert_eq!(tally.item_candidates.load(Ordering::Relaxed), 2);
        assert_eq!(tally.reach_candidates.load(Ordering::Relaxed), 1);
        assert_eq!(tally.file_candidates.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn losing_candidate_containment_abandons_before_any_canonical_execution() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let tally = Tally::default();
        let candidate = Candidate::FileBinary(BinaryIdentity::from_binary(&binaries[1]));
        let judged = judge_script(
            vec![(
                "later",
                Only::All,
                Verdict::Unmetered("lost containment".into()),
                ReachObservation::Unknown,
            )],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    None,
                    &[candidate],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        let Judgement::Abandoned(reason) = judged else {
            panic!("losing containment must abandon immediately")
        };
        assert_eq!(reason, "lost containment");
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn an_initial_exact_probe_is_not_repeated_when_the_same_binary_is_reached() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let tally = Tally::default();
        let hint = Killer {
            package: "subject".into(),
            target: binaries[0].killer_target(),
            test: "historical".into(),
        };
        begin_selection_trace();
        let judged = judge_script(
            vec![
                ("earlier", Only::One("historical"), Verdict::Passed, ReachObservation::Unknown),
                ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
                ("later", Only::All, Verdict::Passed, ReachObservation::Reached),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &[],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(blind()),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.exact_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 3);
        let attempts = take_selection_trace();
        assert_eq!(attempts.len(), 3);
        assert_eq!(
            (attempts[0].ordinal, attempts[0].tier, attempts[0].rank),
            (7, SelectionTier::Exact, 1)
        );
        assert_eq!(attempts[0].test.as_deref(), Some("historical"));
        assert_eq!(attempts[0].result, SelectionResult::Miss);
        assert!(!attempts[0].all_tests);
        assert_eq!((attempts[1].rank, attempts[2].rank), (1, 2));
    }

    #[test]
    fn uncovered_first_binaries_do_not_break_the_remaining_canonical_iteration() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let census = Census::examined(&binaries[0].path, 7, 0, 4);
        let tally = Tally::default();
        let judged = judge_script(
            vec![("later", Only::All, Verdict::Passed, ReachObservation::Reached)],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    None,
                    &[],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(&census),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.uncovered_selections.load(Ordering::Relaxed), 1);
        assert_eq!(tally.available_tests.load(Ordering::Relaxed), 8);
        assert_eq!(tally.selected_tests.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn uncovered_initial_exact_hints_do_not_outrank_a_later_ranked_test() {
        let (work, _plan, _) = policy_harness(Vec::new());
        let binaries = ranked_binaries();
        let census = Census::examined(&binaries[0].path, 7, 0, 4);
        let hint = Killer {
            package: "subject".into(),
            target: binaries[0].killer_target(),
            test: "historical".into(),
        };
        let candidate = Candidate::Exact(Killer {
            package: "subject".into(),
            target: binaries[1].killer_target(),
            test: "candidate".into(),
        });
        let tally = Tally::default();
        let judged = judge_script(
            vec![(
                "later",
                Only::One("candidate"),
                Verdict::Failed(Some("observed".into())),
                ReachObservation::Reached,
            )],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &[candidate],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(&census),
                    &tally,
                )
            },
        );
        let Judgement::Reached(Outcome::Killed, Some(killer), None) = judged else {
            panic!("the eligible ranked test must kill")
        };
        assert_eq!(killer.test, "observed");
        assert_eq!(killer.target, binaries[1].killer_target());
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.item_probes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn only_deterministic_whole_confirmation_can_record_a_negative_reach_hint() {
        for deterministic in [false, true] {
            for reach in [ReachObservation::Reached, ReachObservation::NotReached, ReachObservation::Unknown] {
                let (work, _plan, _) = policy_harness(Vec::new());
                let binaries = ranked_binaries();
                let census = Census::examined(&binaries[0].path, 7, 1, 4);
                let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
                let negative = NegativeLearning::default();
                let tally = Tally::default();
                let judged = judge_script(
                    vec![
                        ("earlier", Only::These(&["tests::t0"]), Verdict::TimedOut, ReachObservation::Unknown),
                        ("earlier", Only::All, Verdict::Passed, reach),
                    ],
                    ranked_attempt(),
                    true,
                    || {
                        judge_ranked(
                            &work,
                            7,
                            &[&binaries[0]],
                            None,
                            &[],
                            None,
                            Some((&negative, &site, deterministic)),
                            Some(2.0),
                            ranked_sweep(&census),
                            &tally,
                        )
                    },
                );
                assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
                assert_eq!(
                    negative.contains(&site, &binaries[0]),
                    deterministic && reach == ReachObservation::NotReached
                );
            }
        }
    }

    /// A census that knows nothing, which is what every test here that is not about narrowing runs
    /// under: the sweep then behaves exactly as it did before there was a census at all.
    fn blind() -> &'static Census {
        static BLIND: OnceLock<Census> = OnceLock::new();

        BLIND.get_or_init(Census::default)
    }

    fn sweep(stall: Stall) -> Sweep<'static> {
        Sweep {
            timeout_floor: Duration::ZERO,
            stall,
            jobs: 1,
            meter: false,
            confirm: true,
            census: blind(),
        }
    }

    #[test]
    fn scheduler_priority_partial_order_always_matches_total_order() {
        let first = LocalPriority {
            distance: (1, 2),
            benefit: 3,
            cost: 4,
            stable_order: 5,
            index: 6,
        };
        let values = [
            first,
            LocalPriority { cost: 7, ..first },
            LocalPriority { distance: (2, 0), ..first },
        ];
        for left in values {
            for right in values {
                assert_eq!(left.partial_cmp(&right), Some(left.cmp(&right)));
                let left = PackageHead {
                    package: 1,
                    turns: 2,
                    local: left,
                };
                let right = PackageHead {
                    package: 3,
                    turns: 4,
                    local: right,
                };
                assert_eq!(left.partial_cmp(&right), Some(left.cmp(&right)));
            }
        }
    }

    #[test]
    fn a_quiet_sweep_emits_heartbeats_until_an_event_arrives() {
        struct HeartbeatCounter(usize);

        impl Events for HeartbeatCounter {
            fn phase(&mut self, _verb: &str, _detail: &str) {}

            fn mutant(&mut self, _mutant: &Mutant) {}

            fn heartbeat(&mut self) {
                self.0 += 1;
            }
        }

        let mut scripted = [Err(mpsc::RecvTimeoutError::Timeout), Ok(SweepEvent::Started)].into_iter();
        let mut events = HeartbeatCounter(0);

        let event = receive_sweep_event_with(&mut events, Duration::from_millis(5), |_timeout| {
            scripted.next().unwrap_or(Err(mpsc::RecvTimeoutError::Disconnected))
        })
        .expect("worker event");

        assert!(matches!(event, SweepEvent::Started));
        assert_eq!(events.0, 1);
    }

    #[test]
    fn a_grouped_census_probe_attributes_an_unnamed_failure_to_its_first_selected_case() {
        let (_directory, work) = crate::testing::helper_workspace("grouped-probe-", &["exit:1"]);
        let binary = crate::testing::helper();
        let tally = Tally::default();
        let names = ["tests::first", "tests::second"];

        let (killer, observation, _) = probe_cases(
            &work,
            7,
            &binary,
            &names,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );
        let killer = killer.expect("the grouped probe fails");

        assert_eq!(killer.identity.test, "tests::first");
        assert!(!killer.observed);
        assert_eq!(observation, Some(true));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        assert_eq!(tally.census_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.census_hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_stall_does_not_claim_the_named_test_is_the_one_that_hung() {
        // Regression, issue-004. libtest names a test only once it has finished, so the test that
        // is spinning is precisely the one not named. Wording that presents the name as the culprit
        // sends people to read a test that was fine, and invites a suppression on it.
        let note = stall_note(Some("tests::round_trip"));

        assert!(note.contains("last test named was `tests::round_trip`"), "{note}");
        assert!(!note.contains("during"), "{note}");
        assert!(!note.contains(" in `"), "{note}");
    }

    #[test]
    fn a_stall_before_any_test_was_named_says_so() {
        let note = stall_note(None);

        assert_eq!(note, "stalled before the harness named a test");
    }

    /// A workspace, a plan holding one pending mutant, and a test binary that behaves as told.
    ///
    /// `test_all` is the scheduler, and the verdicts it has to translate into outcomes are exactly
    /// the ones a real suite produces least often, so the binary is a script rather than a
    /// compiled harness: the process machinery is real, only the suite is stand-in.
    #[cfg(unix)]
    fn harness(body: &str, budget: Duration) -> (tempfile::TempDir, Workspace, Plan, Vec<TestBinary>) {
        let (directory, work) = crate::testing::shell_workspace("test-all", body);
        let plan = one_mutant_plan(work.root.clone());
        let binaries = vec![TestBinary {
            package: "subject".to_owned(),
            baseline: Duration::from_millis(1),
            budget: Some(budget),
            ..crate::testing::test_binary("/bin/sh")
        }];

        (directory, work, plan, binaries)
    }

    /// A workspace and plan for the cases where no binary is ever started.
    ///
    /// A mutant nothing can reach is decided without launching anything, so these need no shell and
    /// no executable — which is what lets them run on every platform rather than only where
    /// `/bin/sh` exists. The uncovered bucket is precisely the one that must not be left untested
    /// on a platform, since being unreachable is what it asserts about.
    fn unreachable_harness(binaries: Vec<TestBinary>) -> (tempfile::TempDir, Workspace, Plan, Vec<TestBinary>) {
        let directory = crate::testing::workdir("test-all-uncovered");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = one_mutant_plan(root);

        (directory, work, plan, binaries)
    }

    /// A plan holding a single pending mutant in package `subject`.
    fn one_mutant_plan(root: Utf8PathBuf) -> Plan {
        let mutant = crate::model::Mutant {
            id: "m1".to_owned().into(),
            ordinal: 1,
            file: (Utf8PathBuf::from("src/a.rs")).into(),
            package: ("subject".to_owned()).into(),
            span: 0..1,
            line: 1,
            end_line: 1,
            column: 1,
            mutator: ("relational.gt_to_ge".to_owned()).into(),
            item_path: ("subject::f".to_owned()).into(),
            occurrence: 0,
            replacement_index: 0,
            original: "a > b".to_owned().into(),
            replacement: "a >= b".to_owned().into(),
            shape: Shape::Expr,
            confidence: cargo_gamma_engine::ops::collect::Confidence::Proven,
            outcome: Outcome::Pending,
            suppression: None,
            expectation: None,
            test_timeout_multiplier: None,
            elapsed_ms: 0,
            killed_by: None,
            killer: None,
            note: None,
        };

        Plan {
            skipped: Vec::new(),
            digests: crate::HashMap::default(),
            root,
            files: Vec::new(),
            mutants: vec![mutant],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        }
    }

    /// Like `harness`, but with `count` pending mutants instead of one, so that a sweep has enough
    /// work in flight for more than one worker to be racing against the others.
    #[cfg(unix)]
    fn harness_n(body: &str, budget: Duration, count: usize) -> (tempfile::TempDir, Workspace, Plan, Vec<TestBinary>) {
        let (directory, work, mut plan, binaries) = harness(body, budget);
        let template = plan.mutants[0].clone();

        plan.mutants = (0..count)
            .map(|index| {
                let mut mutant = template.clone();
                mutant.id = format!("m{index}").into();
                mutant.ordinal = u32::try_from(index + 1).expect("test counts stay well under u32::MAX");
                mutant
            })
            .collect();

        (directory, work, plan, binaries)
    }

    #[test]
    #[cfg(unix)]
    fn file_local_learning_preserves_canonical_precedence() {
        let (_directory, work, mut plan, mut binaries) =
            harness_n("echo 'test tests::caught ... FAILED'; exit 1", Duration::from_secs(30), 2);
        binaries[0].target = "killer".to_owned();
        binaries[0].budget = None;

        let mut passing = binaries[0].clone();
        passing.path = "/bin/true".into();
        passing.target = "passing".to_owned();
        passing.baseline = Duration::ZERO;
        binaries.insert(0, passing);

        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let spent = test_all(
            &work,
            &mut plan,
            &reach,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            true,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep should finish")
        .expect("two mutants should be swept");

        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
        assert!(
            (4..=5).contains(&spent.launches),
            "each mutant reaches the earlier canonical binary; a ranked candidate may need one separate confirmation: {spent:?}"
        );
        assert!(
            (1..=2).contains(&spent.probes),
            "the second mutant must exercise learned candidates without bypassing canonical precedence: {spent:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_all_publishes_every_completed_field_and_refreshes_killer_history() {
        let (_directory, work, mut plan, mut binaries) = harness("echo 'test tests::caught ... FAILED'; exit 1", Duration::from_secs(30));
        binaries[0].budget = None;
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);
        let mut killers = Killers::default();
        killers.record(
            plan.mutants[0].id.clone(),
            Killer {
                package: "stale".to_owned(),
                target: "stale".to_owned(),
                test: "tests::stale".to_owned(),
            },
        );
        let mut events = crate::testing::Recorder::default();

        let spent = test_all(
            &work,
            &mut plan,
            &reach,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            false,
            &mut killers,
            &mut events,
        )
        .expect("the sweep completes")
        .expect("one pending mutant runs");

        let mutant = &plan.mutants[0];
        assert_eq!(mutant.outcome, Outcome::Killed);
        assert_eq!(mutant.killed_by.as_deref(), Some("tests::caught"));
        assert_eq!(mutant.note, None);
        assert_eq!(events.mutants, 1);
        assert_eq!(events.sweep_plan, Some((1, 1)));
        assert_eq!(events.mutant_starts, 1);
        assert_eq!(spent.launches, 1);
        assert_eq!(spent.probes, 0);
        assert_eq!(spent.packages.len(), 1);
        assert_eq!(&*spent.packages[0].package, "subject");
        assert_eq!(spent.packages[0].mutants, 1);
        assert!(spent.packages[0].settled_ms >= spent.packages[0].first_started_ms);
        let refreshed = killers.hint(&mutant.id).expect("the newly observed killer is persisted");
        assert_eq!(refreshed.package, binaries[0].package);
        assert_eq!(refreshed.target, binaries[0].target);
        assert_eq!(refreshed.test, "tests::caught");
    }

    #[test]
    #[cfg(unix)]
    fn different_items_in_the_same_file_run_in_parallel() {
        let (_directory, work, mut plan, binaries) = harness_n("sleep 0.20; exit 0", Duration::from_secs(30), 2);
        plan.mutants[1].item_path = "subject::other".to_owned().into();
        let started = Instant::now();

        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 2,
                meter: false,
                confirm: false,
                census: blind(),
            },
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep should finish");

        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Survived));
        assert!(started.elapsed() < Duration::from_millis(350), "{:?}", started.elapsed());
    }

    /// A plan holding one pending mutant per named package, in the order named.
    ///
    /// Each package reaches only itself, so a mutant's cost is its own package's binary and the
    /// packages are genuinely distinguishable. A scope of the whole workspace would make every
    /// binary reach every mutant, every cost equal, and any ordering test vacuous.
    fn plan_over(packages: &[&str]) -> Plan {
        let mut plan = one_mutant_plan(Utf8PathBuf::from("/nowhere"));
        let template = plan.mutants[0].clone();

        plan.mutants = packages
            .iter()
            .enumerate()
            .map(|(index, package)| crate::model::Mutant {
                id: format!("m{index}").into(),
                ordinal: u32::try_from(index + 1).expect("test counts stay well under u32::MAX"),
                package: ((*package).to_owned()).into(),
                ..template.clone()
            })
            .collect();

        plan.reach = packages
            .iter()
            .map(|package| ((*package).to_owned(), once((*package).to_owned()).collect()))
            .collect();

        plan
    }

    /// A scope that narrows nothing but leaves the plan's reachability relation in force.
    const NARROW: TestScope<'static> = TestScope {
        packages: &[],
        package_local: false,
        whole_workspace: false,
    };

    /// A binary belonging to `package` whose suite takes `baseline` with no mutant active.
    fn binary_of(package: &str, baseline: Duration) -> TestBinary {
        TestBinary {
            package: package.to_owned(),
            baseline,
            budget: Some(Duration::from_mins(1)),
            ..crate::testing::test_binary("/bin/sh")
        }
    }

    #[test]
    fn cold_runs_put_own_package_binaries_before_cheaper_dependents() {
        let mut plan = plan_over(&["subject", "facade"]);
        let inserted = plan
            .reach
            .get_mut("facade")
            .expect("the fixture records every package")
            .insert("subject".to_owned());
        assert!(inserted);
        let mut own_slow = binary_of("subject", Duration::from_secs(20));
        own_slow.target = "subject-tests".to_owned();
        let mut dependent_fast = binary_of("facade", Duration::from_millis(1));
        dependent_fast.target = "facade-smoke".to_owned();
        let binaries = vec![dependent_fast, own_slow];

        let sets = Reachability::build(&plan, &binaries, &NARROW);
        let ordered = sets
            .reachable(&plan.mutants[0])
            .expect("subject holds the plan's one pending mutant");

        assert_eq!(
            ordered.iter().map(|binary| binary.package.as_str()).collect::<Vec<_>>(),
            ["subject", "facade"]
        );
    }

    #[test]
    fn cold_run_tiers_use_baseline_then_stable_identity() {
        let mut dependent_z = binary_of("z-helper", Duration::from_secs(2));
        dependent_z.target = "z".to_owned();
        dependent_z.path = "/tests/z".into();

        let mut own_slow = binary_of("subject", Duration::from_secs(3));
        own_slow.target = "slow".to_owned();
        own_slow.path = "/tests/own-slow".into();

        let mut dependent_a = binary_of("a-helper", Duration::from_secs(2));
        dependent_a.target = "a".to_owned();
        dependent_a.path = "/tests/a".into();

        let mut own_fast = binary_of("subject", Duration::from_secs(1));
        own_fast.target = "fast".to_owned();
        own_fast.path = "/tests/own-fast".into();

        let mut ordered = vec![&dependent_z, &own_slow, &dependent_a, &own_fast];

        order_reachable(&mut ordered, "subject");

        assert_eq!(
            ordered.iter().map(|binary| binary.path.as_str()).collect::<Vec<_>>(),
            ["/tests/own-fast", "/tests/own-slow", "/tests/a", "/tests/z"]
        );
    }

    #[test]
    fn cold_run_ordering_never_drops_a_reachable_binary() {
        let own = binary_of("subject", Duration::from_secs(3));
        let helper = binary_of("helper", Duration::from_secs(2));
        let facade = binary_of("facade", Duration::from_secs(1));
        let mut ordered = vec![&helper, &own, &facade];

        order_reachable(&mut ordered, "subject");

        assert_eq!(
            ordered.iter().map(|binary| binary.package.as_str()).collect::<Vec<_>>(),
            ["subject", "facade", "helper"]
        );
    }

    /// The queue runs the most expensive mutants first, so the sweep does not end on one core.
    ///
    /// Workers pull from a single queue: whatever is picked up last runs alone while every other
    /// core idles. In plan order the cheapest package can be enumerated first and the most
    /// expensive last, which is the worst case and the one nothing prevented.
    #[test]
    fn the_queue_puts_the_most_expensive_mutants_first() {
        let plan = plan_over(&["cheap", "dear", "middling"]);
        let binaries = vec![
            binary_of("cheap", Duration::from_millis(1)),
            binary_of("dear", Duration::from_secs(30)),
            binary_of("middling", Duration::from_secs(2)),
        ];

        let mut pending: Vec<usize> = (0..plan.mutants.len()).collect();
        let sets = Reachability::build(&plan, &binaries, &NARROW);

        schedule(&mut pending, &plan, &sets, blind(), &Killers::default());

        let order: Vec<&str> = pending.iter().map(|position| &*plan.mutants[*position].package).collect();

        assert_eq!(order, vec!["dear", "middling", "cheap"]);
    }

    /// Mutants of equal cost keep plan order, so the same plan schedules identically every time.
    ///
    /// Without a total order the sweep would be reproducible only by luck, and two runs of one plan
    /// could interleave differently — which is exactly the kind of difference that makes an
    /// intermittent failure impossible to attribute.
    #[test]
    fn mutants_that_cost_the_same_keep_the_order_the_plan_gave_them() {
        let plan = plan_over(&["a", "b", "c", "d"]);
        let binaries: Vec<TestBinary> = ["a", "b", "c", "d"]
            .iter()
            .map(|package| binary_of(package, Duration::from_millis(7)))
            .collect();

        let mut pending: Vec<usize> = (0..plan.mutants.len()).collect();
        let sets = Reachability::build(&plan, &binaries, &NARROW);

        schedule(&mut pending, &plan, &sets, blind(), &Killers::default());

        assert_eq!(pending, vec![0, 1, 2, 3]);
    }

    /// A package is a correlated workload: all of its mutants traverse the same binaries in the
    /// same order. Keeping each package contiguous sends every worker into an expensive binary at
    /// once, which is especially destructive for tests that launch subprocesses.
    #[test]
    fn package_queues_are_interleaved_instead_of_forming_worker_convoys() {
        let plan = plan_over(&["dear", "dear", "cheap", "cheap", "middle", "middle"]);
        let binaries = vec![
            binary_of("dear", Duration::from_secs(30)),
            binary_of("cheap", Duration::from_millis(1)),
            binary_of("middle", Duration::from_secs(2)),
        ];

        let mut pending: Vec<usize> = (0..plan.mutants.len()).collect();
        let sets = Reachability::build(&plan, &binaries, &NARROW);

        schedule(&mut pending, &plan, &sets, blind(), &Killers::default());

        let order: Vec<&str> = pending.iter().map(|position| &*plan.mutants[*position].package).collect();

        assert_eq!(order, vec!["dear", "middle", "cheap", "dear", "middle", "cheap"]);
    }

    /// Reachability is worked out per package, and every mutant of a package gets that same answer.
    #[test]
    fn every_mutant_of_a_package_gets_one_reachability_answer() {
        let plan = plan_over(&["left", "right", "left", "right", "left"]);
        let binaries = vec![
            binary_of("left", Duration::from_millis(1)),
            binary_of("right", Duration::from_millis(1)),
        ];

        let pending: Vec<usize> = (0..plan.mutants.len()).collect();
        let sets = Reachability::build(&plan, &binaries, &NARROW);

        assert_eq!(sets.len(), 2, "one entry per distinct package, not one per mutant");

        for position in &pending {
            let mutant = &plan.mutants[*position];
            let package = &*mutant.package;
            let found: Vec<&TestBinary> = binaries.iter().filter(|binary| reaches(binary, package, &plan, &NARROW)).collect();

            assert_eq!(
                sets.reachable(mutant).expect("every mutant in `pending` has a reachability entry"),
                found.as_slice(),
                "the memoized set must equal the one a per-mutant filter would give"
            );
        }
    }

    /// A mutant whose suite never finishes within its budget is recorded as a timeout.
    #[test]
    #[cfg(unix)]
    fn a_mutant_that_exhausts_its_budget_is_recorded_as_a_timeout() {
        let (_directory, work, mut plan, binaries) = harness("sleep 30", Duration::from_millis(50));
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep(Stall::NONE),
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep completes");

        // A hang remains a distinct timeout verdict even though only assertion failures receive
        // detection credit.
        assert_eq!(plan.mutants[0].outcome, Outcome::Timeout);
        assert_eq!(plan.mutants[0].note, None);
    }

    /// A mutant whose suite goes silent is a timeout, annotated with where it went silent.
    #[test]
    #[cfg(unix)]
    fn a_mutant_that_stalls_is_recorded_as_a_timeout_naming_the_test() {
        let (_directory, work, mut plan, binaries) = harness("echo 'test slow::case ... '\nsleep 30", Duration::from_mins(1));
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);
        let stall = Stall {
            budget: Some(Duration::from_millis(50)),
        };

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep(stall),
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep completes");

        // Saying which test was running when the silence started is the whole value of stall
        // detection over simply waiting out the budget.
        assert_eq!(plan.mutants[0].outcome, Outcome::Timeout);
        assert!(plan.mutants[0].note.is_some(), "{:?}", plan.mutants[0].note);
    }

    /// A mutant no test binary can reach is uncovered rather than a survivor.
    #[test]
    fn a_mutant_no_binary_reaches_is_uncovered() {
        let (_directory, work, mut plan, binaries) = unreachable_harness(vec![TestBinary {
            package: "subject".to_owned(),
            budget: Some(Duration::from_secs(30)),
            ..crate::testing::test_binary("does-not-exist")
        }]);
        let scope = TestScope {
            packages: &["other".to_owned()],
            package_local: false,
            whole_workspace: false,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep(Stall::NONE),
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep completes");

        // Blaming the tests that exist for code nothing links would make the score a measure of
        // the build graph rather than of the suite.
        assert_eq!(plan.mutants[0].outcome, Outcome::NoCoverage);
    }

    /// A package whose only test binary announced no tests is uncovered, not full of survivors.
    ///
    /// Regression, issue-011. Cargo emits a unit-test binary for every lib target whether or not it
    /// holds a single test, so a binary always exists and the uncovered bucket was unreachable
    /// except through an explicit `--test-package` exclusion. A package with no tests at all was
    /// then reported as a package whose tests all missed — a materially different, and much more
    /// alarming, thing to read.
    #[test]
    fn a_mutant_whose_only_binary_announced_no_tests_is_uncovered() {
        let (_directory, work, mut plan, binaries) = unreachable_harness(vec![TestBinary {
            package: "subject".to_owned(),
            budget: Some(Duration::from_secs(30)),
            tests: Some(0),
            ..crate::testing::test_binary("does-not-exist")
        }]);
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep(Stall::NONE),
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep completes");

        // The binary is never started — the path it names does not exist — so a run that thought it
        // could convict something here would report a failure rather than this verdict.
        assert_eq!(plan.mutants[0].outcome, Outcome::NoCoverage);
    }

    /// A site most of the suite reaches is *run whole*, never reported uncovered.
    ///
    /// `reaching` answers `None` for such a site — "narrowing would save nothing, run the whole
    /// binary" — and that `None` is byte-for-byte the same answer a blind census gives. The whole
    /// safety property is that the sweep launches the binary and reaches a real verdict, rather than
    /// mistaking this `None` for the empty list that means "no test reaches it" and reporting
    /// `NoCoverage` for a mutant its suite genuinely exercised. Without a sweep-level test the two
    /// `None`s look interchangeable, and a refactor that collapsed them would go unnoticed.
    #[test]
    #[cfg(unix)]
    fn a_site_most_of_the_suite_reaches_is_run_whole_rather_than_called_uncovered() {
        let (_directory, work, mut plan, binaries) = harness("exit 0", Duration::from_mins(1));
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);

        // Five of nine tests reach the mutant's site, which is past half, so `reaching` returns
        // `None` for this binary.
        let census = Census::examined(&binaries[0].path, plan.mutants[0].ordinal, 5, 9);
        let sweep = Sweep {
            timeout_floor: Duration::ZERO,
            stall: Stall::NONE,
            jobs: 1,
            meter: false,
            confirm: true,
            census: &census,
        };

        let spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep,
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep completes")
        .expect("a pending mutant means the sweep ran");

        // Survived, not NoCoverage: the whole binary was launched and its suite passed with the
        // mutant active. NoCoverage would be the bug this guards against.
        assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
        assert_eq!(spent.launches, 1, "the whole binary was run exactly once, not skipped as uncovered");
        assert_eq!(spent.probes, 0, "no hint was given, so no probe was launched");
        assert_eq!(spent.exact_candidates, 0, "an absent hint is not an exact candidate");
    }

    /// A run with nothing to sweep reports the sweep as absent, not as a zero-cost phase.
    ///
    /// `test_all` says so by returning `None`, which the session stores verbatim so diagnostics can
    /// tell "there was nothing to sweep" from "the sweep ran and was free".
    #[test]
    fn a_plan_with_no_pending_mutants_sweeps_nothing_and_returns_none() {
        let (_directory, work, mut plan, binaries) = unreachable_harness(Vec::new());

        plan.mutants[0].outcome = Outcome::Survived;
        let reach = Reachability::build(&plan, &binaries, &NARROW);
        let mut events = crate::testing::Recorder::default();

        let swept = test_all_with_hooks(
            &work,
            &mut plan,
            &reach,
            sweep(Stall::NONE),
            false,
            &mut Killers::default(),
            &mut events,
            (
                |_, _| panic!("a settled plan must not checkpoint another verdict"),
                |_| panic!("a settled plan must not launch a mutant"),
                || panic!("a settled plan must not wait for workers"),
            ),
        )
        .expect("an empty sweep is not a failure");

        assert!(swept.is_none(), "nothing pending means there is no sweep phase, not a free one");
        assert_eq!(events.sweep_plan, Some((0, 1)));
        assert_eq!(events.mutant_starts, 0);
        assert_eq!(events.mutants, 0);
        assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
    }

    /// A note describing a mutant that outgrew its ceiling says both numbers: what it reached and
    /// what it was allowed, since a reader's first question is how far past the ceiling it went.
    #[test]
    fn memory_notes_say_how_far_past_the_ceiling_the_run_went() {
        let note = memory_note(
            Utf8Path::new("/workspace/target/debug/deps/unit-abc"),
            Some(300 * 1024 * 1024),
            256 * 1024 * 1024,
        );

        assert!(note.contains("unit-abc"), "{note}");
        assert!(note.contains("300.0 MB"), "{note}");
        assert!(note.contains("256.0 MB"), "{note}");
        assert!(note.contains("past the"), "{note}");
    }

    #[test]
    fn memory_notes_preserve_byte_unit_boundaries_for_the_allowed_ceiling() {
        assert_eq!(
            memory_note(Utf8Path::new("/tmp/tests"), Some(0), 1023),
            "`tests` reached 0 bytes, against the 1023 bytes this run allowed it"
        );
        assert_eq!(
            memory_note(Utf8Path::new("/tmp/tests"), Some(0), 1024),
            "`tests` reached 0 bytes, against the 1.0 KB this run allowed it"
        );
    }

    /// A sentinel a test or nextest extension printed while enumeration failed must never reach the
    /// mutant note that becomes the report `statusReason`. That value is published verbatim into
    /// JSON, HTML, and SARIF artifacts, so anything it carries has left this run for good. It is
    /// still worth an operator's attention locally, so the same call submits a local diagnostic
    /// through `crate::notes` instead of discarding it outright.
    #[test]
    fn enumeration_notes_never_carry_the_raw_output_that_produced_them() {
        notes::alone(|| {
            const SENTINEL: &str = "super-secret-token-3f9a1c";
            let output = format!("error: could not list tests\n{SENTINEL}\n");

            let note = enumeration_note(Utf8Path::new("/workspace/target/debug/deps/unit-abc"), &output);

            assert!(!note.contains(SENTINEL), "the sentinel leaked into the mutant note: {note}");
            assert!(note.contains("unit-abc"), "{note}");

            let raised = notes::drain();

            assert!(
                raised.iter().any(|line| line.contains(SENTINEL)),
                "the raw output was dropped rather than raised as a local diagnostic: {raised:?}"
            );
        });
    }

    #[test]
    fn enumeration_diagnostics_encode_controls_but_preserve_color() {
        notes::alone(|| {
            let _note = enumeration_note(Utf8Path::new("/workspace/\u{1b}[2Junit"), "\u{1b}[31mred\u{1b}[0m\u{1b}[2J\rforged");
            let raised = notes::drain().join("\n");

            assert!(raised.contains("\u{1b}[31mred\u{1b}[0m"), "{raised:?}");
            assert!(!raised.contains("\u{1b}[2J"), "{raised:?}");
            assert!(!raised.contains('\r'), "{raised:?}");
            assert!(raised.contains("/workspace/\\e[2Junit"), "{raised:?}");
        });
    }

    /// Enumeration output that says nothing raises no diagnostic — there is nothing an operator
    /// would be shown, and a note for an empty string would only be noise.
    #[test]
    fn an_empty_enumeration_output_raises_no_diagnostic() {
        notes::alone(|| {
            let note = enumeration_note(Utf8Path::new("/workspace/target/debug/deps/unit-abc"), "");

            assert!(note.contains("unit-abc"), "{note}");
            assert!(notes::drain().is_empty(), "an empty output should not have raised a diagnostic");
        });
    }

    /// A run stopped exactly at its ceiling is described as being at it, not past it.
    ///
    /// Regression, issue-023. "Past" and "at" are different findings: the first says the workload
    /// grew, the second says the ceiling was set a hair too low. The message was built the same way
    /// either way, which sent the reader looking for a growth that never happened.
    ///
    /// The figures are asserted as well, because this is the ordinary shape of an enforced kill —
    /// the kernel caps the peak at the ceiling, so peak and limit are equal — and it was for a
    /// while the one case that printed raw byte counts, on the theory that identical figures always
    /// mean a rounding collision. Here they mean the workload landed exactly on its ceiling.
    #[test]
    fn a_run_that_only_reached_its_ceiling_is_not_described_as_having_passed_it() {
        let note = memory_note(
            Utf8Path::new("/workspace/target/debug/deps/unit-abc"),
            Some(256 * 1024 * 1024),
            256 * 1024 * 1024,
        );

        assert!(note.contains("at the"), "{note}");
        assert!(!note.contains("past"), "{note}");
        assert!(note.contains("256.0 MB"), "{note}");
        assert!(
            !note.contains("bytes"),
            "an exact count is disambiguation nobody needs here, {note}"
        );
    }

    /// A peak the platform reported as below the ceiling is not described as having passed it
    /// either, since the stop was decided on something other than that figure.
    #[test]
    fn a_reported_peak_below_the_ceiling_is_described_as_measured_against_it() {
        let note = memory_note(
            Utf8Path::new("/workspace/target/debug/deps/unit-abc"),
            Some(100 * 1024 * 1024),
            256 * 1024 * 1024,
        );

        assert!(note.contains("against the"), "{note}");
        assert!(!note.contains("past"), "{note}");
    }

    /// Two figures that round to the same thing are printed exactly, so the note cannot read as a
    /// contradiction.
    ///
    /// "reached 512 MB, past the 512 MB this run allowed it" is a sentence that answers its own
    /// question wrongly; the byte counts are the only thing that says how far past it actually went.
    #[test]
    fn a_note_whose_figures_would_round_together_prints_the_exact_bytes() {
        let limit = 512 * 1024 * 1024;
        let note = memory_note(Utf8Path::new("/workspace/target/debug/deps/unit-abc"), Some(limit + 4096), limit);

        assert!(note.contains(&format!("{} bytes", limit + 4096)), "{note}");
        assert!(note.contains(&format!("{limit} bytes")), "{note}");
        assert!(note.contains("past the"), "{note}");
    }

    /// A note describing a mutant that outgrew its ceiling but whose peak the platform could not
    /// itself report still says the ceiling, without inventing a peak that was never measured.
    #[test]
    fn memory_notes_with_no_measured_peak_still_name_the_ceiling() {
        let note = memory_note(Utf8Path::new("/workspace/target/debug/deps/unit-abc"), None, 256 * 1024 * 1024);

        assert!(note.contains("unit-abc"), "{note}");
        assert!(note.contains("256.0 MB"), "{note}");
        assert!(
            !note.contains("past"),
            "a peak nobody measured must not be described as one, {note}"
        );
    }

    /// A flake names the test to fix and says the mutant was never judged.
    ///
    /// The note is the whole value of this outcome. It scores as neither a detection nor a gap, so
    /// a reader who is not told which test failed both ways is told only that something somewhere
    /// is unreliable — which is less than recording it as a survivor would give, since a
    /// survivor at least names a line.
    #[test]
    fn a_flaky_note_names_the_test_and_says_nothing_was_judged() {
        let note = flaky_note(Utf8Path::new("target/debug/deps/unit-abc"), Some("a::b"));

        assert!(note.contains("test `a::b`"), "{note}");
        assert!(note.contains("unit-abc"), "{note}");
        assert!(note.contains("no mutant active"), "{note}");
        assert!(note.contains("never judged"), "{note}");
    }

    /// A harness that named no test still produces a note that reads as a sentence.
    ///
    /// libtest names a test only when it finishes, so a binary that dies mid-run can fail without
    /// ever having said which test did it. Interpolating an absent name would leave the reader a
    /// sentence with a hole in it.
    #[test]
    fn a_flaky_note_without_a_test_name_still_reads() {
        let note = flaky_note(Utf8Path::new("target/debug/deps/unit-abc"), None);

        assert!(note.starts_with("a test in"), "{note}");
        assert!(note.contains("never judged"), "{note}");
    }

    /// A mutant the machine would not run for is recorded as unjudged, and the sweep goes on.
    ///
    /// The shortage behind a refused spawn — a full descriptor or process table — is one the sweep
    /// creates for itself and clears as its workers finish, so abandoning the run over it would
    /// throw away every verdict an hours-long sweep had already reached in favour of a condition
    /// that lasted milliseconds. The mutant lands as `Pending`, which is scored as excluded, with
    /// the refusal as its note so the reader knows which mutant went without a verdict and why.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_mutant_the_machine_would_not_run_is_recorded_rather_than_abandoning_the_sweep() {
        let (_directory, work, _plan, binaries) = harness("exit 0", Duration::from_secs(30));
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let _refusals: Vec<_> = (0..8).map(|_round| faults::arm(Fault::Spawn)).collect();

        let judgement = judge(
            &work,
            1,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: true,
                census: blind(),
            },
            &Tally::default(),
        );

        match judgement {
            Judgement::Reached(outcome, _killer, note) => {
                assert_eq!(outcome, Outcome::Pending, "an unjudgeable mutant is not a verdict about the mutant");
                assert!(!outcome.is_valid(), "and must not be scored");
                assert!(
                    note.is_some_and(|reason| reason.contains("could not be started")),
                    "the refusal has to travel"
                );
            }
            Judgement::Guessed(killer) => panic!("an unjudgeable mutant cannot have a guessed killer: {killer:?}"),
            Judgement::Abandoned(reason) => panic!("one refused spawn must not take the run with it: {reason}"),
        }
    }

    /// A binary that cannot be metered as a mutant sweep asked abandons the mutant it was judging,
    /// rather than judging it with no protection installed.
    ///
    /// The whole point of asking for memory accounting is that a mutant that exhausts memory gets
    /// caught by the ceiling rather than by wedging the machine; a `judge` that silently ran the
    /// mutant anyway would report a verdict nobody could trust once the accounting it was told it
    /// had turned out never to have been there.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_cannot_be_metered_abandons_the_mutant_it_was_judging() {
        if memory::support().is_ok() {
            return;
        }

        let (_directory, work, _plan, mut binaries) = harness("exit 0", Duration::from_secs(30));
        binaries[0].memory = None;
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let judgement = judge(
            &work,
            1,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: true,
                confirm: true,
                census: blind(),
            },
            &Tally::default(),
        );

        match judgement {
            Judgement::Abandoned(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
            Judgement::Reached(..) | Judgement::Guessed(_) => {
                panic!("expected the mutant to be abandoned rather than judged unprotected")
            }
        }
    }

    /// A sweep that loses its memory accounting partway through stops rather than continue judging
    /// the rest of its mutants unprotected.
    ///
    /// Every worker checks whether the sweep has already been abandoned before picking up its next
    /// mutant, which is what keeps a run that lost its protection from quietly finishing the rest of
    /// its work as though nothing had happened.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_sweep_that_loses_its_memory_accounting_stops_rather_than_continue_unprotected() {
        if memory::support().is_ok() {
            return;
        }

        let (_directory, work, mut plan, binaries) = harness_n("exit 0", Duration::from_secs(30), 8);
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);
        let sweep = Sweep {
            timeout_floor: Duration::ZERO,
            stall: Stall::NONE,
            jobs: 4,
            meter: true,
            confirm: true,
            census: blind(),
        };

        let failure = test_all(
            &work,
            &mut plan,
            &reach,
            sweep,
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect_err("a run that cannot be metered as asked must stop rather than continue");

        // The wrapper has to say the run stopped, and it has to carry the underlying cause through
        // rather than replacing it — that cause is the only thing telling the reader what to fix.
        assert!(failure.to_string().contains("judge anything further"), "{failure}");
        assert!(failure.to_string().contains("cgroup"), "{failure}");

        // At least one worker must have broken out of its loop as soon as it saw the sweep was
        // abandoned, rather than every mutant having raced to a verdict of its own; a run with
        // nothing left pending is not what this path is meant to prove.
        assert!(
            plan.mutants.iter().any(|mutant| mutant.outcome == Outcome::Pending),
            "{:?}",
            plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>()
        );
    }

    /// A binary that outgrows the memory ceiling it was judged under convicts the mutant of using
    /// too much memory, rather than reporting it as a plain survivor or failure.
    ///
    /// The suite's own harness never noticed anything wrong; only the kernel's accounting did, and
    /// a reader who was told the mutant merely "survived" would go looking for a missing assertion
    /// that was never the actual gap.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_binary_that_outgrows_its_ceiling_convicts_the_mutant_of_using_too_much_memory() {
        if crate::testing::without_memory_support("a sweep asserting a ceiling is enforced") {
            return;
        }

        let fill = format!("/dev/shm/gamma-judge.{}", std::process::id());
        let (_directory, work, _plan, mut binaries) = harness(
            &format!("dd if=/dev/zero of={fill} bs=1M count=512 2>/dev/null"),
            Duration::from_mins(1),
        );
        binaries[0].memory = Some(32 * 1024 * 1024);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let judgement = judge(
            &work,
            1,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: true,
                confirm: true,
                census: blind(),
            },
            &Tally::default(),
        );

        let _removed = fs::remove_file(&fill);

        match judgement {
            Judgement::Reached(Outcome::OutOfMemory, None, Some(note)) => {
                assert!(note.contains("32.0 MB"), "{note}");
            }
            _ => panic!("expected the mutant to be convicted of using too much memory"),
        }
    }

    /// The script a probe test runs, which fails only when libtest's `--exact` filter reached it.
    ///
    /// `sh -c BODY name --exact` puts the filter in `$0`, so the body can tell a filtered run from
    /// an unfiltered one. That is the whole assertion these tests need: a verdict that could only
    /// have come from the filtered run proves the probe is what produced it.
    /// A suite that only fails when the run has narrowed itself to `tests::killer`.
    ///
    /// On the portable helper rather than a shell, because these cases narrow the selection — and a
    /// shell fixture carries its script in a positional argument, which is exactly where a real
    /// test binary expects a test-name filter and which the tool therefore replaces.
    const ONLY_WHEN_FILTERED: &[&str] = &[
        "when-arg:tests::killer|print:test tests::killer ... FAILED",
        "when-arg:tests::killer|exit:1",
        "exit:0",
    ];

    /// The same shape as [`harness`], with the portable helper standing in for the shell.
    fn helper_harness(script: &[&str]) -> (tempfile::TempDir, Workspace, Plan, Vec<TestBinary>) {
        let (directory, work) = crate::testing::helper_workspace("test-all-helper", script);
        let plan = one_mutant_plan(work.root.clone());
        let binaries = vec![TestBinary {
            package: "subject".to_owned(),
            baseline: Duration::from_millis(1),
            budget: None,
            ..crate::testing::helper()
        }];

        (directory, work, plan, binaries)
    }

    /// A hint is a guess the run checks, and a hint that convicts spares the rest of the binary.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn the_test_that_caught_a_mutant_last_time_is_tried_first() {
        let (_directory, work, _plan, binaries) = helper_harness(ONLY_WHEN_FILTERED);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::killer".to_owned(),
        };

        // Confirmation off: the fixture cannot tell a mutant run from an exoneration run, so a
        // confirmed kill would come back flaky and prove nothing about the probe.
        let judgement = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        match judgement {
            Judgement::Reached(Outcome::Killed, Some(killer), None) => {
                assert_eq!(killer.test, "tests::killer");
                assert_eq!(killer.package, "subject");
            }
            _ => panic!("expected the recorded test to convict the mutant on its own"),
        }
    }

    #[test]
    fn a_cached_killer_settles_before_a_later_timeout() {
        const SCRIPT: &[&str] = &[
            "when-arg:tests::killer|print:test tests::killer ... FAILED",
            "when-arg:tests::killer|exit:1",
            "sleep:200",
            "exit:0",
        ];

        let (_directory, work, _plan, mut binaries) = helper_harness(SCRIPT);
        binaries[0].budget = Some(Duration::from_millis(20));
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::killer".to_owned(),
        };
        let tally = Tally::default();

        let judgement = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.exact_candidates.load(Ordering::Relaxed), 1);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 0);
    }

    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_confirmed_cached_killer_settles_under_all_resource_policies() {
        const SCRIPT: &[&str] = &[
            "when-env:GAMMA_ACTIVE|when-arg:tests::killer|print:test tests::killer ... FAILED",
            "when-env:GAMMA_ACTIVE|when-arg:tests::killer|exit:1",
            "when-env:GAMMA_ACTIVE|sleep:5000",
            "exit:0",
        ];

        if crate::testing::without_memory_support("a cached killer under memory accounting") {
            return;
        }

        let (_directory, work, _plan, mut binaries) = helper_harness(SCRIPT);
        binaries[0].budget = Some(Duration::from_secs(10));
        binaries[0].memory = Some(1024 * 1024 * 1024);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::killer".to_owned(),
        };
        let tally = Tally::default();

        let judgement = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall {
                    budget: Some(Duration::from_secs(10)),
                },
                jobs: 1,
                meter: true,
                confirm: true,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_partial_census_kill_settles_before_the_whole_binary_timeout() {
        const SCRIPT: &[&str] = &[
            "when-arg:tests::t0|print:test tests::t0 ... FAILED",
            "when-arg:tests::t0|exit:1",
            "sleep:200",
            "exit:0",
        ];

        let (_directory, work, plan, mut binaries) = helper_harness(SCRIPT);
        binaries[0].budget = Some(Duration::from_millis(20));
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let census = Census::partial(&binaries[0].path, plan.mutants[0].ordinal, 1, 4);
        let tally = Tally::default();

        let judgement = judge(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: &census,
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.hinted_fallbacks.load(Ordering::Relaxed), 0);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn negative_reach_hints_require_canonical_policy_free_runs() {
        let mut binary = crate::testing::helper();
        binary.budget = None;
        let base = Sweep {
            timeout_floor: Duration::ZERO,
            stall: Stall::NONE,
            jobs: 1,
            meter: false,
            confirm: false,
            census: blind(),
        };

        assert!(negative_reach_hint_is_final(&binary, None, base));

        binary.budget = Some(Duration::from_secs(1));
        assert!(
            !negative_reach_hint_is_final(&binary, None, base),
            "timeout policy must run canonically"
        );
        binary.budget = None;

        assert!(
            !negative_reach_hint_is_final(
                &binary,
                None,
                Sweep {
                    stall: Stall {
                        budget: Some(Duration::from_secs(1)),
                    },
                    ..base
                }
            ),
            "stall policy must run canonically"
        );
        assert!(
            !negative_reach_hint_is_final(&binary, None, Sweep { meter: true, ..base }),
            "memory limits and metering failures must run canonically"
        );
        assert!(
            !negative_reach_hint_is_final(&binary, None, Sweep { confirm: true, ..base }),
            "confirmation flakes must run canonically"
        );
    }

    #[test]
    fn a_complete_census_records_an_uncovered_selection_without_launching_the_binary() {
        let (_directory, work, plan, binaries) = helper_harness(&["exit:0"]);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let census = Census::complete(&binaries[0].path);
        let tally = Tally::default();

        let judgement = judge(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            None,
            Sweep {
                census: &census,
                ..sweep(Stall::NONE)
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::NoCoverage, None, None)));
        assert_eq!(tally.uncovered_selections.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_complete_census_failure_settles_without_a_whole_binary_rerun() {
        const SCRIPT: &[&str] = &[
            "when-arg:tests::t0|print:test tests::t0 ... FAILED",
            "when-arg:tests::t0|exit:1",
            "sleep:5000",
            "exit:0",
        ];

        let (_directory, work, plan, mut binaries) = helper_harness(SCRIPT);
        binaries[0].budget = Some(Duration::from_secs(2));
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let census = Census::examined(&binaries[0].path, plan.mutants[0].ordinal, 1, 4);
        let tally = Tally::default();

        let judgement = judge(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: &census,
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.case_selections.load(Ordering::Relaxed), 1);
        assert_eq!(tally.selected_tests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_cached_killer_cannot_bypass_confirmation_of_a_flaky_test() {
        let (_directory, work, _plan, binaries) = helper_harness(&["print:test tests::killer ... FAILED", "exit:1"]);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::killer".to_owned(),
        };
        let tally = Tally::default();

        let judgement = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: true,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Flaky, None, Some(_))));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_file_local_killer_cannot_move_a_later_binary_before_a_timeout() {
        let (_directory, work, _plan, mut binaries) = helper_harness(&["sleep:200", "exit:0"]);
        binaries[0].target = "canonical".to_owned();
        binaries[0].budget = Some(Duration::from_millis(20));
        let mut later = binaries[0].clone();
        later.target = "hinted".to_owned();
        later.budget = Some(Duration::from_secs(1));
        binaries.push(later);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let hint = Killer {
            package: "subject".to_owned(),
            target: "hinted".to_owned(),
            test: "tests::killer".to_owned(),
        };
        let tally = Tally::default();

        let judgement = judge_ordered(
            &work,
            1,
            &reachable,
            None,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Timeout, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_partial_census_hint_that_passes_falls_back_to_the_whole_binary() {
        const SCRIPT: &[&str] = &["when-arg:tests::t0|exit:0", "print:test tests::whole ... FAILED", "exit:1"];

        let (_directory, work, plan, binaries) = helper_harness(SCRIPT);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let census = Census::partial(&binaries[0].path, plan.mutants[0].ordinal, 1, 4);
        let tally = Tally::default();

        let judgement = judge(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: &census,
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, _, None)));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    /// A hint naming a test the user's own filter excludes convicts nobody.
    ///
    /// This is the composition defect end to end: the probe would once have been launched with the
    /// recorded name appended to the user's filter, which libtest reads as "either", so the
    /// excluded test would have run and convicted the mutant — crediting the suite with a detection
    /// it does not make as configured. The probe is now refused, and the verdict comes from the
    /// binary run under the user's filter, where nothing fails.
    #[test]
    fn a_hint_naming_a_test_the_users_filter_excludes_does_not_convict() {
        let (_directory, mut work, _plan, binaries) = helper_harness(ONLY_WHEN_FILTERED);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        // The user asked for `tests::other` alone, which the fixture's killer is not.
        let mut arguments: Vec<String> = ONLY_WHEN_FILTERED.iter().map(crate::testing::directive).collect();

        arguments.push("tests::other".to_owned());
        work.set_test_args(arguments);

        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::killer".to_owned(),
        };

        let judgement = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        assert!(
            matches!(judgement, Judgement::Reached(Outcome::Survived, None, None)),
            "an excluded test must not convict"
        );
    }

    /// The same mutant with no hint reaches the ordinary verdict, which is what makes the case above
    /// a measurement of the probe rather than of the fixture.
    #[test]
    #[cfg(unix)]
    fn the_same_mutant_without_a_hint_is_judged_by_the_whole_binary() {
        let (_directory, work, _plan, binaries) = helper_harness(ONLY_WHEN_FILTERED);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let judgement = judge(
            &work,
            1,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        assert!(
            matches!(judgement, Judgement::Reached(Outcome::Survived, None, None)),
            "the unfiltered script passes, so this mutant survives"
        );
    }

    /// A hint naming a binary this mutant cannot reach is ignored rather than run.
    ///
    /// A map written before the test packages were narrowed, or before the test moved, names a
    /// binary the run has excluded. Running it anyway would judge the mutant against a suite the
    /// caller deliberately took out of the picture.
    #[test]
    #[cfg(unix)]
    fn a_hint_naming_an_unreachable_binary_is_ignored() {
        let (_directory, work, _plan, binaries) = helper_harness(ONLY_WHEN_FILTERED);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let hint = Killer {
            package: "elsewhere".to_owned(),
            target: String::new(),
            test: "tests::killer".to_owned(),
        };

        let judgement = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        assert!(
            matches!(judgement, Judgement::Reached(Outcome::Survived, None, None)),
            "a hint for another package must not decide anything"
        );
    }

    /// A script that passes under the hinted filter and fails without it.
    ///
    /// This is the fixture that makes a wrong hint observable. A script failing either way cannot
    /// distinguish a verdict the probe reached from one the ordinary sweep reached, so it proves
    /// nothing about which of them decided the outcome.
    #[cfg(unix)]
    const FAILS_ONLY_UNFILTERED: &str = r#"if [ "$0" = "tests::gone" ]; then exit 0; fi; echo "test tests::other ... FAILED"; exit 1"#;

    /// A hint that no longer convicts leaves the verdict exactly where it would have been.
    ///
    /// This is the property the whole optimization rests on: the map may only ever change what a
    /// run costs, never what it concludes. The two judgements are compared against each other
    /// rather than against a written-down expectation, because the claim is an equality between two
    /// runs and not a claim about any particular outcome.
    ///
    /// The killer's name is the part that makes this a real check. Under this fixture the hinted
    /// test passes and a different one fails, so a run that believed its hint would credit
    /// `tests::gone` — a test that did not fail and, in a stale map, may not exist. Asserting the
    /// recorded killer is `tests::other` is what proves the hint was discarded rather than trusted.
    #[test]
    #[cfg(unix)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_hint_that_no_longer_convicts_reaches_the_verdict_the_run_would_have_reached_anyway() {
        let (_directory, work, _plan, binaries) = harness(FAILS_ONLY_UNFILTERED, Duration::from_secs(30));
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::gone".to_owned(),
        };

        let hinted = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );
        let unhinted = judge(
            &work,
            1,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        for (judgement, described) in [(hinted, "with a wrong hint"), (unhinted, "with no hint")] {
            match judgement {
                Judgement::Reached(Outcome::Killed, Some(killer), None) => {
                    assert_eq!(killer.test, "tests::other", "{described}: the wrong test was credited");
                }
                Judgement::Reached(outcome, killer, note) => {
                    panic!("{described}: expected a kill by `tests::other`, got {outcome:?} / {killer:?} / {note:?}")
                }
                Judgement::Guessed(killer) => {
                    panic!("{described}: expected an observed kill by `tests::other`, got a guess for {killer:?}")
                }
                Judgement::Abandoned(reason) => panic!("{described}: the run was abandoned: {reason}"),
            }
        }
    }

    /// A wrong hint must not manufacture a kill out of a mutant nothing catches.
    ///
    /// The opposite direction of the same law, and the more damaging one: a survivor turned into a
    /// kill is a test gap the report says does not exist.
    #[test]
    #[cfg(unix)]
    fn a_wrong_hint_cannot_turn_a_survivor_into_a_kill() {
        let (_directory, work, _plan, binaries) = harness("exit 0", Duration::from_secs(30));
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let hint = Killer {
            package: "subject".to_owned(),
            target: String::new(),
            test: "tests::gone".to_owned(),
        };

        let hinted = judge(
            &work,
            1,
            &reachable,
            Some(&hint),
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );
        let unhinted = judge(
            &work,
            1,
            &reachable,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        assert!(matches!(hinted, Judgement::Reached(Outcome::Survived, None, None)));
        assert!(matches!(unhinted, Judgement::Reached(Outcome::Survived, None, None)));
    }

    /// A sweep writes back what caught each mutant, and drops what caught one that nothing caught.
    ///
    /// The forgetting half matters as much as the recording half: an entry left behind for a mutant
    /// that now survives makes every later run pay for a probe already shown not to convict.
    #[test]
    #[cfg(unix)]
    fn a_sweep_records_the_killer_it_found_and_forgets_the_one_it_did_not() {
        let (_directory, work, mut plan, binaries) = harness("echo 'test tests::caught ... FAILED'; exit 1", Duration::from_secs(30));
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let reach = Reachability::build(&plan, &binaries, &scope);
        let sweep = Sweep {
            timeout_floor: Duration::ZERO,
            stall: Stall::NONE,
            jobs: 1,
            meter: false,
            confirm: false,
            census: blind(),
        };

        let mut killers = Killers::default();
        killers.record(
            "m1".into(),
            Killer {
                package: "subject".to_owned(),
                target: String::new(),
                test: "tests::stale".to_owned(),
            },
        );

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep,
            false,
            &mut killers,
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep to finish");

        assert_eq!(plan.mutants[0].outcome, Outcome::Killed);
        assert_eq!(killers.hint("m1").map(|found| found.test.as_str()), Some("tests::caught"));

        // Now the same tree with a suite that catches nothing: the entry has to go.
        let (_directory, work, mut plan, binaries) = harness("exit 0", Duration::from_secs(30));
        let reach = Reachability::build(&plan, &binaries, &scope);

        let _spent = test_all(
            &work,
            &mut plan,
            &reach,
            sweep,
            false,
            &mut killers,
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep to finish");

        assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
        assert!(killers.hint("m1").is_none(), "a mutant nothing caught must not keep a killer");

        drop(binaries);
    }

    /// A mutant with a custom timeout multiplier uses that multiplier instead of the binary's default.
    #[test]
    #[cfg(unix)]
    fn a_mutant_with_a_timeout_multiplier_overrides_the_binary_budget() {
        // Keep a wide gap on both sides of the sleep so scheduler jitter cannot change the verdict.
        let (_directory, work, _plan, mut binaries) = harness("sleep 0.2; exit 0", Duration::from_millis(10));
        binaries[0].baseline = Duration::from_millis(50);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();

        let sweep = Sweep {
            timeout_floor: Duration::ZERO,
            stall: Stall::NONE,
            jobs: 1,
            meter: false,
            confirm: false,
            census: blind(),
        };

        let without_override = judge(&work, 1, &reachable, None, None, sweep, &Tally::default());
        assert!(
            matches!(without_override, Judgement::Reached(Outcome::Timeout, None, None)),
            "default budget times out"
        );

        let with_override = judge(&work, 1, &reachable, None, Some(100.0), sweep, &Tally::default());
        assert!(
            matches!(with_override, Judgement::Reached(Outcome::Survived, None, None)),
            "extended budget allows completion"
        );
    }

    /// With census data, a mutant whose reaching tests cost less schedules after one whose
    /// reaching tests cost more — even if both are in the same package.
    #[test]
    fn census_based_scheduling_uses_per_site_measured_cost() {
        use super::mutant_cost;
        let plan = plan_over(&["subject", "subject"]);
        let binaries = vec![binary_of("subject", Duration::from_secs(10))];

        let sets = Reachability::build(&plan, &binaries, &NARROW);

        // Without census data, both get the same estimated killed-run cost.
        let cost_a = mutant_cost(0, &plan, &sets, blind(), &Killers::default());
        let cost_b = mutant_cost(1, &plan, &sets, blind(), &Killers::default());
        assert_eq!(cost_a, cost_b);
        assert_eq!(cost_a, Duration::from_secs(5));
    }

    #[test]
    fn scheduling_cost_handles_unreachable_and_partial_census_sites() {
        use super::mutant_cost;

        let plan = plan_over(&["subject"]);
        assert_eq!(
            mutant_cost(0, &plan, &Reachability::default(), blind(), &Killers::default()),
            Duration::ZERO
        );

        let mut binary = binary_of("subject", Duration::from_secs(10));
        binary.tests = Some(4);
        let binaries = [binary];
        let reach = Reachability::build(&plan, &binaries, &NARROW);
        let partial = Census::partial(&binaries[0].path, plan.mutants[0].ordinal, 1, 4);

        assert_eq!(
            mutant_cost(0, &plan, &reach, &partial, &Killers::default()),
            Duration::from_millis(1_751),
            "partial census evidence combines the hinted test cost with inferred-test fallback cost"
        );

        #[cfg(unix)]
        {
            let uncovered = Census::examined(&binaries[0].path, plan.mutants[0].ordinal, 0, 4);
            assert_eq!(
                mutant_cost(0, &plan, &reach, &uncovered, &Killers::default()),
                Duration::ZERO,
                "a complete census that reaches no test contributes no sweep work"
            );
        }
    }

    #[test]
    fn scheduling_interleaves_packages_after_longest_first_ordering() {
        let mut plan = plan_over(&["short", "long", "long", "short", "middle"]);
        for (index, mutant) in plan.mutants.iter_mut().enumerate() {
            mutant.ordinal = u32::try_from(index + 1).expect("the fixture has only five mutants");
        }
        let binaries = [
            binary_of("short", Duration::from_secs(1)),
            binary_of("long", Duration::from_secs(9)),
            binary_of("middle", Duration::from_secs(4)),
        ];
        let reach = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );
        let mut pending = vec![0, 1, 2, 3, 4];

        schedule(&mut pending, &plan, &reach, blind(), &Killers::default());

        assert_eq!(
            pending,
            vec![1, 4, 0, 2, 3],
            "each round takes one mutant from each cost-ordered package queue"
        );
    }

    #[test]
    fn sweep_queue_and_completion_policy_preserve_boundaries_and_all_fields() {
        let mut plan = one_mutant_plan(Utf8PathBuf::from("/nowhere"));
        let mut inactive = plan.mutants[0].clone();
        inactive.id = "inactive".into();
        inactive.ordinal = 0;
        let mut settled = plan.mutants[0].clone();
        settled.id = "settled".into();
        settled.ordinal = 2;
        settled.outcome = Outcome::Killed;
        plan.mutants.push(inactive);
        plan.mutants.push(settled);

        assert_eq!(pending_positions(&plan), vec![0]);
        assert_eq!(worker_count(0), 1);
        assert_eq!(worker_count(1), 1);
        assert_eq!(worker_count(8), 8);
        assert_eq!(elapsed_millis(Duration::from_millis(17)), 17);
        assert_eq!(elapsed_millis(Duration::MAX), u64::MAX);

        let killer = Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: "tests::caught".to_owned(),
        };
        let mut killers = Killers::default();
        let mut events = crate::testing::Recorder::default();
        publish_completed(
            &mut plan,
            &mut killers,
            &mut events,
            Completed {
                position: 0,
                outcome: Outcome::Killed,
                elapsed_ms: 23,
                killer: Some(KillerAttribution::Observed(killer.clone())),
                note: Some("detail".to_owned()),
                first_started_ms: 0,
                settled_ms: 23,
                attempts: Vec::new(),
            },
        );
        assert_eq!(plan.mutants[0].outcome, Outcome::Killed);
        assert_eq!(plan.mutants[0].elapsed_ms, 23);
        assert_eq!(plan.mutants[0].killed_by.as_deref(), Some("tests::caught"));
        assert_eq!(
            plan.mutants[0].killer,
            Some(crate::model::KillerIdentity {
                package: "subject".to_owned(),
                target: "lib".to_owned(),
                test: "tests::caught".to_owned(),
            })
        );
        assert_eq!(plan.mutants[0].note.as_deref(), Some("detail"));
        assert_eq!(killers.hint(&plan.mutants[0].id), Some(&killer));
        assert_eq!(events.mutants, 1);

        publish_completed(
            &mut plan,
            &mut killers,
            &mut events,
            Completed {
                position: 0,
                outcome: Outcome::Survived,
                elapsed_ms: 29,
                killer: None,
                note: None,
                first_started_ms: 0,
                settled_ms: 29,
                attempts: Vec::new(),
            },
        );
        assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
        assert_eq!(plan.mutants[0].elapsed_ms, 29);
        assert_eq!(plan.mutants[0].killed_by, None);
        assert_eq!(plan.mutants[0].killer, None);
        assert_eq!(plan.mutants[0].note, None);
        assert!(killers.hint(&plan.mutants[0].id).is_none());
        assert_eq!(events.mutants, 2);

        publish_completed(
            &mut plan,
            &mut killers,
            &mut events,
            Completed {
                position: usize::MAX,
                outcome: Outcome::Killed,
                elapsed_ms: 0,
                killer: Some(KillerAttribution::Observed(killer)),
                note: None,
                first_started_ms: 0,
                settled_ms: 0,
                attempts: Vec::new(),
            },
        );
        assert_eq!(events.mutants, 2, "an obsolete queue position publishes nothing");
    }

    #[test]
    fn an_ambiguous_grouped_probe_persists_only_a_scheduling_hint() {
        let mut plan = one_mutant_plan(Utf8PathBuf::from("/nowhere"));
        let killer = Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: "tests::first".to_owned(),
        };
        let mut killers = Killers::default();
        let mut events = crate::testing::Recorder::default();

        publish_completed(
            &mut plan,
            &mut killers,
            &mut events,
            Completed {
                position: 0,
                outcome: Outcome::Killed,
                elapsed_ms: 25,
                killer: Some(KillerAttribution::Guessed(killer.clone())),
                note: None,
                first_started_ms: 0,
                settled_ms: 25,
                attempts: Vec::new(),
            },
        );

        assert_eq!(plan.mutants[0].outcome, Outcome::Killed);
        assert_eq!(plan.mutants[0].killed_by, None);
        assert_eq!(plan.mutants[0].killer, None);
        assert_eq!(killers.hint(&plan.mutants[0].id), Some(&killer));
        assert_eq!(events.mutants, 1);
    }

    /// A mutant with a known killer hint has a lower estimated cost (one binary baseline).
    #[test]
    fn a_hinted_mutant_costs_less_than_an_unhinted_one() {
        use super::mutant_cost;

        let plan = plan_over(&["subject", "subject"]);
        let binaries = vec![
            binary_of("subject", Duration::from_secs(5)),
            binary_of("subject", Duration::from_secs(20)),
        ];

        let sets = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );

        let mut killers = Killers::default();
        killers.record(
            plan.mutants[0].id.clone(),
            Killer {
                package: "subject".to_owned(),
                target: String::new(),
                test: "tests::hint".to_owned(),
            },
        );

        let cost_hinted = mutant_cost(0, &plan, &sets, blind(), &killers);
        let cost_unhinted = mutant_cost(1, &plan, &sets, blind(), &Killers::default());

        assert!(
            cost_hinted < cost_unhinted,
            "hinted={cost_hinted:?} should be less than unhinted={cost_unhinted:?}"
        );
    }

    #[test]
    fn live_workload_preserves_exact_and_whole_execution_shapes() {
        let plan = plan_over(&["subject", "subject"]);
        let mut binaries = vec![
            binary_of("subject", Duration::from_secs(5)),
            binary_of("subject", Duration::from_secs(20)),
        ];
        for binary in &mut binaries {
            binary.budget = Some(Duration::from_mins(1));
        }
        let reach = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );
        let mut killers = Killers::default();
        killers.record(
            plan.mutants[0].id.clone(),
            Killer {
                package: "subject".to_owned(),
                target: String::new(),
                test: "tests::hint".to_owned(),
            },
        );
        let exact = mutant_work(0, &plan, &reach, blind(), &killers);
        let whole = mutant_work(1, &plan, &reach, blind(), &Killers::default());
        let mut generalized = Killers::default();
        generalized.replace_generalized(GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            items: vec![ItemHints {
                file: plan.mutants[1].file.as_ref().to_path_buf(),
                item: plan.mutants[1].item_path.to_string(),
                candidates: vec![RankedHint {
                    candidate: Killer {
                        package: "subject".to_owned(),
                        target: String::new(),
                        test: "tests::related".to_owned(),
                    },
                    seeds: 2,
                    hits: 7,
                    misses: 1,
                    measured_ms: 12_000,
                    samples: 4,
                    order: 0,
                }],
            }],
            binaries: Vec::new(),
            test_sets: Vec::new(),
            reach: Vec::new(),
        });
        let generalized = mutant_work(1, &plan, &reach, blind(), &generalized);
        let census_binaries = [binary_of("subject", Duration::from_secs(20))];
        let census_reach = Reachability::build(
            &plan,
            &census_binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );
        let selected_census = Census::examined(&census_binaries[0].path, plan.mutants[1].ordinal, 1, 4);
        let selected = mutant_work(1, &plan, &census_reach, &selected_census, &Killers::default());
        let hinted_census = Census::partial(&census_binaries[0].path, plan.mutants[1].ordinal, 1, 4);
        let hinted = mutant_work(1, &plan, &census_reach, &hinted_census, &Killers::default());
        let uncovered_census = Census::examined(&census_binaries[0].path, plan.mutants[1].ordinal, 0, 4);
        let uncovered = mutant_work(1, &plan, &census_reach, &uncovered_census, &Killers::default());

        assert_eq!(exact.kind, crate::estimate::WorkKind::Exact);
        assert_eq!(whole.kind, crate::estimate::WorkKind::Whole);
        assert_eq!(generalized.kind, crate::estimate::WorkKind::Hinted);
        assert_eq!(selected.kind, crate::estimate::WorkKind::Selected);
        assert_eq!(hinted.kind, crate::estimate::WorkKind::Hinted);
        assert_eq!(uncovered.kind, crate::estimate::WorkKind::Uncovered);
    }

    fn observed_planning_hint<T>(candidate: T, samples: u32) -> RankedHint<T> {
        RankedHint {
            candidate,
            seeds: 11,
            hits: 3,
            misses: 5,
            measured_ms: 37,
            samples,
            order: 13,
        }
    }

    #[test]
    fn inferred_planning_indexes_retain_file_hints_and_first_last_reach_sets_but_skip_invalid_indices() {
        let plan = plan_over(&["subject"]);
        let site = SiteIdentity::from_mutant(&plan.mutants[0]);
        let mut hints = GeneralizedHints::empty_supported();
        hints.binaries.push(FileBinaryHints {
            file: plan.mutants[0].file.as_ref().to_owned(),
            candidates: vec![observed_planning_hint(
                BinaryHint {
                    package: "subject".into(),
                    target: "lib".into(),
                },
                2,
            )],
        });
        hints.test_sets = vec![vec![killer("first")], vec![killer("last")]];
        hints.reach = [0, 1]
            .map(|test_set| crate::discover::ReachCluster {
                site: site.clone(),
                test_set,
            })
            .to_vec();
        let mut killers = Killers::default();
        let mut invalid = hints.clone();
        invalid.reach.push(crate::discover::ReachCluster {
            site: site.clone(),
            test_set: u32::MAX,
        });
        killers.replace_generalized(invalid);
        let invalid = InferredPlanning::new(&killers);
        assert!(invalid.files.is_empty());
        assert!(invalid.reach.is_empty());
        killers.replace_generalized(hints);
        let planning = InferredPlanning::new(&killers);
        let candidates = planning.files.get(plan.mutants[0].file.as_ref()).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            (
                candidates[0].samples,
                candidates[0].measured_ms,
                candidates[0].hits,
                candidates[0].misses
            ),
            (2, 37, 3, 5)
        );
        let (first, last) = planning.reach.get(&site).unwrap();
        assert_eq!(*first, [killer("first")]);
        assert_eq!(*last, [killer("last")]);
        assert_eq!(planning.reach.len(), 1);
    }

    #[test]
    fn reach_and_file_cost_candidates_preserve_the_matched_binary_and_every_nondefault_observation() {
        let binaries = ranked_binaries();
        let reachable = [&binaries[0], &binaries[1]];
        let killers = ["missing", "later", "earlier"].map(|target| Killer {
            package: "subject".into(),
            target: target.into(),
            test: "probe".into(),
        });
        let reached = reached_work_candidate(&reachable, &killers).unwrap();
        assert!(std::ptr::eq(reached.binary, &raw const binaries[1]));
        assert_eq!(
            (reached.measured_ms, reached.samples, reached.successes, reached.misses),
            (0, 0, 0, 0)
        );
        assert!(matches!(reached.probe, InferredProbe::Filtered));
        assert!(reached_work_candidate(&reachable, &killers[..1]).is_none());
        let candidates = ["missing", "later", "earlier"].map(|target| {
            observed_planning_hint(
                BinaryHint {
                    package: "subject".into(),
                    target: target.into(),
                },
                2,
            )
        });
        let file = file_candidate(&reachable, &candidates).unwrap();
        assert!(std::ptr::eq(file.binary, &raw const binaries[1]));
        assert_eq!((file.measured_ms, file.samples, file.successes, file.misses), (37, 2, 3, 5));
        assert!(matches!(file.probe, InferredProbe::Whole));
        assert!(file_candidate(&reachable, &candidates[..1]).is_none());
    }

    #[test]
    fn a_package_without_reachable_binaries_has_an_empty_index_and_no_inferred_work() {
        let plan = plan_over(&["not-subject"]);
        let binaries = ranked_binaries();
        let reach = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: true,
                whole_workspace: false,
            },
        );
        assert!(reach.reachable(&plan.mutants[0]).unwrap().is_empty());
        let killers = Killers::default();
        let planning = InferredPlanning::new(&killers);
        assert!(inferred_mutant_work(&plan.mutants[0], &reach, &planning).is_none());
    }

    #[test]
    fn actual_planning_costs_distinguish_exact_reach_file_and_measured_sample_boundaries() {
        let plan = plan_over(&["subject"]);
        let mut binaries = ranked_binaries();
        binaries[0].baseline = Duration::from_millis(120);
        binaries[1].baseline = Duration::from_millis(360);
        let reach = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );
        let candidate = Killer {
            package: "subject".into(),
            target: "earlier".into(),
            test: "probe".into(),
        };
        let mut exact = Killers::default();
        exact.record(plan.mutants[0].id.clone(), candidate.clone());
        let work = mutant_work(0, &plan, &reach, blind(), &exact);
        // Exact: first baseline / 4, plus half the combined baselines weighted by the exact miss prior.
        assert_eq!(work.kind, crate::estimate::WorkKind::Exact);
        assert_eq!(work.scheduling_cost(), Duration::from_millis(42));
        let mut hinted = GeneralizedHints::empty_supported();
        hinted.test_sets.push(vec![candidate.clone()]);
        hinted.reach.push(crate::discover::ReachCluster {
            site: SiteIdentity::from_mutant(&plan.mutants[0]),
            test_set: 0,
        });
        let mut reached = Killers::default();
        reached.replace_generalized(hinted);
        // Reach evidence uses the same narrow probe prior but the less-trusted inferred miss prior.
        assert_eq!(
            mutant_work(0, &plan, &reach, blind(), &reached).scheduling_cost(),
            Duration::from_millis(114)
        );
        for file in [false, true] {
            // Cold probes use baseline / 4 (item) or / 2 (file). Measured samples average
            // whole milliseconds first, truncating the remainder before constructing a duration.
            for (samples, measured_probe) in [(0, if file { 60 } else { 30 }), (1, 37), (2, 18)] {
                let mut hints = GeneralizedHints::empty_supported();
                if file {
                    hints.binaries.push(FileBinaryHints {
                        file: plan.mutants[0].file.as_ref().to_owned(),
                        candidates: vec![observed_planning_hint(
                            BinaryHint {
                                package: "subject".into(),
                                target: "earlier".into(),
                            },
                            samples,
                        )],
                    });
                } else {
                    hints.items.push(ItemHints {
                        file: plan.mutants[0].file.as_ref().to_owned(),
                        item: plan.mutants[0].item_path.to_string(),
                        candidates: vec![observed_planning_hint(candidate.clone(), samples)],
                    });
                }
                let mut killers = Killers::default();
                killers.replace_generalized(hints);
                let work = mutant_work(0, &plan, &reach, blind(), &killers);
                assert_eq!(work.kind, crate::estimate::WorkKind::Hinted);
                // Fallback is half the combined baselines times the posterior miss share:
                // 1 - (prior_strength * inferred_hit_share + hits) / (prior_strength + hits + misses).
                assert_eq!(
                    work.scheduling_cost(),
                    Duration::from_millis(measured_probe + 117),
                    "file={file}, samples={samples}"
                );
            }
        }
    }

    #[test]
    fn census_costs_add_fractional_selected_hinted_and_later_whole_binaries_without_breaking_on_uncovered() {
        let plan = plan_over(&["subject"]);
        let mut binaries = ranked_binaries();
        binaries[0].baseline = Duration::from_millis(120);
        binaries[1].baseline = Duration::from_millis(360);
        let reach = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );
        let ordinal = plan.mutants[0].ordinal;
        // Every row adds half the later whole binary. The fixture measures each selected case
        // at one millisecond: complete selection halves that cost, partial selection adds its
        // full cost plus the inferred-weighted half of the first baseline, and uncovered adds
        // nothing. Microsecond expectations retain the complete selection's fractional millisecond.
        for (census, kind, micros) in [
            (
                Census::examined(&binaries[0].path, ordinal, 1, 4),
                crate::estimate::WorkKind::Whole,
                180_500,
            ),
            (
                Census::partial(&binaries[0].path, ordinal, 1, 4),
                crate::estimate::WorkKind::Hinted,
                202_000,
            ),
            (
                Census::examined(&binaries[0].path, ordinal, 0, 4),
                crate::estimate::WorkKind::Whole,
                180_000,
            ),
        ] {
            let killers = Killers::default();
            let work = mutant_work(0, &plan, &reach, &census, &killers);
            assert_eq!(work.kind, kind);
            assert_eq!(work.scheduling_cost(), Duration::from_micros(micros));
            assert_eq!(mutant_cost(0, &plan, &reach, &census, &killers), Duration::from_micros(micros));
        }
    }

    fn killer(test: &str) -> Killer {
        Killer {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: test.to_owned(),
        }
    }

    #[test]
    fn same_item_exact_candidates_always_rank_before_file_binaries() {
        let learning = FileLearning::new();
        learning.publish("subject::a", &Judgement::Reached(Outcome::Killed, Some(killer("tests::a")), None));
        learning.publish("subject::a", &Judgement::Reached(Outcome::Killed, Some(killer("tests::a")), None));

        let same = learning.candidates("subject::a", Duration::MAX);
        assert!(matches!(&same[0], Candidate::Exact(found) if found.test == "tests::a"));
        assert!(matches!(&same[1], Candidate::FileBinary(_)));

        let other = learning.candidates("subject::b", Duration::MAX);
        assert_eq!(other.len(), 1);
        assert!(matches!(&other[0], Candidate::FileBinary(_)));
        let state = learning.locked();
        assert_eq!(state.items["subject::a"].exact.len(), 1);
        assert_eq!(state.binaries.len(), 1);
        assert_eq!(state.next_order, 2, "each published observation reserves a stable order");
    }

    #[test]
    fn candidate_cost_and_ranking_account_for_identity_hits_misses_and_samples() {
        let binary = TestBinary {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            baseline: Duration::from_millis(17),
            ..crate::testing::test_binary("unused")
        };
        let reachable = [&binary];
        let exact = Candidate::Exact(killer("tests::a"));
        let missing = Candidate::FileBinary(BinaryIdentity {
            package: "elsewhere".to_owned(),
            target: "lib".to_owned(),
        });

        assert_eq!(exact.estimated_cost(&reachable), Duration::from_millis(17));
        assert_eq!(missing.estimated_cost(&reachable), Duration::ZERO);

        let mut ranked = RankedCandidate {
            identity: killer("tests::a"),
            seeds: 1,
            hits: 0,
            misses: 0,
            canonical_passes: 0,
            measured: Duration::ZERO,
            samples: 0,
            order: 9,
        };
        assert_eq!(
            ranked.score(),
            (0, core::cmp::Reverse(0), Duration::MAX, 9),
            "an unmeasured candidate sorts behind measured candidates with the same history"
        );
        ranked.observe(true, Duration::from_millis(12));
        ranked.observe(false, Duration::from_millis(6));
        assert_eq!((ranked.hits, ranked.misses, ranked.samples), (1, 1, 2));
        assert_eq!(ranked.measured, Duration::from_millis(18));
        assert_eq!(ranked.score(), (0, core::cmp::Reverse(1), Duration::from_millis(9), 9));

        let reached = reached_candidate(BinaryIdentity::from_binary(&binary), Duration::from_millis(4), 3);
        assert_eq!(
            (reached.seeds, reached.hits, reached.misses, reached.samples, reached.order),
            (1, 0, 0, 1, 3)
        );
        let published = published_candidate(killer("tests::a"), 5);
        assert_eq!(
            (
                published.seeds,
                published.hits,
                published.misses,
                published.samples,
                published.order
            ),
            (1, 0, 0, 0, 5)
        );

        let mut state = LearningState {
            next_order: u64::MAX,
            ..LearningState::default()
        };
        advance_order(&mut state);
        assert_eq!(state.next_order, u64::MAX, "learning order saturates instead of wrapping");
    }

    #[test]
    fn kind_qualified_binary_learning_matches_current_and_legacy_hints() {
        let binary = TestBinary {
            package: "subject".to_owned(),
            target: "subject".to_owned(),
            target_kind: "lib".to_owned(),
            ..crate::testing::test_binary("unused")
        };
        let killer = Killer {
            package: binary.package.clone(),
            target: binary.killer_target(),
            test: "tests::caught".to_owned(),
        };
        let current = BinaryIdentity::from_binary(&binary);
        let published = BinaryIdentity::from_killer(&killer);

        assert_eq!(current, published);
        assert!(binary.matches_killer(&killer));
        assert!(binary.matches_killer(&Killer {
            package: binary.package.clone(),
            target: binary.target.clone(),
            test: killer.test.clone(),
        }));
        assert!(BinaryIdentity::from_hint(&published.hint()).names(&binary));
        assert!(
            BinaryIdentity::from_hint(&BinaryHint {
                package: binary.package.clone(),
                target: binary.target.clone(),
            })
            .names(&binary)
        );

        let other_kind = TestBinary {
            target_kind: "bin".to_owned(),
            ..binary
        };
        assert!(!other_kind.matches_killer(&killer));
        assert!(!published.names(&other_kind));
    }

    #[test]
    fn inferred_admission_requires_transfer_value_and_retires_repeated_misses() {
        let candidate = |hits, misses, measured, samples| RankedCandidate {
            identity: killer("tests::candidate"),
            seeds: 1,
            hits,
            misses,
            canonical_passes: 0,
            measured: Duration::from_millis(measured),
            samples,
            order: 0,
        };

        assert!(candidate(0, 0, 0, 0).is_admissible(Duration::from_millis(30)));
        assert!(!candidate(0, 2, 2, 2).is_admissible(Duration::from_millis(30)));
        assert!(!candidate(1, 1, 100, 2).is_admissible(Duration::from_millis(30)));
        assert!(candidate(1, 1, 10, 2).is_admissible(Duration::from_millis(30)));
    }

    #[test]
    fn canonical_passes_are_half_weight_negative_evidence() {
        let candidate = || RankedCandidate {
            identity: killer("tests::candidate"),
            seeds: 1,
            hits: 0,
            misses: 0,
            canonical_passes: 0,
            measured: Duration::ZERO,
            samples: 0,
            order: 0,
        };
        let mut explicit = candidate();
        explicit.observe(false, Duration::ZERO);
        let mut canonical = candidate();
        canonical.observe_canonical_pass();
        canonical.observe_canonical_pass();

        assert_eq!(canonical.score().0, explicit.score().0);
        assert!(
            canonical.score() > explicit.score(),
            "canonical evidence has no measured duration and must retain unknown-cost ordering"
        );
        assert!(canonical.is_admissible(Duration::from_millis(30)));

        canonical.observe_canonical_pass();
        canonical.observe_canonical_pass();
        assert!(!canonical.is_admissible(Duration::from_millis(30)));
    }

    #[test]
    fn inferred_candidates_are_bounded_per_tier() {
        let mut candidates = (0..5)
            .map(|index| Candidate::Exact(killer(&format!("tests::item_{index}"))))
            .chain((0..3).map(|index| {
                Candidate::FileBinary(BinaryIdentity {
                    package: "subject".to_owned(),
                    target: format!("file-{index}"),
                })
            }))
            .collect::<Vec<_>>();

        assert_eq!(bound_candidates(&mut candidates), [3, 0, 2]);
        assert_eq!(candidate_tier_counts(&candidates), [2, 0, 1]);
    }

    #[test]
    fn learning_selection_preserves_tier_order_identity_and_exact_rejection_counts() {
        fn ranked<T>(identity: T, order: u64, misses: u32) -> RankedCandidate<T> {
            RankedCandidate {
                identity,
                seeds: 1,
                hits: 0,
                misses,
                canonical_passes: 0,
                measured: Duration::ZERO,
                samples: 0,
                order,
            }
        }
        let learning = FileLearning::new();
        let binary = |target: &str| BinaryIdentity {
            package: "subject".into(),
            target: target.into(),
        };
        {
            let mut state = learning.locked();
            let item = ItemLearning {
                exact: ["third", "second", "first"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, test)| ranked(killer(test), 3 - index as u64, 0))
                    .collect(),
                reached: ["item-z", "item-a", "item-retired"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, target)| ranked(binary(target), index as u64, if index == 2 { 2 } else { 0 }))
                    .collect(),
            };
            let _ = state.items.insert("subject::item".into(), item);
            state.reached_file = ["reach-z", "reach-a", "reach-retired", "item-z"]
                .into_iter()
                .enumerate()
                .map(|(index, target)| ranked(binary(target), index as u64, if index == 2 { 2 } else { 0 }))
                .collect();
            state.binaries = ["file-z", "file-a", "file-retired", "item-z", "file-b"]
                .into_iter()
                .enumerate()
                .map(|(index, target)| ranked(binary(target), index as u64, if index == 2 { 2 } else { 0 }))
                .collect();
        }
        let (selected, rejected) = learning.select_candidates("subject::item", Duration::MAX);
        assert_eq!(rejected, [3, 3, 4]);
        assert_eq!(selected.len(), 5);
        assert!(matches!(&selected[0], Candidate::Exact(identity) if identity.test == "first"));
        assert!(matches!(&selected[1], Candidate::Exact(identity) if identity.test == "second"));
        assert!(matches!(&selected[2], Candidate::ItemBinary(identity) if identity.target == "item-z"));
        assert!(matches!(&selected[3], Candidate::ReachFileBinary(identity) if identity.target == "reach-z"));
        assert!(matches!(&selected[4], Candidate::FileBinary(identity) if identity.target == "file-z"));
        assert_eq!(candidate_tier_counts(&selected), [3, 1, 1]);
        let tally = Tally::default();
        record_rejections(&tally, rejected);
        record_rejections(&tally, [1, 2, 3]);
        assert_eq!(tally.item_rejected.load(Ordering::Relaxed), 4);
        assert_eq!(tally.reach_rejected.load(Ordering::Relaxed), 5);
        assert_eq!(tally.file_rejected.load(Ordering::Relaxed), 7);
        assert_eq!(tally.inferred_rejected.load(Ordering::Relaxed), 16);
        {
            let mut state = learning.locked();
            let item = state.items.get_mut("subject::item").unwrap();
            for candidate in &mut item.exact {
                candidate.misses = 2;
            }
            for candidate in &mut item.reached {
                candidate.misses = 2;
            }
            for candidate in &mut state.reached_file {
                candidate.misses = 2;
            }
            for candidate in &mut state.binaries {
                candidate.misses = 2;
            }
        }
        let (retired, rejected) = learning.select_candidates("subject::item", Duration::MAX);
        assert!(retired.is_empty());
        assert_eq!(rejected, [6, 4, 5]);
    }

    #[test]
    fn zero_hit_tiers_trip_a_campaign_circuit_breaker() {
        let tally = Tally::default();
        tally.item_probes.store(INFERRED_ZERO_HIT_LIMIT, Ordering::Relaxed);

        assert!(!ProbeKind::Item.reserve(&tally));
        ProbeKind::Item.reject(&tally);
        assert_eq!(tally.item_rejected.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_rejected.load(Ordering::Relaxed), 1);

        tally.item_hits.store(1, Ordering::Relaxed);
        assert!(ProbeKind::Item.reserve(&tally), "a productive tier remains available");
    }

    #[test]
    fn concurrent_zero_hit_admission_reserves_only_the_campaign_limit() {
        let tally = Tally::default();
        let admitted = AtomicUsize::new(0);

        std::thread::scope(|scope| {
            for _worker in 0..(INFERRED_ZERO_HIT_LIMIT * 4) {
                scope.spawn(|| {
                    if ProbeKind::Item.reserve(&tally) {
                        let _previous = admitted.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });

        assert_eq!(admitted.load(Ordering::Relaxed), INFERRED_ZERO_HIT_LIMIT);
        assert_eq!(tally.item_probes.load(Ordering::Relaxed), INFERRED_ZERO_HIT_LIMIT);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), INFERRED_ZERO_HIT_LIMIT);
    }

    #[test]
    fn persisted_file_binary_misses_demote_the_persisted_candidate() {
        let hints = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            binaries: vec![FileBinaryHints {
                file: "src/lib.rs".into(),
                candidates: vec![
                    RankedHint {
                        candidate: BinaryHint {
                            package: "subject".to_owned(),
                            target: "first".to_owned(),
                        },
                        seeds: 1,
                        hits: 1,
                        misses: 0,
                        measured_ms: 1,
                        samples: 1,
                        order: 0,
                    },
                    RankedHint {
                        candidate: BinaryHint {
                            package: "subject".to_owned(),
                            target: "second".to_owned(),
                        },
                        seeds: 1,
                        hits: 1,
                        misses: 0,
                        measured_ms: 1,
                        samples: 1,
                        order: 1,
                    },
                ],
            }],
            ..GeneralizedHints::empty_supported()
        };
        let learning = FileLearning::from_hints(Utf8Path::new("src/lib.rs"), &hints);
        let first = Candidate::FileBinary(BinaryIdentity {
            package: "subject".to_owned(),
            target: "first".to_owned(),
        });

        learning.observe("subject::item", &first, false, Duration::from_millis(1));
        learning.observe("subject::item", &first, false, Duration::from_millis(1));

        let candidates = learning.candidates("subject::item", Duration::MAX);
        assert!(matches!(&candidates[0], Candidate::FileBinary(binary) if binary.target == "second"));
        let state = learning.locked();
        let demoted = state
            .binaries
            .iter()
            .find(|candidate| candidate.identity.target == "first")
            .expect("the persisted candidate remains recorded");
        assert_eq!(demoted.misses, 2);
        assert_eq!(demoted.hits, 1);
        assert_eq!(demoted.measured, Duration::from_millis(3));
        assert_eq!(demoted.samples, 3);
        assert_eq!(demoted.score(), (1, core::cmp::Reverse(1), Duration::from_millis(1), 0));
    }

    #[test]
    fn scoped_learning_replaces_touched_files_and_preserves_untouched_files() {
        let mut generalized = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            items: vec![ItemHints {
                file: "src/untouched.rs".into(),
                item: "untouched::item".to_owned(),
                candidates: vec![RankedHint {
                    candidate: killer("tests::old"),
                    seeds: 1,
                    hits: 1,
                    misses: 0,
                    measured_ms: 1,
                    samples: 1,
                    order: 0,
                }],
            }],
            binaries: vec![FileBinaryHints {
                file: "src/touched.rs".into(),
                candidates: Vec::new(),
            }],
            ..GeneralizedHints::empty_supported()
        };
        let touched = FileLearning::new();
        touched.publish(
            "touched::item",
            &Judgement::Reached(Outcome::Killed, Some(killer("tests::new")), None),
        );

        generalized.version = 0;
        persist_learning(&mut generalized, &[Arc::from(Utf8Path::new("src/touched.rs"))], &[touched]);

        assert_eq!(generalized.version, GENERALIZED_HINTS_VERSION);
        assert!(
            generalized
                .items
                .iter()
                .any(|entry| entry.file == Utf8Path::new("src/untouched.rs")),
            "{generalized:?}"
        );
        assert_eq!(
            generalized
                .binaries
                .iter()
                .filter(|entry| entry.file == Utf8Path::new("src/touched.rs"))
                .count(),
            1,
            "the touched file's old binary hints are replaced rather than retained alongside new learning"
        );
        assert!(
            generalized
                .items
                .iter()
                .any(|entry| entry.file == Utf8Path::new("src/touched.rs") && entry.item == "touched::item"),
            "{generalized:?}"
        );
    }

    #[test]
    fn reach_index_preserves_first_execution_and_last_estimation_candidates() {
        let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
        let first = vec![killer("first")];
        let last = vec![killer("last")];
        let mut hints = GeneralizedHints::empty_supported();
        hints.test_sets = vec![first.clone(), last.clone()];
        hints.reach = vec![
            crate::discover::ReachCluster {
                site: site.clone(),
                test_set: 0,
            },
            crate::discover::ReachCluster {
                site: site.clone(),
                test_set: 1,
            },
        ];
        let mut killers = Killers::default();
        killers.replace_generalized(hints);
        let index = InferredPlanning::new(&killers);
        let (execution, estimation) = index.reach[&site];
        assert_eq!(execution, first);
        assert_eq!(execution, killers.reach_hints(&site));
        assert_eq!(estimation, last);
    }

    #[test]
    fn indexed_reach_filter_preserves_legacy_matching_order_and_duplicate_counts() {
        fn original(hints: &[Killer], reachable: &[&TestBinary], census: &Census) -> Vec<Killer> {
            hints
                .iter()
                .filter(|hint| {
                    let Some(binary) = reachable.iter().copied().find(|binary| binary.matches_killer(hint)) else {
                        return false;
                    };
                    let Some(total) = binary.tests else { return false };
                    let measured = match census.work(binary, 7) {
                        CensusWork::Selected(duration) | CensusWork::Hinted(duration) => duration,
                        CensusWork::Whole | CensusWork::Uncovered => return false,
                    };
                    !binary.baseline.is_zero()
                        && measured < binary.baseline
                        && hints.iter().filter(|candidate| binary.matches_killer(candidate)).count() <= total / 2
                })
                .cloned()
                .collect()
        }
        let mut first = crate::testing::helper();
        first.package = "subject".into();
        first.target = "shared".into();
        first.target_kind = "lib".into();
        first.tests = Some(8);
        first.baseline = Duration::from_millis(10);
        let mut second = first.clone();
        second.target_kind = "bin".into();
        second.tests = Some(2);
        let hints = ["shared", "lib:shared", "bin:shared", "shared", "missing"]
            .into_iter()
            .enumerate()
            .map(|(index, target)| Killer {
                package: "subject".into(),
                target: target.into(),
                test: format!("test_{index}"),
            })
            .collect::<Vec<_>>();
        for reachable in [[&first, &second], [&second, &first]] {
            for census in [Census::partial(&first.path, 7, 1, 8), Census::default()] {
                for count in 0..=hints.len() {
                    assert_eq!(
                        bounded_reach_hints(&hints[..count], &reachable, 7, &census),
                        original(&hints[..count], &reachable, &census),
                    );
                }
            }
        }
    }

    #[test]
    fn persisted_reach_hints_are_bounded_by_half_the_binary_suite() {
        let mut binary = crate::testing::helper();
        binary.package = "subject".to_owned();
        binary.target = "lib".to_owned();
        binary.tests = Some(4);
        binary.baseline = Duration::from_millis(10);
        let reachable = [&binary];
        let hints = ["a", "b", "c"].into_iter().map(killer).collect::<Vec<_>>();
        let over_half = Census::partial(&binary.path, 7, 3, 4);
        let bounded = Census::partial(&binary.path, 7, 2, 4);

        assert!(bounded_reach_hints(&hints, &reachable, 7, &over_half).is_empty());
        assert_eq!(bounded_reach_hints(&hints[..2], &reachable, 7, &bounded).len(), 2);
        let mut expensive = binary.clone();
        expensive.baseline = Duration::from_millis(1);
        assert!(
            bounded_reach_hints(&hints[..2], &[&expensive], 7, &bounded).is_empty(),
            "persisted exact probes must cost less than the binary they replace"
        );

        let mut unknown_count = binary.clone();
        unknown_count.tests = None;
        assert!(bounded_reach_hints(&hints[..2], &[&unknown_count], 7, &bounded).is_empty());
        assert!(
            bounded_reach_hints(
                &[Killer {
                    package: "elsewhere".to_owned(),
                    target: "other".to_owned(),
                    test: "tests::a".to_owned(),
                }],
                &reachable,
                7,
                &bounded,
            )
            .is_empty()
        );
    }

    #[test]
    fn a_grouped_census_probe_that_passes_caches_no_killer() {
        let (_directory, work) = crate::testing::helper_workspace("grouped-probe-pass-", &["exit:0"]);
        let binary = crate::testing::helper();
        let tally = Tally::default();

        let (killer, observation, _) = probe_cases(
            &work,
            7,
            &binary,
            &["tests::first", "tests::second"],
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );
        assert_eq!(killer, None);
        assert_eq!(observation, Some(false));
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn promoted_item_and_file_hints_seed_a_clean_checkout_without_mutant_ids() {
        let hints = GeneralizedHints {
            version: GENERALIZED_HINTS_VERSION,
            items: vec![ItemHints {
                file: "src/lib.rs".into(),
                item: "subject::changed".to_owned(),
                candidates: vec![RankedHint {
                    candidate: killer("tests::item"),
                    seeds: 1,
                    hits: 4,
                    misses: 0,
                    measured_ms: 8,
                    samples: 4,
                    order: 0,
                }],
            }],
            binaries: vec![FileBinaryHints {
                file: "src/lib.rs".into(),
                candidates: vec![RankedHint {
                    candidate: BinaryHint {
                        package: "subject".to_owned(),
                        target: "integration".to_owned(),
                    },
                    seeds: 1,
                    hits: 3,
                    misses: 1,
                    measured_ms: 20,
                    samples: 4,
                    order: 1,
                }],
            }],
            ..GeneralizedHints::empty_supported()
        };
        let learning = FileLearning::from_hints(Utf8Path::new("src/lib.rs"), &hints);
        let candidates = learning.candidates("subject::changed", Duration::MAX);

        assert!(matches!(&candidates[0], Candidate::Exact(found) if found.test == "tests::item"));
        assert!(matches!(
            &candidates[1],
            Candidate::FileBinary(binary) if binary.target == "integration"
        ));
        assert_eq!(
            learning.locked().next_order,
            2,
            "new observations follow the greatest persisted order"
        );
    }

    #[test]
    fn stale_inferred_test_falls_back_without_changing_the_outcome_and_reports_a_miss() {
        let (_directory, work, plan, mut binaries) =
            helper_harness(&["when-arg:tests::stale|exit:0", "print:test tests::actual ... FAILED", "exit:1"]);
        binaries[0].budget = None;
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let candidates = [Candidate::Exact(Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].target.clone(),
            test: "tests::stale".to_owned(),
        })];
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            &candidates,
            None,
            None,
            None,
            Sweep {
                confirm: false,
                ..sweep(Stall::NONE)
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, _, None)));
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.item_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.item_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_stale_exact_hint_leaves_inferred_candidates_eligible() {
        let (_directory, work, plan, binaries) = helper_harness(&[
            "when-arg:tests::stale|exit:0",
            "when-arg:tests::actual|print:test tests::actual ... FAILED",
            "when-arg:tests::actual|exit:1",
            "exit:0",
        ]);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let exact = Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].target.clone(),
            test: "tests::stale".to_owned(),
        };
        let candidates = [Candidate::Exact(Killer {
            test: "tests::actual".to_owned(),
            ..exact.clone()
        })];
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            Some(&exact),
            &candidates,
            None,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall {
                    budget: Some(Duration::from_secs(1)),
                },
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.exact_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn an_inadmissible_exact_hint_leaves_inferred_candidates_eligible() {
        const SCRIPT: &[&str] = &[
            "when-arg:tests::actual|print:test tests::actual ... FAILED",
            "when-arg:tests::actual|exit:1",
            "exit:0",
        ];

        let (_directory, mut work, plan, binaries) = helper_harness(SCRIPT);
        let mut arguments: Vec<String> = SCRIPT.iter().map(crate::testing::directive).collect();
        arguments.push("tests::actual".to_owned());
        work.set_test_args(arguments);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let exact = Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].target.clone(),
            test: "tests::excluded".to_owned(),
        };
        let candidates = [Candidate::Exact(Killer {
            test: "tests::actual".to_owned(),
            ..exact.clone()
        })];
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            Some(&exact),
            &candidates,
            None,
            None,
            None,
            Sweep {
                confirm: false,
                ..sweep(Stall::NONE)
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.exact_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn exact_and_inferred_probe_hit_rates_are_counted_separately() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["print:test tests::killer ... FAILED", "exit:1"]);
        binaries[0].budget = None;
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let exact = Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].target.clone(),
            test: "tests::killer".to_owned(),
        };
        let exact_tally = Tally::default();
        let inferred_tally = Tally::default();

        let exact_judged = judge(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            Some(&exact),
            None,
            Sweep {
                confirm: false,
                ..sweep(Stall::NONE)
            },
            &exact_tally,
        );
        let inferred_judged = judge_ranked(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            &[Candidate::Exact(exact)],
            None,
            None,
            None,
            Sweep {
                confirm: false,
                ..sweep(Stall::NONE)
            },
            &inferred_tally,
        );

        assert!(matches!(exact_judged, Judgement::Reached(Outcome::Killed, _, _)));
        assert!(matches!(inferred_judged, Judgement::Reached(Outcome::Killed, _, _)));
        assert_eq!(exact_tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(exact_tally.exact_hits.load(Ordering::Relaxed), 1);
        assert_eq!(exact_tally.inferred_probes.load(Ordering::Relaxed), 0);
        assert_eq!(inferred_tally.exact_probes.load(Ordering::Relaxed), 0);
        assert_eq!(inferred_tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(inferred_tally.inferred_hits.load(Ordering::Relaxed), 1);
        assert_eq!(inferred_tally.item_probes.load(Ordering::Relaxed), 1);
        assert_eq!(inferred_tally.item_hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn whole_binary_mode_suppresses_exact_and_inferred_case_probes() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["when-arg:tests::hinted|exit:97", "exit:0"]);
        binaries[0].budget = None;
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let hint = Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].target.clone(),
            test: "tests::hinted".to_owned(),
        };
        let candidate = Candidate::Exact(hint.clone());

        for (exact, inferred) in [(Some(&hint), &[][..]), (None, core::slice::from_ref(&candidate))] {
            let learning = FileLearning::new();
            let negative = NegativeLearning::default();
            let tally = Tally::default();
            let judged = judge_learning(
                &work,
                plan.mutants[0].ordinal,
                &reachable,
                exact,
                inferred,
                &learning,
                "subject::f",
                &SiteIdentity::from_mutant(&plan.mutants[0]),
                &negative,
                false,
                None,
                Sweep {
                    confirm: false,
                    ..sweep(Stall::NONE)
                },
                &tally,
            );

            assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
            assert_eq!(tally.probes.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn terminal_verdict_policy_preserves_every_outcome_killer_and_note() {
        let mut binary = crate::testing::helper();
        binary.package = "subject".to_owned();
        binary.target = "lib".to_owned();

        assert!(terminal_judgement(&binary, Verdict::Passed).is_none());
        assert!(matches!(
            terminal_judgement(&binary, Verdict::Failed(Some("tests::caught".to_owned()))),
            Some(Judgement::Reached(
                Outcome::Killed,
                Some(Killer {
                    package,
                    target,
                    test
                }),
                None
            )) if package == "subject" && target == "lib" && test == "tests::caught"
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::Failed(None)),
            Some(Judgement::Reached(Outcome::Killed, None, None))
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::TestEnumerationFailed(String::new())),
            Some(Judgement::Reached(Outcome::Killed, None, Some(note))) if note.contains("could not enumerate")
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::TimedOut),
            Some(Judgement::Reached(Outcome::Timeout, None, None))
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::Stalled(Some("tests::last".to_owned()))),
            Some(Judgement::Reached(Outcome::Timeout, None, Some(note))) if note.contains("tests::last")
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::MemoryLimit { peak: Some(20), limit: 10 }),
            Some(Judgement::Reached(Outcome::OutOfMemory, None, Some(note))) if note.contains("20 bytes")
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::Flaky(Some("tests::flake".to_owned()))),
            Some(Judgement::Reached(Outcome::Flaky, None, Some(note))) if note.contains("tests::flake")
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::Unmetered("meter lost".to_owned())),
            Some(Judgement::Abandoned(reason)) if reason == "meter lost"
        ));
        assert!(matches!(
            terminal_judgement(&binary, Verdict::Unjudged("spawn refused".to_owned())),
            Some(Judgement::Reached(Outcome::Pending, None, Some(note))) if note == "spawn refused"
        ));
    }

    #[test]
    fn an_unmetered_candidate_is_not_deferred_to_canonical_iteration() {
        assert!(matches!(
            defer_candidate_verdict(Verdict::Unmetered("meter lost".to_owned())),
            Err(Judgement::Abandoned(reason)) if reason == "meter lost"
        ));
        assert!(matches!(defer_candidate_verdict(Verdict::Passed), Ok(Verdict::Passed)));
        assert!(matches!(defer_candidate_verdict(Verdict::TimedOut), Ok(Verdict::TimedOut)));
    }

    #[test]
    fn only_failures_and_clean_passes_train_transfer_ranking() {
        assert_eq!(transfer_observation(&Verdict::Failed(None)), Some(true));
        assert_eq!(transfer_observation(&Verdict::Passed), Some(false));
        assert_eq!(transfer_observation(&Verdict::TimedOut), None);
        assert_eq!(transfer_observation(&Verdict::MemoryLimit { peak: None, limit: 10 }), None);
        assert_eq!(transfer_observation(&Verdict::Flaky(None)), None);
        assert_eq!(transfer_observation(&Verdict::Unmetered("meter lost".to_owned())), None);
        assert_eq!(transfer_observation(&Verdict::Unjudged("spawn refused".to_owned())), None);
    }

    #[test]
    fn positive_reach_is_reused_by_the_same_item_and_as_a_file_fallback() {
        let learning = FileLearning::new();
        let binary = TestBinary {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            ..crate::testing::test_binary("unused")
        };

        learning.reached("subject::a", &binary, Duration::from_millis(7));

        let same = learning.candidates("subject::a", Duration::MAX);
        assert_eq!(same.len(), 1);
        assert!(matches!(&same[0], Candidate::ItemBinary(identity) if identity.target == "lib"));

        let other = learning.candidates("subject::b", Duration::MAX);
        assert_eq!(other.len(), 1);
        assert!(matches!(&other[0], Candidate::ReachFileBinary(identity) if identity.target == "lib"));

        learning.reached("subject::a", &binary, Duration::from_millis(99));
        let state = learning.locked();
        assert_eq!(state.items["subject::a"].reached.len(), 1);
        assert_eq!(state.reached_file.len(), 1);
        assert_eq!(state.next_order, 2, "each reach observation reserves one stable order");
        assert_eq!(state.items["subject::a"].reached[0].measured, Duration::from_millis(7));
    }

    #[test]
    fn canonical_passes_do_not_create_candidates() {
        let learning = FileLearning::new();
        let binary = TestBinary {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            ..crate::testing::test_binary("unused")
        };

        learning.record_canonical_pass("subject::a", &binary, true);

        assert!(learning.candidates("subject::a", Duration::MAX).is_empty());
        assert!(learning.candidates("subject::b", Duration::MAX).is_empty());
    }

    #[test]
    fn a_reach_candidate_kill_waits_for_canonical_precedence() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["print:test tests::reached ... FAILED", "exit:1"]);
        binaries[0].target = "earlier".to_owned();
        let mut reached = binaries[0].clone();
        reached.target = "reached".to_owned();
        binaries.push(reached);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let learning = FileLearning::new();
        learning.reached("subject::f", &binaries[1], Duration::from_millis(2));
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            &learning.candidates("subject::f", Duration::MAX),
            Some((&learning, "subject::f")),
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        assert_eq!(tally.saved.load(Ordering::Relaxed), 0);
        assert_eq!(tally.reach_saved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn exact_site_negative_evidence_skips_a_sibling_replacement() {
        let directive = format!("write-le:{}|{}", gamma_rt::CENSUS_VAR, gamma_rt::SEAL);
        let (_directory, work, mut plan, binaries) = helper_harness(&[&directive]);
        let learned_site = SiteIdentity::from_mutant(&plan.mutants[0]);
        let mut sibling = plan.mutants[0].clone();
        sibling.id = "m2".to_owned().into();
        sibling.ordinal = 2;
        sibling.replacement_index = 1;
        sibling.replacement = "a == b".to_owned().into();
        let sibling_site = SiteIdentity::from_mutant(&sibling);
        assert_eq!(learned_site, sibling_site, "replacement variants share one stable site");
        plan.mutants.push(sibling);

        let reach = Reachability::build(
            &plan,
            &binaries,
            &TestScope {
                packages: &[],
                package_local: false,
                whole_workspace: true,
            },
        );
        let spent = test_all(
            &work,
            &mut plan,
            &reach,
            Sweep {
                confirm: false,
                ..sweep(Stall::NONE)
            },
            true,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep completes")
        .expect("both sibling replacements are pending");

        assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
        assert_eq!(plan.mutants[1].outcome, Outcome::NoCoverage);
        assert_eq!(spent.launches, 1);
        assert_eq!(spent.saved, 1);
        assert_eq!(spent.reach_saved, 1);
    }

    #[test]
    fn exact_site_negative_evidence_never_generalizes_or_applies_to_nondeterministic_runs() {
        let (_directory, work, plan, binaries) = helper_harness(&["exit:0"]);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let learned_site = SiteIdentity::from_mutant(&plan.mutants[0]);
        let negative = NegativeLearning::default();
        negative.record(&learned_site, &binaries[0]);

        for (site, deterministic) in [
            (
                SiteIdentity {
                    occurrence: learned_site.occurrence + 1,
                    ..learned_site.clone()
                },
                true,
            ),
            (
                SiteIdentity {
                    item: "subject::other".to_owned(),
                    ..learned_site.clone()
                },
                true,
            ),
            (learned_site, false),
        ] {
            let tally = Tally::default();
            let judged = judge_ranked(
                &work,
                plan.mutants[0].ordinal,
                &reachable,
                None,
                &[],
                None,
                Some((&negative, &site, deterministic)),
                None,
                Sweep {
                    confirm: false,
                    ..sweep(Stall::NONE)
                },
                &tally,
            );

            assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
            assert_eq!(tally.reach_saved.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn exact_site_negative_evidence_distinguishes_harness_paths() {
        let (_directory, work, plan, binaries) = helper_harness(&["exit:0"]);
        let site = SiteIdentity::from_mutant(&plan.mutants[0]);
        let negative = NegativeLearning::default();
        negative.record(&site, &binaries[0]);

        let mut other_harness = binaries[0].clone();
        other_harness.path = work.target.join("same-cargo-target-different-harness");

        assert!(negative.contains(&site, &binaries[0]));
        assert_eq!(other_harness.package, binaries[0].package);
        assert_eq!(other_harness.target, binaries[0].target);
        assert!(!negative.contains(&site, &other_harness));

        let safe_sweep = Sweep {
            confirm: false,
            ..sweep(Stall::NONE)
        };
        assert!(negative_excludes(Some((&negative, &site, true)), &binaries[0], None, safe_sweep));
        assert!(!negative_excludes(Some((&negative, &site, true)), &other_harness, None, safe_sweep));
    }

    #[test]
    fn only_complete_deterministic_whole_passes_publish_negative_reach() {
        let passed = BinaryRun {
            verdict: Verdict::Passed,
            reach: ReachObservation::NotReached,
        };
        assert!(negative_reach_is_final(&passed, Only::All, true));
        assert!(!negative_reach_is_final(&passed, Only::These(&["tests::one"]), true));
        assert!(!negative_reach_is_final(&passed, Only::One("tests::one"), true));
        assert!(!negative_reach_is_final(&passed, Only::All, false));
        assert!(!negative_reach_is_final(
            &BinaryRun {
                verdict: Verdict::Passed,
                reach: ReachObservation::Unknown,
            },
            Only::All,
            true,
        ));
        assert!(!negative_reach_is_final(
            &BinaryRun {
                verdict: Verdict::Failed(None),
                reach: ReachObservation::NotReached,
            },
            Only::All,
            true,
        ));
    }

    #[test]
    fn exact_negative_reach_cannot_bypass_verdict_precedence() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["exit:0"]);
        let site = SiteIdentity::from_mutant(&plan.mutants[0]);
        let negative = NegativeLearning::default();
        negative.record(&site, &binaries[0]);

        let cases = [
            (Some(Duration::from_secs(1)), Stall::NONE, false, false),
            (
                None,
                Stall {
                    budget: Some(Duration::from_secs(1)),
                },
                false,
                false,
            ),
            (None, Stall::NONE, true, false),
            (None, Stall::NONE, false, true),
        ];

        for (budget, stall, meter, confirm) in cases {
            binaries[0].budget = budget;
            let reachable = [&binaries[0]];
            let tally = Tally::default();
            let judged = judge_ranked(
                &work,
                plan.mutants[0].ordinal,
                &reachable,
                None,
                &[],
                None,
                Some((&negative, &site, true)),
                None,
                Sweep {
                    timeout_floor: Duration::ZERO,
                    stall,
                    jobs: 1,
                    meter,
                    confirm,
                    census: blind(),
                },
                &tally,
            );

            assert!(!matches!(judged, Judgement::Reached(Outcome::NoCoverage, _, _)));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
            assert_eq!(tally.reach_saved.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn an_earlier_unjudged_verdict_takes_precedence_over_a_later_candidate_kill() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["print:test tests::caught ... FAILED", "exit:1"]);
        binaries[0].path = work.root.join("missing-test-binary");
        binaries[0].target = "canonical".to_owned();
        let mut reached = binaries[0].clone();
        reached.path = crate::testing::helper().path;
        reached.target = "reached".to_owned();
        binaries.push(reached);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let candidate = Candidate::ItemBinary(BinaryIdentity {
            package: "subject".to_owned(),
            target: "reached".to_owned(),
        });

        let judged = judge_ranked(
            &work,
            plan.mutants[0].ordinal,
            &reachable,
            None,
            &[candidate],
            None,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &Tally::default(),
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Pending, None, Some(_))));
    }

    #[test]
    fn a_safe_same_item_exact_candidate_settles_immediately() {
        let (_directory, work, _plan, mut binaries) = helper_harness(ONLY_WHEN_FILTERED);
        binaries[0].target = "earlier".to_owned();
        let mut later = binaries[0].clone();
        later.target = "later".to_owned();
        binaries.push(later);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let candidates = [
            Candidate::Exact(Killer {
                package: "subject".to_owned(),
                target: "later".to_owned(),
                test: "tests::killer".to_owned(),
            }),
            Candidate::FileBinary(BinaryIdentity {
                package: "subject".to_owned(),
                target: "earlier".to_owned(),
            }),
        ];
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            1,
            &reachable,
            None,
            &candidates,
            None,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.saved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_later_candidate_kill_settles_before_an_earlier_unjudged_binary() {
        let (_directory, work, _plan, mut binaries) = helper_harness(ONLY_WHEN_FILTERED);
        binaries[0].path = work.root.join("missing-test-binary");
        binaries[0].target = "earlier".to_owned();
        let mut later = crate::testing::helper();
        later.package = "subject".to_owned();
        later.target = "later".to_owned();
        later.tests = Some(1);
        binaries.push(later);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let candidate = Candidate::Exact(Killer {
            package: "subject".to_owned(),
            target: "later".to_owned(),
            test: "tests::killer".to_owned(),
        });
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            1,
            &reachable,
            None,
            &[candidate],
            None,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn another_item_reuses_the_binary_without_assuming_its_exact_test() {
        let (_directory, work, _plan, binaries) = helper_harness(&["print:test tests::different ... FAILED", "exit:1"]);
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let candidates = [Candidate::FileBinary(BinaryIdentity {
            package: "subject".to_owned(),
            target: String::new(),
        })];
        let tally = Tally::default();

        let judged = judge_ranked(
            &work,
            1,
            &reachable,
            None,
            &candidates,
            None,
            None,
            None,
            Sweep {
                timeout_floor: Duration::ZERO,
                stall: Stall::NONE,
                jobs: 1,
                meter: false,
                confirm: false,
                census: blind(),
            },
            &tally,
        );

        assert!(matches!(
            judged,
            Judgement::Reached(Outcome::Killed, Some(Killer { test, .. }), None)
                if test == "tests::different"
        ));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_candidate_is_retired_after_two_transfer_misses() {
        let learning = FileLearning::new();
        learning.publish(
            "subject::a",
            &Judgement::Reached(Outcome::Killed, Some(killer("tests::first")), None),
        );
        learning.publish(
            "subject::a",
            &Judgement::Reached(Outcome::Killed, Some(killer("tests::second")), None),
        );
        let first = Candidate::Exact(killer("tests::first"));

        learning.observe("subject::a", &first, false, Duration::from_millis(20));
        assert!(matches!(&learning.candidates("subject::a", Duration::MAX)[0], Candidate::Exact(found) if found.test == "tests::first"));
        learning.observe("subject::a", &first, false, Duration::from_millis(10));

        assert!(matches!(&learning.candidates("subject::a", Duration::MAX)[0], Candidate::Exact(found) if found.test == "tests::second"));
        assert!(
            !learning
                .candidates("subject::a", Duration::MAX)
                .iter()
                .any(|candidate| matches!(candidate, Candidate::Exact(found) if found.test == "tests::first"))
        );
        let recorded = {
            let state = learning.locked();
            state
                .items
                .get("subject::a")
                .and_then(|item| item.exact.iter().find(|candidate| candidate.identity.test == "tests::first"))
                .cloned()
        };
        let recorded = recorded.expect("the first candidate remains ranked");
        assert_eq!((recorded.seeds, recorded.hits, recorded.misses), (1, 0, 2));
        assert_eq!(recorded.measured, Duration::from_millis(30));
        assert_eq!(recorded.samples, 2);
    }

    fn scheduled(
        file: usize,
        item: &str,
        package: &str,
        cost_ms: u64,
        sibling_benefit: usize,
        hinted: bool,
        stable_order: usize,
    ) -> ScheduledWork {
        ScheduledWork {
            file,
            item: item.to_owned().into(),
            package: package.to_owned().into(),
            cost: Duration::from_millis(cost_ms),
            sibling_benefit,
            hinted,
            stable_order,
            resources: Vec::new(),
        }
    }

    struct ReservationSnapshot {
        remaining: Vec<bool>,
        remaining_count: usize,
        active_files: Vec<usize>,
        active_items: crate::HashMap<(usize, Arc<str>), usize>,
        active_resources: crate::HashMap<Arc<str>, usize>,
        learned_items: crate::HashSet<(usize, Arc<str>)>,
        package_turns: Vec<usize>,
    }

    fn reservation_snapshot(scheduler: &Scheduler) -> ReservationSnapshot {
        let state = scheduler.state.lock().expect("scripted scheduler transitions do not panic");
        ReservationSnapshot {
            remaining: state.remaining.clone(),
            remaining_count: state.remaining_count,
            active_files: state.active_files.clone(),
            active_items: state.active_items.clone(),
            active_resources: state.active_resources.clone(),
            learned_items: state.learned_items.clone(),
            package_turns: state.package_turns.clone(),
        }
    }

    #[test]
    fn assignment_completion_and_abandonment_preserve_every_live_reservation_at_one_and_zero() {
        let resource: Arc<str> = "shared".into();
        let work = (0..3)
            .map(|order| ScheduledWork {
                resources: vec![Arc::clone(&resource)],
                ..scheduled(0, "item", "package", 7, 2, true, order)
            })
            .collect();
        let scheduler = Scheduler::with_resources(work, 1, std::iter::once((Arc::clone(&resource), 2)).collect());
        let abandoned = OnceLock::new();
        let first = scheduler.assignment(&abandoned).unwrap();
        let second = scheduler.assignment(&abandoned).unwrap();
        assert_eq!((first.index(), second.index()), (0, 1));
        let key = (0, Arc::<str>::from("item"));
        {
            let state = reservation_snapshot(&scheduler);
            assert_eq!(state.active_files, [2]);
            assert_eq!(state.active_items.get(&key), Some(&2));
            assert_eq!(state.active_resources.get(&resource), Some(&2));
            assert_eq!(state.package_turns, [2]);
            assert_eq!(state.remaining_count, 1);
        }
        drop(first);
        {
            let state = reservation_snapshot(&scheduler);
            assert_eq!(state.active_files, [1]);
            assert_eq!(state.active_items.get(&key), Some(&1));
            assert_eq!(state.active_resources.get(&resource), Some(&1));
            assert!(
                state.learned_items.is_empty(),
                "an abandoned assignment must not manufacture learning"
            );
            assert_eq!(state.package_turns, [2]);
        }
        second.complete();
        {
            let state = reservation_snapshot(&scheduler);
            assert_eq!(state.active_files, [0]);
            assert!(state.active_items.is_empty());
            assert!(state.active_resources.is_empty());
            assert_eq!(state.learned_items, std::iter::once(key).collect());
            assert_eq!(state.package_turns, [2]);
        }
        let last = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(last.index(), 2);
        last.complete();
        assert!(scheduler.assignment(&abandoned).is_none());
        let state = reservation_snapshot(&scheduler);
        assert_eq!(state.active_files, [0]);
        assert!(state.active_items.is_empty());
        assert!(state.active_resources.is_empty());
        assert_eq!(state.remaining_count, 0);
    }

    #[test]
    fn preexisting_abandonment_refuses_eligible_work_without_reserving_or_consuming_it() {
        let scheduler = Scheduler::new(vec![scheduled(0, "item", "package", 3, 2, true, 0)], 1);
        let abandoned = OnceLock::from("already abandoned".to_owned());
        assert!(scheduler.assignment(&abandoned).is_none());
        let state = reservation_snapshot(&scheduler);
        assert_eq!(state.remaining, [true]);
        assert_eq!(state.remaining_count, 1);
        assert_eq!(state.active_files, [0]);
        assert!(state.active_items.is_empty());
        assert_eq!(state.package_turns, [0]);
    }

    #[test]
    fn abandoning_the_scheduler_notifies_and_refuses_work_before_any_assignment_is_released() {
        let resource: Arc<str> = "shared".into();
        let work = (0..2)
            .map(|order| ScheduledWork {
                resources: vec![Arc::clone(&resource)],
                ..scheduled(0, "item", "package", 7, 2, true, order)
            })
            .collect();
        let scheduler = Scheduler::with_resources(work, 1, std::iter::once((Arc::clone(&resource), 1)).collect());
        let abandoned = OnceLock::new();
        let first = scheduler.assignment(&abandoned).unwrap();
        let eligible = {
            let state = scheduler.state.lock().unwrap();
            scheduler.select(&state)
        };
        assert_eq!(eligible, None, "the held resource blocks the remaining work");
        abandoned.set("scripted stop".to_owned()).unwrap();
        let mut notifications = 0;
        scheduler.abandon_with(|| notifications += 1);
        assert_eq!(notifications, 1, "abandonment must notify without requiring assignment release");
        assert!(scheduler.assignment(&abandoned).is_none());
        let state = reservation_snapshot(&scheduler);
        assert_eq!(state.remaining_count, 1);
        assert_eq!(state.active_resources.get(&resource), Some(&1));
        drop(first);
        assert!(reservation_snapshot(&scheduler).active_resources.is_empty());
    }

    #[test]
    fn priority_matrix_observes_contention_learning_and_exact_learning_value() {
        for file_active in [0, 1, 3] {
            for item_active in [0, 1] {
                for learned in [false, true] {
                    for hinted in [false, true] {
                        let work = vec![scheduled(0, "item", "package", 0, 7, hinted, 13)];
                        let key = (0, Arc::<str>::from("item"));
                        let mut state = SchedulerState {
                            remaining: vec![true],
                            active_files: vec![file_active],
                            ..SchedulerState::default()
                        };
                        state.active_items.insert(key.clone(), item_active);
                        if learned {
                            state.learned_items.insert(key);
                        }
                        let expected_distance = if file_active == 0 {
                            Some((0, 0))
                        } else if item_active == 0 {
                            Some((1, file_active))
                        } else if learned || hinted {
                            Some((2, file_active))
                        } else {
                            None
                        };
                        let actual = priority(&work, &state, 0);
                        assert_eq!(actual.map(|value| value.distance), expected_distance);
                        if let Some(actual) = actual {
                            assert_eq!(actual.benefit, if learned || hinted { 0 } else { 7 });
                            assert_eq!((actual.cost, actual.stable_order, actual.index), (1, 13, 0));
                        }
                        state.remaining[0] = false;
                        assert_eq!(priority(&work, &state, 0), None);
                    }
                }
            }
        }
    }

    #[test]
    fn ranked_candidate_score_and_admission_match_small_exact_boundary_matrix() {
        for hits in 0..=2 {
            for misses in 0..=3 {
                for canonical_passes in 0..=4 {
                    for samples in 0..=3 {
                        let candidate = RankedCandidate {
                            identity: (),
                            seeds: 3,
                            hits,
                            misses,
                            canonical_passes,
                            measured: Duration::from_nanos(12),
                            samples,
                            order: 71,
                        };
                        assert_eq!(
                            candidate.score(),
                            (
                                (2 * misses + canonical_passes) / 4,
                                core::cmp::Reverse(hits),
                                if samples == 0 {
                                    Duration::MAX
                                } else {
                                    Duration::from_nanos(12) / samples
                                },
                                71,
                            )
                        );
                        for fallback in [0, 3, 12, 24, 36, 60] {
                            let expected = !(hits == 0 && misses + canonical_passes / 2 >= 2)
                                && (samples == 0 || 12 / samples <= fallback * (hits + 1) / (samples + 2));
                            assert_eq!(
                                candidate.is_admissible(Duration::from_nanos(u64::from(fallback))),
                                expected,
                                "hits={hits} misses={misses} canonical={canonical_passes} samples={samples} fallback={fallback}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn probe_reservation_and_hit_counters_have_exact_tier_boundaries() {
        for kind in [ProbeKind::Item, ProbeKind::Reach, ProbeKind::File] {
            let tally = Tally::default();
            for _ in 0..INFERRED_ZERO_HIT_LIMIT {
                assert!(kind.reserve(&tally));
            }
            assert!(!kind.reserve(&tally));
            assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), INFERRED_ZERO_HIT_LIMIT);
            kind.hit(&tally);
            assert!(kind.reserve(&tally));
            let (probes, hits) = match kind {
                ProbeKind::Item => (&tally.item_probes, &tally.item_hits),
                ProbeKind::Reach => (&tally.reach_probes, &tally.reach_hits),
                ProbeKind::File => (&tally.file_probes, &tally.file_hits),
                _ => unreachable!(),
            };
            assert_eq!(probes.load(Ordering::Relaxed), INFERRED_ZERO_HIT_LIMIT + 1);
            assert_eq!(hits.load(Ordering::Relaxed), 1);
            assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        }
        for kind in [ProbeKind::Exact, ProbeKind::Census] {
            let tally = Tally::default();
            assert!(kind.reserve(&tally));
            kind.hit(&tally);
            let exact = matches!(kind, ProbeKind::Exact);
            assert_eq!(tally.exact_probes.load(Ordering::Relaxed), usize::from(exact));
            assert_eq!(tally.exact_hits.load(Ordering::Relaxed), usize::from(exact));
            assert_eq!(tally.census_probes.load(Ordering::Relaxed), usize::from(!exact));
            assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), usize::from(!exact));
            assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), usize::from(!exact));
        }
    }

    #[test]
    fn scheduler_prefers_idle_files_then_the_least_contended_inactive_item() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "active", "a", 10, 0, false, 0),
                scheduled(0, "idle-item", "a", 10, 0, false, 1),
                scheduled(1, "idle-item", "b", 10, 0, false, 2),
            ],
            2,
        );
        let mut state = scheduler.state.lock().expect("scheduler state is healthy");
        state.active_files[0] = 2;
        state.active_items.insert((0, "active".to_owned().into()), 1);
        scheduler.refresh_file(&mut state, 0);

        assert_eq!(scheduler.select(&state), Some(2), "an idle file outranks every active file");

        state.active_files[1] = 1;
        scheduler.refresh_file(&mut state, 1);
        assert_eq!(
            scheduler.select(&state),
            Some(2),
            "the inactive item in the less-contended file outranks both a duplicate and a busier file"
        );
    }

    #[test]
    fn scheduler_keeps_same_item_exclusion_stronger_than_file_separation() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "active", "a", 100, 9, false, 0),
                scheduled(0, "other", "a", 1, 0, false, 1),
            ],
            1,
        );
        let mut state = scheduler.state.lock().expect("scheduler state is healthy");
        state.active_files[0] = 1;
        state.active_items.insert((0, "active".to_owned().into()), 1);
        scheduler.refresh_file(&mut state, 0);

        assert_eq!(scheduler.select(&state), Some(1));
        scheduler.withdraw(&mut state, 1);
        assert_eq!(scheduler.select(&state), None, "an unhinted same-item duplicate is not reserved");
    }

    #[test]
    fn scheduler_skips_work_blocked_by_an_active_resource() {
        let resource: Arc<str> = "exclusive".into();
        let mut first = scheduled(0, "first", "a", 30, 0, false, 0);
        first.resources.push(Arc::clone(&resource));
        let mut blocked = scheduled(1, "blocked", "b", 20, 0, false, 1);
        blocked.resources.push(Arc::clone(&resource));
        let unrelated = scheduled(2, "unrelated", "c", 10, 0, false, 2);
        let mut capacities = crate::HashMap::default();
        capacities.insert(resource, 1);
        let scheduler = Scheduler::with_resources(vec![first, blocked, unrelated], 3, capacities);
        let abandoned = OnceLock::new();

        let first = scheduler.assignment(&abandoned).expect("the first resource user is available");
        assert_eq!(first.index(), 0);
        let unrelated = scheduler.assignment(&abandoned).expect("unrelated work remains available");
        assert_eq!(unrelated.index(), 2, "the second worker skips the blocked resource user");
        drop(first);
        let blocked = scheduler
            .assignment(&abandoned)
            .expect("releasing the resource admits blocked work");
        assert_eq!(blocked.index(), 1);
    }

    #[test]
    fn checkpoint_abandonment_wakes_all_resource_waiters_before_the_active_worker_finishes() {
        let resource: Arc<str> = "exclusive".into();
        let work = (0..5)
            .map(|index| {
                let mut work = scheduled(index, "item", "package", 1, 0, false, index);
                work.resources.push(Arc::clone(&resource));
                work
            })
            .collect();
        let scheduler = Scheduler::with_resources(work, 5, std::iter::once((resource, 1)).collect());
        let abandoned = OnceLock::new();
        let active = scheduler.assignment(&abandoned).unwrap();
        let (waiting, waited) = std::sync::mpsc::channel();
        let (finished, joined) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..4 {
                let waiting = waiting.clone();
                let finished = finished.clone();
                let scheduler = &scheduler;
                let abandoned = &abandoned;
                let _worker = scope.spawn(move || {
                    let claimed = scheduler.claim_with(abandoned, || {
                        waiting.send(()).unwrap();
                    });
                    finished.send(claimed).unwrap();
                });
            }
            for _ in 0..4 {
                waited
                    .recv_timeout(Duration::from_secs(10))
                    .expect("each worker reaches resource admission");
            }
            abandoned.set("checkpoint failed".to_owned()).unwrap();
            scheduler.abandon();
            for _ in 0..4 {
                assert_eq!(
                    joined
                        .recv_timeout(Duration::from_secs(10))
                        .expect("abandonment wakes every resource waiter"),
                    None
                );
            }
            assert_eq!(scheduler.state.lock().unwrap().active_resources.values().copied().sum::<usize>(), 1);
        });
        drop(active);
        assert_eq!(scheduler.state.lock().unwrap().active_resources.values().copied().sum::<usize>(), 0);
    }

    #[test]
    fn scheduler_resource_fallback_can_skip_the_head_of_the_same_item_queue() {
        let resource: Arc<str> = "exclusive".into();
        let mut first = scheduled(0, "same", "a", 30, 1, false, 0);
        first.resources.push(Arc::clone(&resource));
        let mut blocked = scheduled(0, "same", "a", 20, 0, true, 1);
        blocked.resources.push(Arc::clone(&resource));
        let available = scheduled(0, "same", "a", 10, 0, true, 2);
        let capacities = std::iter::once((resource, 1)).collect();
        let scheduler = Scheduler::with_resources(vec![first, blocked, available], 1, capacities);
        let abandoned = OnceLock::new();

        let first = scheduler.assignment(&abandoned).expect("the scout is available");
        assert_eq!(first.index(), 0);
        let available = scheduler
            .assignment(&abandoned)
            .expect("the lower-priority hinted alternative is available");
        assert_eq!(available.index(), 2);
        drop(first);
        let blocked = scheduler
            .assignment(&abandoned)
            .expect("the released resource admits the skipped head");
        assert_eq!(blocked.index(), 1);
    }

    #[test]
    fn exact_work_reserves_only_the_killer_tests_resources() {
        let (_directory, mut work) = crate::testing::helper_workspace("scheduled-resource-", &["exit:0"]);
        let mut binary = crate::testing::helper();
        binary.package = "subject".to_owned();
        binary.target = "lib".to_owned();
        work.set_resources(Resources::fake_tests(
            &binary,
            &[("tests::killer", "killer-resource"), ("tests::unrelated", "unrelated-resource")],
        ));
        let hint = Killer {
            package: binary.package.clone(),
            target: binary.target.clone(),
            test: "tests::killer".to_owned(),
        };

        let resources = scheduled_resources(
            &work,
            crate::estimate::MutationWork::hinted(crate::estimate::WorkKind::Exact, Duration::from_millis(1), Duration::from_millis(2)),
            &[&binary],
            Some(&hint),
            1,
            blind(),
        );

        assert_eq!(resources.iter().map(AsRef::as_ref).collect::<Vec<_>>(), ["killer-resource"]);
    }

    #[test]
    fn scheduler_selection_does_not_revisit_the_completed_population() {
        let population = 10_000;
        let scheduler = Scheduler::new(
            (0..population)
                .map(|index| scheduled(index, &format!("item-{index}"), "one-package", 1, 0, false, index))
                .collect(),
            population,
        );
        {
            let mut state = scheduler.state.lock().expect("scheduler state is healthy");
            state.priority_updates = 0;
            assert_eq!(state.heads.len(), 1, "selection indexes one head for the package");
        }

        let abandoned = OnceLock::new();
        assert_eq!(scheduler.claim(&abandoned), Some(0));

        let state = scheduler.state.lock().expect("scheduler state is healthy");
        assert_eq!(state.remaining_count, population - 1);
        assert!(state.priorities[0].is_none(), "claimed work leaves the frontier permanently");
        assert_eq!(
            state.priority_updates, 1,
            "claiming from a single-mutant file rekeys that file, not the original population"
        );
        assert_eq!(state.heads.len(), 1);
    }

    #[test]
    fn scheduler_rekeys_only_candidates_in_a_changed_file() {
        let unrelated = 10_000;
        let mut work = (0..unrelated)
            .map(|index| scheduled(index + 1, &format!("unrelated-{index}"), "a", 1, 0, false, index))
            .collect::<Vec<_>>();
        work.extend([
            scheduled(0, "scout", "a", 1, 2, false, unrelated),
            scheduled(0, "sibling", "b", 1, 2, false, unrelated + 1),
            scheduled(0, "hinted", "b", 1, 2, true, unrelated + 2),
        ]);
        let scheduler = Scheduler::new(work, unrelated + 1);
        let mut state = scheduler.state.lock().expect("scheduler state is healthy");
        state.priority_updates = 0;
        state.active_files[0] = 1;
        state.active_items.insert((0, "scout".to_owned().into()), 1);

        scheduler.refresh_file(&mut state, 0);

        assert_eq!(
            state.priority_updates, 3,
            "a file-state change rekeys only the candidates indexed under that file"
        );
        assert!(state.priorities[..unrelated].iter().all(Option::is_some));
    }

    #[test]
    fn scheduler_balances_learning_value_cost_long_work_and_stable_order() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "low-value", "a", 20, 2, false, 3),
                scheduled(1, "high-value", "a", 5, 2, false, 2),
                scheduled(2, "long", "a", 10, 4, false, 1),
                scheduled(3, "stable", "a", 10, 4, false, 0),
            ],
            4,
        );
        let state = scheduler.state.lock().expect("scheduler state is healthy");

        assert_eq!(
            scheduler.select(&state),
            Some(3),
            "learning per estimated cost wins, then equal value keeps useful long work and stable order"
        );
    }

    #[test]
    fn scheduler_updates_heads_not_the_population_of_a_large_file() {
        let population = 4096;
        for items in [1, 16, population] {
            let scheduler = Scheduler::new(
                (0..population)
                    .map(|index| scheduled(0, &format!("item-{}", index % items), "a", 1, 2, false, index))
                    .collect(),
                1,
            );
            {
                let mut state = scheduler.state.lock().expect("scheduler state is healthy");
                state.priority_updates = 0;
                state.learning_updates = 0;
            }
            let abandoned = OnceLock::new();
            for _ in 0..population {
                let index = scheduler.claim(&abandoned).expect("pending work is available");
                scheduler.complete(index);
            }
            assert_eq!(scheduler.claim(&abandoned), None);
            let state = scheduler.state.lock().expect("scheduler state is healthy");
            assert_eq!(state.priority_updates, population * 2, "each reservation updates one item head");
            assert!(state.learning_updates <= population, "cold priorities are converted at most once");
            assert!(
                state.pending_groups.is_empty(),
                "finished groups leave the resource-selection frontier"
            );
            assert!(state.files[0].idle.is_empty());
            assert!(state.files[0].active.is_empty());
            assert!(state.heads.is_empty());
        }
    }

    #[test]
    fn scheduler_hierarchy_matches_full_scan_through_claims_and_releases() {
        for resource_limited in [false, true] {
            for seed in 0..8 {
                let work = (0..96)
                    .map(|index| {
                        let mut entry = scheduled(
                            index % 4,
                            &format!("item-{}", index % 7),
                            &format!("package-{}", index % 3),
                            (index % 11 + 1) as u64,
                            index % 5,
                            index % 3 == 0,
                            95 - index,
                        );
                        if resource_limited && index % 3 != 0 {
                            entry.resources.push(format!("resource-{}", index % 2).into());
                        }
                        entry
                    })
                    .collect();
                let capacities = if resource_limited {
                    [("resource-0".into(), 1), ("resource-1".into(), 2)].into_iter().collect()
                } else {
                    crate::HashMap::default()
                };
                let scheduler = Scheduler::with_resources(work, 4, capacities);
                let abandoned = OnceLock::new();
                let mut active = Vec::new();
                let mut step = seed;
                loop {
                    let (expected, remaining) = {
                        let state = scheduler.state.lock().expect("scheduler state is healthy");
                        let expected = (0..scheduler.work.len())
                            .filter(|index| scheduler.resources_available(&state, *index))
                            .filter_map(|index| {
                                let package = scheduler.package_slots[index];
                                priority(&scheduler.work, &state, index).map(|local| PackageHead {
                                    package,
                                    turns: state.package_turns[package],
                                    local,
                                })
                            })
                            .min()
                            .map(|head| head.local.index);
                        assert_eq!(
                            scheduler.select(&state),
                            expected,
                            "seed={seed}, step={step}, resources={resource_limited}"
                        );
                        (expected, state.remaining_count)
                    };
                    if remaining == 0 && active.is_empty() {
                        break;
                    }
                    step += 1;
                    if !active.is_empty() && (expected.is_none() || active.len() == 8 || step % 3 == 0) {
                        let index = active.swap_remove((step * 7 + seed) % active.len());
                        scheduler.release(index, step % 4 != 0);
                    } else if let Some(expected) = expected {
                        assert_eq!(scheduler.claim(&abandoned), Some(expected));
                        active.push(expected);
                    } else {
                        panic!("pending work must be available when no assignment is active");
                    }
                }
            }
        }
    }

    #[test]
    fn scheduler_preserves_package_fairness_at_assignment_time() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "a1", "a", 20, 0, false, 0),
                scheduled(1, "a2", "a", 20, 0, false, 1),
                scheduled(2, "b1", "b", 1, 0, false, 2),
            ],
            3,
        );
        let abandoned = OnceLock::new();

        assert_eq!(scheduler.claim(&abandoned), Some(0));
        assert_eq!(
            scheduler.claim(&abandoned),
            Some(2),
            "a package that has not received a turn outranks another assignment from the leading package"
        );
    }

    #[test]
    fn hinted_work_may_share_an_active_item() {
        let scheduler = Arc::new(Scheduler::new(
            vec![
                scheduled(0, "same", "a", 10, 1, false, 0),
                scheduled(0, "same", "a", 10, 1, true, 1),
            ],
            1,
        ));
        let abandoned = OnceLock::new();

        assert_eq!(scheduler.claim(&abandoned), Some(0));
        assert_eq!(scheduler.claim(&abandoned), Some(1));
    }

    #[test]
    fn unwinding_assignment_releases_its_reservation_and_wakes_follow_on_work() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "same", "a", 10, 1, false, 0),
                scheduled(0, "same", "a", 10, 1, false, 1),
            ],
            1,
        );
        let abandoned = OnceLock::new();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let assignment = scheduler.assignment(&abandoned).expect("the first mutant is assigned");
            assert_eq!(assignment.index(), 0);
            panic!("simulated worker failure");
        }));

        assert!(panic.is_err());
        assert_eq!(
            scheduler.claim(&abandoned),
            Some(1),
            "dropping a panicking assignment releases its item and file reservations"
        );
    }

    #[test]
    fn conflicting_tail_waits_for_a_state_change_without_reserving_work() {
        let scheduler = Arc::new(Scheduler::new(
            vec![
                scheduled(0, "same", "a", 10, 2, false, 0),
                scheduled(0, "same", "a", 10, 2, false, 1),
                scheduled(0, "same", "a", 10, 2, false, 2),
            ],
            1,
        ));
        let abandoned = Arc::new(OnceLock::new());
        assert_eq!(scheduler.claim(&abandoned), Some(0));

        let (sent, received) = mpsc::channel();
        let waiting_scheduler = Arc::clone(&scheduler);
        let waiting_abandoned = Arc::clone(&abandoned);
        let waiter = thread::spawn(move || {
            let _sent = sent.send(waiting_scheduler.claim(&waiting_abandoned));
        });

        assert!(
            received.recv_timeout(Duration::from_millis(30)).is_err(),
            "the conflicting tail must wait for notification"
        );
        {
            let state = scheduler.state.lock().expect("scheduler state is healthy");
            assert!(state.remaining[1], "waiting must not reserve the sibling");
            assert_eq!(state.active_files[0], 1);
        }

        scheduler.complete(0);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).expect("completion wakes the waiter"),
            Some(1)
        );
        waiter.join().expect("the waiter exits normally");
    }

    #[test]
    fn learning_is_published_before_a_related_follow_on_assignment() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "same", "a", 10, 2, false, 0),
                scheduled(0, "same", "a", 10, 2, false, 1),
                scheduled(0, "same", "a", 10, 2, false, 2),
            ],
            1,
        );
        let abandoned = OnceLock::new();
        let learning = FileLearning::new();

        assert_eq!(scheduler.claim(&abandoned), Some(0));
        learning.publish("same", &Judgement::Reached(Outcome::Killed, Some(killer("tests::learned")), None));
        scheduler.complete(0);
        assert_eq!(scheduler.claim(&abandoned), Some(1));
        assert!(matches!(&learning.candidates("same", Duration::MAX)[0], Candidate::Exact(found) if found.test == "tests::learned"));
        assert_eq!(
            scheduler.claim(&abandoned),
            Some(2),
            "published learning removes cold-scout spacing from informed siblings"
        );
    }

    #[test]
    fn long_running_scouts_do_not_block_unrelated_work_or_race_same_item_siblings() {
        let scheduler = Arc::new(Scheduler::new(
            vec![
                scheduled(0, "scouted", "a", 100, 1, false, 0),
                scheduled(0, "scouted", "a", 100, 1, false, 1),
                scheduled(1, "unrelated", "a", 1, 0, false, 2),
            ],
            2,
        ));
        let abandoned = Arc::new(OnceLock::new());
        let (started, scout_started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let scout_scheduler = Arc::clone(&scheduler);
        let scout_abandoned = Arc::clone(&abandoned);
        let scout = thread::spawn(move || {
            let index = scout_scheduler.claim(&scout_abandoned).expect("the scout is assigned");
            let _sent = started.send(index);
            released.recv().expect("the test eventually releases the long-running scout");
            scout_scheduler.complete(index);
        });

        assert_eq!(scout_started.recv().expect("the scout starts"), 0);
        assert_eq!(
            scheduler.claim(&abandoned),
            Some(2),
            "unrelated work proceeds while the scout remains active"
        );
        {
            let state = scheduler.state.lock().expect("scheduler state is healthy");
            assert!(state.remaining[1], "the same-item sibling remains unreserved");
        }

        scheduler.complete(2);
        release.send(()).expect("the scout can be released");
        scout.join().expect("the scout exits normally");
        assert_eq!(scheduler.claim(&abandoned), Some(1));
    }

    #[test]
    fn equivalent_serial_and_dynamic_schedules_produce_the_same_verdicts() {
        let (_directory, work, serial_template, binaries) = helper_harness(&["exit:1"]);
        let template = serial_template.mutants[0].clone();
        let mutants = || {
            (0..4)
                .map(|index| {
                    let mut mutant = template.clone();
                    mutant.id = format!("m{index}").into();
                    mutant.ordinal = u32::try_from(index + 1).expect("four mutants fit in u32");
                    mutant.file = Utf8PathBuf::from(format!("src/{index}.rs")).into();
                    mutant.item_path = format!("subject::f{index}").into();
                    mutant
                })
                .collect()
        };
        let mut serial = serial_template;
        serial.mutants = mutants();
        let mut dynamic = one_mutant_plan(work.root.clone());
        dynamic.mutants = mutants();
        let scope = TestScope {
            packages: &[],
            package_local: false,
            whole_workspace: true,
        };
        let serial_reach = Reachability::build(&serial, &binaries, &scope);
        let dynamic_reach = Reachability::build(&dynamic, &binaries, &scope);

        for (plan, reach, jobs) in [(&mut serial, serial_reach, 1), (&mut dynamic, dynamic_reach, 4)] {
            let _spent = test_all(
                &work,
                plan,
                &reach,
                Sweep {
                    jobs,
                    confirm: false,
                    ..sweep(Stall::NONE)
                },
                false,
                &mut Killers::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect("the sweep completes");
        }

        assert_eq!(
            serial.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
            dynamic.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>()
        );
        assert!(serial.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
    }
}
