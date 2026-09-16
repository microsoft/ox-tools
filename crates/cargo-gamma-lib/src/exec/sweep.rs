// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running the suite once per mutant and turning each result into a verdict.

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::thread;
use std::time::Instant;

use camino::Utf8Path;
use cargo_gamma_process::MemoryRequest;

use super::census::{Census, CensusSelection, CensusWork};
use super::events::Events;
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
use crate::model::Outcome;
use crate::report::{encode_controls, encode_preserving_color};
use crate::{Result, notes};

/// The minimum launch-cost estimate used by census admission.
///
/// Kept here for the existing shared cost model; sweep workers do not wait for this duration.
pub(super) const MIN_SCOUT_WAIT: Duration = Duration::from_millis(5);

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

/// One mutant's result: its index in the plan, what happened, how long it took and any detail.
type Completed = (usize, Outcome, u64, Option<Killer>, Option<String>);

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

    // A mutant with a persisted killer hint is expected to be killed by one probe.
    if let Some(hint) = killers.hint(&mutant.id)
        && let Some(binaries) = reach.reachable(mutant)
        && let Some(binary) = binaries.iter().find(|b| hint.names(&b.package, &b.target))
    {
        return binary.baseline;
    }

    let Some(binaries) = reach.reachable(mutant) else {
        return Duration::ZERO;
    };

    let mut total = Duration::ZERO;

    for binary in binaries {
        let ordinal = mutant.ordinal;
        match census.work(binary, ordinal) {
            CensusWork::Selected(duration) => total += duration,
            CensusWork::Whole => total += binary.baseline,
            CensusWork::Uncovered => {}
            CensusWork::Hinted(_duration) => total += binary.baseline,
        }
    }

    total
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
}

#[derive(Debug)]
struct SchedulerState {
    remaining: Vec<bool>,
    active_files: Vec<usize>,
    active_items: crate::HashMap<(usize, Arc<str>), usize>,
    learned_items: crate::HashSet<(usize, Arc<str>)>,
    package_turns: crate::HashMap<Arc<str>, usize>,
}

struct Scheduler {
    work: Vec<ScheduledWork>,
    state: Mutex<SchedulerState>,
    changed: Condvar,
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
    fn drop(&mut self) {
        self.scheduler.release(self.index, self.learned);
    }
}

impl Scheduler {
    fn new(work: Vec<ScheduledWork>, files: usize) -> Self {
        let remaining = vec![true; work.len()];
        Self {
            work,
            state: Mutex::new(SchedulerState {
                remaining,
                active_files: vec![0; files],
                active_items: crate::HashMap::default(),
                learned_items: crate::HashSet::default(),
                package_turns: crate::HashMap::default(),
            }),
            changed: Condvar::new(),
        }
    }

    fn claim(&self, abandoned: &OnceLock<String>) -> Option<usize> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        loop {
            if abandoned.get().is_some() {
                return None;
            }

            if let Some(index) = self.select(&state) {
                let work = &self.work[index];
                state.remaining[index] = false;
                state.active_files[work.file] = state.active_files[work.file].saturating_add(1);
                let active_item = state.active_items.entry((work.file, Arc::clone(&work.item))).or_default();
                *active_item = active_item.saturating_add(1);
                let turns = state.package_turns.entry(Arc::clone(&work.package)).or_default();
                *turns = turns.saturating_add(1);
                return Some(index);
            }

            if !state.remaining.iter().any(|remaining| *remaining) {
                return None;
            }

            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn assignment<'a>(&'a self, abandoned: &OnceLock<String>) -> Option<Assignment<'a>> {
        self.claim(abandoned).map(|index| Assignment {
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
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (work.file, Arc::clone(&work.item));
        if learned {
            let _inserted = state.learned_items.insert(key.clone());
        }
        state.active_files[work.file] = state.active_files[work.file].saturating_sub(1);
        if let Some(active) = state.active_items.get_mut(&key) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_items.remove(&key);
            }
        }
        core::mem::drop(state);
        self.changed.notify_all();
    }

    fn abandon(&self) {
        self.changed.notify_all();
    }

    fn select(&self, state: &SchedulerState) -> Option<usize> {
        let mut selected = None;

        for (index, remaining) in state.remaining.iter().copied().enumerate() {
            if !remaining || self.distance(state, index).is_none() {
                continue;
            }

            if selected.is_none_or(|current| self.precedes(state, index, current)) {
                selected = Some(index);
            }
        }

        selected
    }

    fn distance(&self, state: &SchedulerState, index: usize) -> Option<(u8, usize)> {
        let work = &self.work[index];
        let file_contention = state.active_files[work.file];
        if file_contention == 0 {
            return Some((0, 0));
        }

        let item_contention = state.active_items.get(&(work.file, Arc::clone(&work.item))).copied().unwrap_or(0);
        if item_contention == 0 {
            return Some((1, file_contention));
        }

        // The deterministic tail policy leaves capacity idle while an unhinted same-item scout is
        // active. Hinted work is already informed and may share an item without waiting.
        (work.hinted || state.learned_items.contains(&(work.file, Arc::clone(&work.item)))).then_some((2, file_contention))
    }

    fn precedes(&self, state: &SchedulerState, left: usize, right: usize) -> bool {
        let left_work = &self.work[left];
        let right_work = &self.work[right];
        let left_distance = self.distance(state, left).expect("selection considers only eligible work");
        let right_distance = self.distance(state, right).expect("selection considers only eligible work");
        let left_turns = state.package_turns.get(&left_work.package).copied().unwrap_or(0);
        let right_turns = state.package_turns.get(&right_work.package).copied().unwrap_or(0);

        left_distance
            .cmp(&right_distance)
            .then_with(|| left_turns.cmp(&right_turns))
            .then_with(|| {
                learning_value_cmp(
                    left_work,
                    right_work,
                    state.learned_items.contains(&(left_work.file, Arc::clone(&left_work.item))),
                    state.learned_items.contains(&(right_work.file, Arc::clone(&right_work.item))),
                )
            })
            .then_with(|| right_work.cost.cmp(&left_work.cost))
            .then_with(|| left_work.stable_order.cmp(&right_work.stable_order))
            .is_lt()
    }
}

fn learning_value_cmp(left: &ScheduledWork, right: &ScheduledWork, left_learned: bool, right_learned: bool) -> core::cmp::Ordering {
    let left_cost = left.cost.as_nanos().max(1);
    let right_cost = right.cost.as_nanos().max(1);
    let left_benefit = usize::from(!left_learned && !left.hinted).saturating_mul(left.sibling_benefit);
    let right_benefit = usize::from(!right_learned && !right.hinted).saturating_mul(right.sibling_benefit);
    let left_value = (left_benefit as u128).saturating_mul(right_cost);
    let right_value = (right_benefit as u128).saturating_mul(left_cost);

    right_value.cmp(&left_value)
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
    generalized_probes: AtomicUsize,
    generalized_hits: AtomicUsize,

    /// How many canonical binary launches reach learning proved unnecessary.
    saved: AtomicUsize,

    /// How many of those savings came specifically from sweep-derived reach evidence.
    reach_saved: AtomicUsize,
}

/// What a finished sweep spent, read off its [`Tally`] once every worker has stopped.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Spent {
    /// How many test-binary subprocesses the sweep launched in total.
    pub(super) launches: usize,

    /// How many of those launches were hint-directed probes.
    pub(super) probes: usize,
    pub(super) exact_probes: usize,
    pub(super) exact_hits: usize,
    pub(super) generalized_probes: usize,
    pub(super) generalized_hits: usize,

    /// How many canonical binary launches reach learning avoided.
    pub(super) saved: usize,

    /// How many launches were saved specifically by sweep-derived reach evidence.
    pub(super) reach_saved: usize,
}

#[derive(Default)]
struct NegativeLearning {
    binaries: Mutex<crate::HashSet<(SiteIdentity, BinaryIdentity)>>,
}

impl NegativeLearning {
    fn contains(&self, site: &SiteIdentity, binary: &TestBinary) -> bool {
        self.binaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&(site.clone(), BinaryIdentity::from_binary(binary)))
    }

    fn record(&self, site: &SiteIdentity, binary: &TestBinary) {
        let _inserted = self
            .binaries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((site.clone(), BinaryIdentity::from_binary(binary)));
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
#[expect(clippy::too_many_lines, reason = "sweep setup and its scoped worker loop share borrowed state")]
pub(super) fn test_all(
    work: &Workspace,
    plan: &mut Plan,
    reach: &Reachability<'_>,
    sweep: Sweep<'_>,
    deterministic_reach: bool,
    killers: &mut Killers,
    events: &mut impl Events,
) -> Result<Option<Spent>> {
    let jobs = sweep.jobs;

    let mut pending = pending_positions(plan);

    if pending.is_empty() {
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
    let costs: Vec<Duration> = pending
        .iter()
        .map(|position| mutant_cost(*position, plan, reach, sweep.census, killers))
        .collect();
    let mut file_paths = files.into_iter().collect::<Vec<_>>();
    // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
    file_paths.sort_by_key(|(_file, slot)| *slot);
    let file_paths = file_paths.into_iter().map(|(file, _slot)| file).collect::<Vec<_>>();
    let file_killers: Vec<FileLearning> = file_paths
        .iter()
        .map(|file| FileLearning::from_hints(file, killers.generalized()))
        .collect();

    let (sender, receiver) = mpsc::channel::<Completed>();

    // Resolved up front for the same reason reachability is: a worker must need nothing from the
    // plan, so the calling thread can borrow it mutably and record verdicts as they arrive. Cloned
    // rather than borrowed because the same map is written back as those verdicts land.
    let hints: Vec<Option<Killer>> = pending
        .iter()
        .map(|position| killers.hint(&plan.mutants[*position].id).cloned())
        .collect();
    let reach_hints: Vec<Vec<Candidate>> = pending
        .iter()
        .map(|position| {
            bounded_reach_hints(
                killers.reach_hints(&SiteIdentity::from_mutant(&plan.mutants[*position])),
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
    let scheduler = Scheduler::new(
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
            })
            .collect(),
        file_paths.len(),
    );
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

            let _handle = scope.spawn(move || {
                let _notes = notes::enter(notes.as_ref());

                while abandoned.get().is_none() {
                    let Some(assignment) = scheduler.assignment(abandoned) else {
                        return;
                    };
                    let index = assignment.index();
                    let position = pending[index];

                    let (ordinal, timeout_multiplier) = ordinals[index];
                    let active = ordinal;
                    let started = Instant::now();
                    let reachable = &reachable[index];
                    let judged = judge_learning(
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
                    );

                    let (outcome, killer, note) = match judged {
                        Judgement::Reached(outcome, killer, note) => (outcome, killer, note),
                        Judgement::Abandoned(reason) => {
                            let _first = abandoned.set(reason);
                            core::mem::drop(assignment);
                            scheduler.abandon();

                            return;
                        }
                    };

                    let elapsed = elapsed_millis(started.elapsed());
                    assignment.complete();

                    // A closed receiver means the calling thread is gone, which cannot happen while
                    // the scope is open; there is nothing useful to do about it either way.
                    let _sent = sender.send((position, outcome, elapsed, killer, note));
                }
            });
        }

        // The workers hold the only remaining senders, so the drain ends when the last one finishes.
        // #[gamma::skip(stmt.delete_call, reason = "the receiver waits for channel closure, so retaining the coordinator sender blocks collection forever")]
        core::mem::drop(sender);

        for completed in receiver {
            publish_completed(plan, killers, events, completed);
        }
    });

    let mut generalized = killers.generalized().clone();
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    persist_learning(&mut generalized, &file_paths, &file_killers);
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    killers.replace_generalized(generalized);

    abandoned.into_inner().map_or_else(
        || {
            Ok(Some(Spent {
                launches: tally.launches.into_inner(),
                probes: tally.probes.into_inner(),
                exact_probes: tally.exact_probes.into_inner(),
                exact_hits: tally.exact_hits.into_inner(),
                generalized_probes: tally.generalized_probes.into_inner(),
                generalized_hits: tally.generalized_hits.into_inner(),
                saved: tally.saved.into_inner(),
                reach_saved: tally.reach_saved.into_inner(),
            }))
        },
        |reason| {
            Err(error!(
                "the run cannot be trusted to judge anything further: {reason}.\n\
                 It stops here rather than reach verdicts that would each have to be taken on faith."
            ))
        },
    )
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

fn worker_count(jobs: usize) -> usize {
    jobs.max(1)
}

fn elapsed_millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn publish_completed(
    plan: &mut Plan,
    killers: &mut Killers,
    events: &mut impl Events,
    (position, outcome, elapsed, killer, note): Completed,
) {
    let Some(mutant) = plan.mutants.get_mut(position) else {
        return;
    };
    mutant.outcome = outcome;
    mutant.elapsed_ms = elapsed;
    mutant.killed_by = killer.as_ref().map(|killer| killer.test.clone());
    mutant.note = note;

    match killer {
        Some(killer) => killers.record(mutant.id.clone(), killer),
        None => killers.forget(&mutant.id),
    }

    events.mutant(mutant);
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
enum Judgement {
    /// The mutant was judged: an outcome, the test that caught it if one did, and any note.
    Reached(Outcome, Option<Killer>, Option<String>),

    /// The run could no longer be metered as asked, and no verdict from here on would mean anything.
    Abandoned(String),
}

#[derive(Clone, Copy)]
enum ProbeKind {
    Exact,
    Generalized,
}

impl ProbeKind {
    fn attempt(self, tally: &Tally) {
        let counter = match self {
            Self::Exact => &tally.exact_probes,
            Self::Generalized => &tally.generalized_probes,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
    }

    fn hit(self, tally: &Tally) {
        let counter = match self {
            Self::Exact => &tally.exact_hits,
            Self::Generalized => &tally.generalized_hits,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Runs the one test that caught this mutant last time, and says whether it caught it again.
///
/// A re-killed mutant's cost falls from a partial binary — every test ahead of its killer, in
/// whatever order the harness runs them — to a single test.
///
/// Only a failure is believed. Every other verdict a probe can reach is discarded and the ordinary
/// binary run proceeds unchanged. The caller reaches the probe in canonical binary order and only
/// permits it when filtering cannot bypass another outcome from that same binary.
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
) -> Option<Killer> {
    let request = MemoryRequest {
        meter: sweep.meter,
        limit: binary.memory,
    };

    let attempt = Attempt {
        active: Some(ordinal),
        timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
        stall: sweep.stall,
        request,
        only: Only::One(&hint.test),
        census: None,
    };

    // A probe is one subprocess launched because a hint pointed at it, so it counts as both.
    record_launch(tally);
    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);
    kind.attempt(tally);

    let verdict = run_binary(work, binary, attempt, sweep.confirm);

    match verdict {
        // The harness names the test it ran, which under a filter can only be the one asked for;
        // the recorded name is used when it names nothing, so the map stays populated either way.
        Verdict::Failed(named) => {
            kind.hit(tally);
            Some(Killer {
                package: hint.package.clone(),
                target: hint.target.clone(),
                test: named.unwrap_or_else(|| hint.test.clone()),
            })
        }
        _inconclusive => None,
    }
}

/// Whether a filtered failure is conclusive for this binary.
///
/// Filtering changes the workload, so it cannot settle a kill when the whole binary might instead
/// time out, stall, exceed a memory limit, fail confirmation, or lose its meter. Launch refusal is
/// covered by the successful filtered launch itself. Earlier binaries are protected by trying the
/// probe only when canonical iteration reaches this binary.
fn filtered_kill_is_final(binary: &TestBinary, timeout_multiplier: Option<f64>, sweep: Sweep<'_>) -> bool {
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

fn bounded_reach_hints(hints: Vec<Killer>, reachable: &[&TestBinary], ordinal: u32, census: &Census) -> Vec<Killer> {
    let all = hints;

    all.iter()
        .filter(|hint| {
            let Some(binary) = reachable.iter().copied().find(|binary| hint.names(&binary.package, &binary.target)) else {
                return false;
            };
            let Some(total) = binary.tests else {
                return false;
            };
            let measured = match census.work(binary, ordinal) {
                CensusWork::Selected(duration) | CensusWork::Hinted(duration) => duration,
                CensusWork::Whole | CensusWork::Uncovered => return false,
            };
            if binary.baseline.is_zero() || measured.cmp(&binary.baseline).is_ge() {
                return false;
            }
            let selected = all
                .iter()
                .filter(|candidate| candidate.names(&binary.package, &binary.target))
                .count();

            selected <= total / 2
        })
        .cloned()
        .collect()
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
) -> Option<Killer> {
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

    record_launch(tally);
    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);
    ProbeKind::Generalized.attempt(tally);

    match run_binary(work, binary, attempt, sweep.confirm) {
        Verdict::Failed(name) => {
            record_generalized_hit(tally);
            Some(Killer {
                package: binary.package.clone(),
                target: binary.target.clone(),
                test: name.unwrap_or_else(|| {
                    names
                        .first()
                        .copied()
                        .expect("an incomplete census hint always names at least one test")
                        .to_owned()
                }),
            })
        }
        _inconclusive => None,
    }
}

/// Runs one mutant against every test binary that can reach it, stopping at the first detection.
///
/// Later binaries are not run once one has caught the mutant: the answer cannot change, and the
/// time saved is the difference between a sweep that finishes overnight and one that does not.
///
/// When an earlier run recorded which test caught this mutant, that one test is tried when canonical
/// iteration reaches its binary and filtering cannot change the binary's outcome. It is a guess and
/// it is checked, never believed: see [`probe`].
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
    let active = ordinal;
    let mut exact_hits: Vec<Option<Killer>> = vec![None; reachable.len()];
    // #[gamma::skip(all, reason = "the value controls scheduling, accounting, identity, or a conservative bound whose one-step perturbation has no safely deterministic external observation here")]
    let mut binary_runs: Vec<Option<(Verdict, bool)>> = core::iter::repeat_with(|| None).take(reachable.len()).collect();

    // Learned candidates are tried in ranked order, but their verdict is held until canonical
    // iteration reaches that binary. This gets the useful launch under way first without allowing
    // a later kill to bypass an earlier timeout, resource result, flake, or lost meter.
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    if hint.is_none() {
        'candidate: for candidate in candidates {
            match candidate {
                Candidate::Exact(candidate_hint) => {
                    let Some((index, binary)) = reachable
                        .iter()
                        .copied()
                        .enumerate()
                        .find(|(_index, binary)| candidate_hint.names(&binary.package, &binary.target))
                    else {
                        continue 'candidate;
                    };

                    if census_excludes(sweep.census, binary, active) {
                        continue 'candidate;
                    }

                    if is_negatively_excluded(negative, binary, timeout_multiplier, sweep) {
                        continue 'candidate;
                    }

                    if !filtered_kill_is_final(binary, timeout_multiplier, sweep) {
                        continue 'candidate;
                    }

                    let started = Instant::now();
                    let found = probe(
                        work,
                        active,
                        binary,
                        candidate_hint,
                        timeout_multiplier,
                        sweep,
                        tally,
                        ProbeKind::Generalized,
                    );
                    let elapsed = started.elapsed();

                    if let Some((observed, item_path)) = learning {
                        FileLearning::observe(observed, item_path, candidate, found.is_some(), elapsed);
                    }

                    if let Some(killer) = found {
                        exact_hits[index] = Some(killer);
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

                    if is_negatively_excluded(negative, binary, timeout_multiplier, sweep) {
                        continue 'candidate;
                    }

                    if binary_runs[index].as_ref().is_some() {
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
                    ProbeKind::Generalized.attempt(tally);
                    let run = run_binary_observed(work, binary, attempt, sweep.confirm);
                    let convicted = matches!(run.verdict, Verdict::Failed(_));
                    if convicted {
                        record_generalized_hit(tally);
                    }

                    if let Some((observed, item_path)) = learning {
                        FileLearning::observe(observed, item_path, candidate, convicted, started.elapsed());
                        if matches!(run.reach, ReachObservation::Reached) {
                            FileLearning::reached(observed, item_path, binary, started.elapsed());
                        }
                    }
                    if negative_reach_is_final(&run, only, negative.is_some_and(|(_, _, deterministic)| deterministic))
                        && let Some((negative, site, _)) = negative
                    {
                        NegativeLearning::record(negative, site, binary);
                    }

                    let terminal = !matches!(&run.verdict, Verdict::Passed);
                    binary_runs[index] = Some((run.verdict, matches!(only, Only::All)));

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

        if is_negatively_excluded(negative, binary, timeout_multiplier, sweep) {
            let _saved = tally.saved.fetch_add(1, Ordering::Relaxed);
            let _reach_saved = tally.reach_saved.fetch_add(1, Ordering::Relaxed);
            continue 'binary;
        }

        // A hint naming another binary is stale or outside the selected package set. Waiting until
        // canonical iteration reaches the named binary prevents its kill from bypassing any
        // earlier Flaky, Pending, resource, or metering outcome.
        let killer_hint = hint.filter(|hint| hint.names(&binary.package, &binary.target));

        if let Some(hint) = killer_hint
            && filtered_kill_is_final(binary, timeout_multiplier, sweep)
            && let Some(killer) = probe(work, active, binary, hint, timeout_multiplier, sweep, tally, ProbeKind::Exact)
        {
            return killed_by(killer);
        }

        if let Some(killer) = exact_hits[binary_index].take() {
            return killed_by(killer);
        }

        if let CensusSelection::Hinted(names) = &selection
            && filtered_kill_is_final(binary, timeout_multiplier, sweep)
            && let Some(killer) = probe_cases(work, active, binary, names, timeout_multiplier, sweep, tally)
        {
            return killed_by(killer);
        }

        let only = match &selection {
            CensusSelection::Whole | CensusSelection::Hinted(_) => Only::All,
            CensusSelection::Uncovered => continue 'binary,
            CensusSelection::Selected(names) => Only::These(names),
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
            if matches!(run.reach, ReachObservation::Reached)
                && let Some((observed, item_path)) = learning
            {
                FileLearning::reached(observed, item_path, binary, started.elapsed());
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
            if matches!(run.reach, ReachObservation::Reached)
                && let Some((observed, item_path)) = learning
            {
                FileLearning::reached(observed, item_path, binary, started.elapsed());
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
                target: binary.target.clone(),
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

fn negative_excludes(
    negative: Option<(&NegativeLearning, &SiteIdentity, bool)>,
    binary: &TestBinary,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
) -> bool {
    let Some((negative, site, true)) = negative else {
        return false;
    };

    negative.contains(site, binary) && filtered_kill_is_final(binary, timeout_multiplier, sweep)
}

fn killed_by(killer: Killer) -> Judgement {
    Judgement::Reached(Outcome::Killed, Some(killer), None)
}

fn census_excludes(census: &Census, binary: &TestBinary, ordinal: u32) -> bool {
    matches!(census.selection(binary, ordinal), CensusSelection::Uncovered)
}

fn is_negatively_excluded(
    negative: Option<(&NegativeLearning, &SiteIdentity, bool)>,
    binary: &TestBinary,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
) -> bool {
    negative_excludes(negative, binary, timeout_multiplier, sweep)
}

fn record_generalized_hit(tally: &Tally) {
    ProbeKind::hit(ProbeKind::Generalized, tally);
}

fn record_launch(tally: &Tally) {
    let _previous = tally.launches.fetch_add(1, Ordering::Relaxed);
}

fn whole_attempt(attempt: Attempt<'_>) -> Attempt<'_> {
    let mut whole = attempt;
    whole.only = Only::All;
    whole
}

fn filtered_run_needs_confirmation(already_whole: bool, selection: &CensusSelection<'_>, verdict: &Verdict) -> bool {
    !already_whole && matches!(selection, CensusSelection::Selected(_)) && !matches!(verdict, Verdict::Passed)
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
    if let Some(hint) = hint {
        let judged = judge_ranked(
            work,
            active,
            reachable,
            Some(hint),
            &[],
            Some((observed, item_path)),
            Some((negative, site, deterministic_reach)),
            timeout_multiplier,
            sweep,
            tally,
        );

        FileLearning::publish(observed, item_path, &judged);

        return judged;
    }

    let mut candidates = promoted.to_vec();
    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    candidates.extend(FileLearning::candidates(observed, item_path));
    let judged = judge_ranked(
        work,
        active,
        reachable,
        None,
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BinaryIdentity {
    package: String,
    target: String,
}

impl BinaryIdentity {
    fn from_binary(binary: &TestBinary) -> Self {
        Self {
            package: binary.package.clone(),
            target: binary.target.clone(),
        }
    }

    fn from_killer(killer: &Killer) -> Self {
        Self {
            package: killer.package.clone(),
            target: killer.target.clone(),
        }
    }

    fn names(&self, binary: &TestBinary) -> bool {
        self.package == binary.package && self.target == binary.target
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
    hits: u32,
    misses: u32,
    measured: Duration,
    samples: u32,
    order: u64,
}

impl<T> RankedCandidate<T> {
    fn score(&self) -> (u32, core::cmp::Reverse<u32>, Duration, u64) {
        (
            self.misses / 2,
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
}

impl<T: Clone> RankedCandidate<T> {
    fn from_hint<U>(hint: &RankedHint<U>, identity: impl FnOnce(&U) -> T) -> Self {
        Self {
            identity: identity(&hint.candidate),
            hits: hint.hits,
            misses: hint.misses,
            measured: Duration::from_millis(hint.measured_ms),
            samples: hint.samples,
            order: hint.order,
        }
    }

    fn hint<U>(&self, candidate: impl FnOnce(&T) -> U) -> RankedHint<U> {
        RankedHint {
            candidate: candidate(&self.identity),
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

    fn candidates(&self, item_path: &str) -> Vec<Candidate> {
        let state = self.locked();
        let mut exact = state.items.get(item_path).map_or_else(Vec::new, |item| item.exact.iter().collect());
        let mut reached = state
            .items
            .get(item_path)
            .map_or_else(Vec::new, |item| item.reached.iter().collect());
        let mut reached_file: Vec<_> = state.reached_file.iter().collect();
        let mut binaries: Vec<_> = state.binaries.iter().collect();
        exact.sort_by_key(|candidate| candidate.score());
        // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
        reached.sort_by_key(|candidate| candidate.score());
        // #[gamma::skip(all, reason = "the ordering or deduplication is retained for deterministic, efficient behavior; the current internal consumer observes the same population")]
        reached_file.sort_by_key(|candidate| candidate.score());
        binaries.sort_by_key(|candidate| candidate.score());
        reached_file.retain(|candidate| !contains_identity(&reached, &candidate.identity));
        // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
        binaries.retain(|candidate| !contains_identity(&reached, &candidate.identity));

        exact
            .into_iter()
            .map(|candidate| Candidate::Exact(candidate.identity.clone()))
            .chain(
                reached
                    .into_iter()
                    .map(|candidate| Candidate::ItemBinary(candidate.identity.clone())),
            )
            .chain(
                reached_file
                    .into_iter()
                    .map(|candidate| Candidate::ReachFileBinary(candidate.identity.clone())),
            )
            .chain(
                binaries
                    .into_iter()
                    .map(|candidate| Candidate::FileBinary(candidate.identity.clone())),
            )
            .collect()
    }

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
        let Judgement::Reached(_outcome, Some(killer), _note) = judged else {
            return;
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

fn contains_identity(candidates: &[&RankedCandidate<BinaryIdentity>], identity: &BinaryIdentity) -> bool {
    candidates.iter().any(|candidate| same_identity(&candidate.identity, identity))
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
        hits: u32::from(true),
        misses: u32::from(false),
        measured: elapsed,
        samples: u32::from(true),
        order,
    }
}

fn published_candidate<T>(identity: T, order: u64) -> RankedCandidate<T> {
    RankedCandidate {
        identity,
        hits: u32::from(true),
        misses: u32::from(false),
        measured: Duration::ZERO,
        samples: u32::from(false),
        order,
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use core::iter::once;
    #[cfg(unix)]
    use std::fs;

    use camino::Utf8PathBuf;

    use super::*;
    #[cfg(unix)]
    use crate::exec::faults::{self, Fault};
    #[cfg(unix)]
    use crate::exec::memory;
    use crate::ops::collect::Shape;

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
    fn a_grouped_census_probe_attributes_an_unnamed_failure_to_its_first_selected_case() {
        let (_directory, work) = crate::testing::helper_workspace("grouped-probe-", &["exit:1"]);
        let binary = crate::testing::helper();
        let tally = Tally::default();
        let names = ["tests::first", "tests::second"];

        let killer = probe_cases(
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
        )
        .expect("the grouped probe fails");

        assert_eq!(killer.test, "tests::first");
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.generalized_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.generalized_hits.load(Ordering::Relaxed), 1);
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
            outcome: Outcome::Pending,
            suppression: None,
            expectation: None,
            test_timeout_multiplier: None,
            elapsed_ms: 0,
            killed_by: None,
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
    fn file_local_killer_is_probed_only_after_earlier_binaries() {
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
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep should finish")
        .expect("two mutants should be swept");

        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
        assert_eq!(
            spent.launches, 5,
            "both mutants must run the earlier binary before the learned hint is checked"
        );
        assert_eq!(
            spent.probes, 2,
            "the learned killer is checked only after the earlier binary, then confirmed with the whole binary"
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
        assert_eq!(spent.launches, 1);
        assert_eq!(spent.probes, 0);
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
    }

    /// A run with nothing to sweep reports the sweep as absent, not as a zero-cost phase.
    ///
    /// `test_all` says so by returning `None`, which the session stores verbatim so the diagnostics
    /// and `--estimate` can tell "there was nothing to sweep" from "the sweep ran and was free".
    #[test]
    fn a_plan_with_no_pending_mutants_sweeps_nothing_and_returns_none() {
        let (_directory, work, mut plan, binaries) = unreachable_harness(Vec::new());

        // No pending mutant of any kind, which is what makes the sweep absent rather than empty.
        plan.mutants.clear();
        let reach = Reachability::build(&plan, &binaries, &NARROW);

        let swept = test_all(
            &work,
            &mut plan,
            &reach,
            sweep(Stall::NONE),
            false,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("an empty sweep is not a failure");

        assert!(swept.is_none(), "nothing pending means there is no sweep phase, not a free one");
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
            Judgement::Reached(..) => panic!("expected the mutant to be abandoned rather than judged unprotected"),
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
    fn a_cached_killer_cannot_bypass_the_canonical_timeout() {
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

        assert!(matches!(judgement, Judgement::Reached(Outcome::Timeout, None, None)));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_partial_census_probe_cannot_bypass_the_whole_binary_timeout() {
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

        assert!(matches!(judgement, Judgement::Reached(Outcome::Timeout, None, None)));
        assert_eq!(tally.probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_group_census_hint_is_disabled_by_every_canonical_precedence_policy() {
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

        assert!(filtered_kill_is_final(&binary, None, base));

        binary.budget = Some(Duration::from_secs(1));
        assert!(!filtered_kill_is_final(&binary, None, base), "timeout policy must run canonically");
        binary.budget = None;

        assert!(
            !filtered_kill_is_final(
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
            !filtered_kill_is_final(&binary, None, Sweep { meter: true, ..base }),
            "memory limits and metering failures must run canonically"
        );
        assert!(
            !filtered_kill_is_final(&binary, None, Sweep { confirm: true, ..base }),
            "confirmation flakes must run canonically"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_complete_census_failure_is_checked_against_the_whole_binary() {
        const SCRIPT: &[&str] = &[
            "when-arg:tests::t0|print:test tests::t0 ... FAILED",
            "when-arg:tests::t0|exit:1",
            "sleep:200",
            "exit:0",
        ];

        let (_directory, work, plan, mut binaries) = helper_harness(SCRIPT);
        binaries[0].budget = Some(Duration::from_millis(20));
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

        assert!(matches!(judgement, Judgement::Reached(Outcome::Timeout, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
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
        assert_eq!(tally.probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
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
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(unix)]
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

        // Without census data, both get the same cost (binary baseline sum).
        let cost_a = mutant_cost(0, &plan, &sets, blind(), &Killers::default());
        let cost_b = mutant_cost(1, &plan, &sets, blind(), &Killers::default());
        assert_eq!(cost_a, cost_b);
        assert_eq!(cost_a, Duration::from_secs(10));
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
            binaries[0].baseline,
            "partial census evidence remains a whole-binary scheduling cost"
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
            (0, Outcome::Killed, 23, Some(killer.clone()), Some("detail".to_owned())),
        );
        assert_eq!(plan.mutants[0].outcome, Outcome::Killed);
        assert_eq!(plan.mutants[0].elapsed_ms, 23);
        assert_eq!(plan.mutants[0].killed_by.as_deref(), Some("tests::caught"));
        assert_eq!(plan.mutants[0].note.as_deref(), Some("detail"));
        assert_eq!(killers.hint(&plan.mutants[0].id), Some(&killer));
        assert_eq!(events.mutants, 1);

        publish_completed(&mut plan, &mut killers, &mut events, (0, Outcome::Survived, 29, None, None));
        assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
        assert_eq!(plan.mutants[0].elapsed_ms, 29);
        assert_eq!(plan.mutants[0].killed_by, None);
        assert_eq!(plan.mutants[0].note, None);
        assert!(killers.hint(&plan.mutants[0].id).is_none());
        assert_eq!(events.mutants, 2);

        publish_completed(
            &mut plan,
            &mut killers,
            &mut events,
            (usize::MAX, Outcome::Killed, 0, Some(killer), None),
        );
        assert_eq!(events.mutants, 2, "an obsolete queue position publishes nothing");
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

        let same = learning.candidates("subject::a");
        assert!(matches!(&same[0], Candidate::Exact(found) if found.test == "tests::a"));
        assert!(matches!(&same[1], Candidate::FileBinary(_)));

        let other = learning.candidates("subject::b");
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
            hits: 0,
            misses: 0,
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
        assert_eq!((reached.hits, reached.misses, reached.samples, reached.order), (1, 0, 1, 3));
        let published = published_candidate(killer("tests::a"), 5);
        assert_eq!((published.hits, published.misses, published.samples, published.order), (1, 0, 0, 5));

        let mut state = LearningState {
            next_order: u64::MAX,
            ..LearningState::default()
        };
        advance_order(&mut state);
        assert_eq!(state.next_order, u64::MAX, "learning order saturates instead of wrapping");
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

        let candidates = learning.candidates("subject::item");
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

        assert!(bounded_reach_hints(hints.clone(), &reachable, 7, &over_half).is_empty());
        assert_eq!(bounded_reach_hints(hints[..2].to_vec(), &reachable, 7, &bounded).len(), 2);
        let mut expensive = binary.clone();
        expensive.baseline = Duration::from_millis(1);
        assert!(
            bounded_reach_hints(hints[..2].to_vec(), &[&expensive], 7, &bounded).is_empty(),
            "persisted exact probes must cost less than the binary they replace"
        );

        let mut unknown_count = binary.clone();
        unknown_count.tests = None;
        assert!(bounded_reach_hints(hints[..2].to_vec(), &[&unknown_count], 7, &bounded).is_empty());
        assert!(
            bounded_reach_hints(
                vec![Killer {
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

        assert_eq!(
            probe_cases(
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
            ),
            None
        );
        assert_eq!(tally.generalized_hits.load(Ordering::Relaxed), 0);
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
        let candidates = learning.candidates("subject::changed");

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
    fn stale_generalized_test_falls_back_without_changing_the_outcome_and_reports_a_miss() {
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
        assert_eq!(tally.generalized_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.generalized_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn exact_and_generalized_probe_hit_rates_are_counted_separately() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["print:test tests::killer ... FAILED", "exit:1"]);
        binaries[0].budget = None;
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let exact = Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].target.clone(),
            test: "tests::killer".to_owned(),
        };
        let exact_tally = Tally::default();
        let generalized_tally = Tally::default();

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
        let generalized_judged = judge_ranked(
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
            &generalized_tally,
        );

        assert!(matches!(exact_judged, Judgement::Reached(Outcome::Killed, _, _)));
        assert!(matches!(generalized_judged, Judgement::Reached(Outcome::Killed, _, _)));
        assert_eq!(exact_tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(exact_tally.exact_hits.load(Ordering::Relaxed), 1);
        assert_eq!(exact_tally.generalized_probes.load(Ordering::Relaxed), 0);
        assert_eq!(generalized_tally.exact_probes.load(Ordering::Relaxed), 0);
        assert_eq!(generalized_tally.generalized_probes.load(Ordering::Relaxed), 1);
        assert_eq!(generalized_tally.generalized_hits.load(Ordering::Relaxed), 1);
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
    fn positive_reach_is_reused_by_the_same_item_and_as_a_file_fallback() {
        let learning = FileLearning::new();
        let binary = TestBinary {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            ..crate::testing::test_binary("unused")
        };

        learning.reached("subject::a", &binary, Duration::from_millis(7));

        let same = learning.candidates("subject::a");
        assert_eq!(same.len(), 1);
        assert!(matches!(&same[0], Candidate::ItemBinary(identity) if identity.target == "lib"));

        let other = learning.candidates("subject::b");
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
    fn a_reach_candidate_is_released_only_at_its_canonical_binary() {
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
            &learning.candidates("subject::f"),
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
    fn a_reach_candidate_cannot_bypass_an_earlier_timeout() {
        let (_directory, work, plan, mut binaries) = helper_harness(&["sleep:200", "exit:0"]);
        binaries[0].target = "canonical".to_owned();
        binaries[0].budget = Some(Duration::from_millis(20));
        let mut reached = binaries[0].clone();
        reached.target = "reached".to_owned();
        reached.budget = None;
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

        assert!(matches!(judged, Judgement::Reached(Outcome::Timeout, None, None)));
    }

    #[test]
    fn a_safe_same_item_exact_candidate_waits_for_canonical_iteration() {
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
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 2);
        assert_eq!(tally.saved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_later_candidate_kill_cannot_bypass_an_earlier_unjudged_binary() {
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

        assert!(matches!(judged, Judgement::Reached(Outcome::Pending, None, Some(_))));
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
        assert_eq!(tally.generalized_probes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_candidate_is_demoted_after_two_misses() {
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
        assert!(matches!(&learning.candidates("subject::a")[0], Candidate::Exact(found) if found.test == "tests::first"));
        learning.observe("subject::a", &first, false, Duration::from_millis(10));

        assert!(matches!(&learning.candidates("subject::a")[0], Candidate::Exact(found) if found.test == "tests::second"));
        let state = learning.locked();
        let recorded = state.items["subject::a"]
            .exact
            .iter()
            .find(|candidate| candidate.identity.test == "tests::first")
            .expect("the first candidate remains ranked");
        assert_eq!((recorded.hits, recorded.misses), (1, 2));
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

        assert_eq!(scheduler.select(&state), Some(2), "an idle file outranks every active file");

        state.active_files[1] = 1;
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

        assert_eq!(scheduler.select(&state), Some(1));
        state.remaining[1] = false;
        assert_eq!(scheduler.select(&state), None, "an unhinted same-item duplicate is not reserved");
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
        assert!(matches!(&learning.candidates("same")[0], Candidate::Exact(found) if found.test == "tests::learned"));
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
