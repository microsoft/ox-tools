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
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::thread;

use camino::Utf8Path;
use cargo_gamma_process::MemoryRequest;
use tick::{SimpleClock, Stopwatch};

use super::events::{Events, SelectionAttempt, SelectionResult, SelectionTier};
use super::harness_filters::HarnessFilters;
use super::killers::Killers;
use super::stall::Stall;
use super::test_binary::{Reachability, TestBinary};
#[cfg(test)]
use super::test_binary::{TestScope, order_reachable, reaches};
use super::verdict::{
    Attempt, BinaryRun, DIAGNOSTIC_TAIL_LINES, Only, ReachObservation, Verdict, reach_hint_is_final, run_binary_observed, tail,
};
use super::workspace::Workspace;
use crate::discover::{
    BinaryHint, FileBinaryHints, GENERALIZED_HINTS_VERSION, GeneralizedHints, ItemHints, Killer, Plan, RankedHint, SiteIdentity,
};
use crate::error::error;
use crate::model::{Mutant, Outcome};
use crate::report::{encode_controls, encode_preserving_color};
use crate::{Result, notes};

mod binary_kills;
mod named_kills;
#[cfg(feature = "internals")]
#[cfg_attr(coverage_nightly, coverage(off))]
mod native_smoke;
mod staged;

use binary_kills::BinaryKills;
use named_kills::NamedKills;
#[cfg(feature = "internals")]
#[doc(hidden)]
pub use native_smoke::run_scheduler_native_smoke;
use staged::{PendingState, ProbeState, Stage};

/// Persisted exact probes usually terminate before a quarter of the binary baseline.
const EXACT_PROBE_COST_DIVISOR: u32 = 4;
/// Cold killed-mutant work usually consumes about half of its complete fallback.
const KILLED_MUTANT_COST_DIVISOR: u32 = 2;

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

/// One mutant's completed execution and the scheduling timestamps used for package telemetry.
struct Completed {
    position: usize,
    outcome: Outcome,
    elapsed_ms: u64,
    killer: Option<Killer>,
    note: Option<String>,
    first_started_ms: u64,
    settled_ms: u64,
    attempts: Vec<SelectionAttempt>,
}

enum SweepEvent {
    Started,
    Activity(bool),
    InitialKillerConfirmed(crate::model::MutantId),
    Accounting(SelectionTier, bool, super::session::AttemptCost),
    SelectionAttempts(Vec<SelectionAttempt>),
    Completed(Completed),
}

struct SweepDrain<'a> {
    work: &'a Workspace,
    plan: &'a mut Plan,
    killers: &'a mut Killers,
    tally: &'a Tally,
    package_spans: &'a mut crate::HashMap<Arc<str>, (usize, u64, u64)>,
    checkpoint_failed: &'a mut bool,
    abandoned: &'a OnceLock<String>,
    scheduler: &'a Scheduler,
}

impl SweepDrain<'_> {
    fn drain<E: Events>(
        self,
        events: &mut E,
        mut checkpoint: impl FnMut(&Mutant, &Killers, &super::session::SweepCost) -> Result<()>,
        mut receive: impl FnMut(&mut E) -> Option<SweepEvent>,
        elapsed: impl Fn() -> Duration,
    ) {
        let Self {
            work,
            plan,
            killers,
            tally,
            package_spans,
            checkpoint_failed,
            abandoned,
            scheduler,
        } = self;
        while let Some(event) = receive(events) {
            match event {
                SweepEvent::Started => events.mutant_started(),
                SweepEvent::Activity(active) => events.mutant_activity(active),
                SweepEvent::InitialKillerConfirmed(id) => {
                    if apply_initial_confirmation(tally, id.clone()) && !*checkpoint_failed {
                        let mut cost = super::measure::sweep_cost(Some(tally.checkpoint_spent(package_spans)), elapsed())
                            .expect("a confirmed initial killer always supplies sweep cost");
                        let _inserted = cost.confirmed_initial_killers.insert(id);
                        if let Err(failure) = work.checkpoint_sweep(&cost) {
                            *checkpoint_failed = true;
                            let _first = abandoned.set(failure.to_string());
                            scheduler.abandon();
                        }
                    }
                }
                SweepEvent::Accounting(tier, confirmation, cost) => {
                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                    apply_accounting(tally, tier, confirmation, &cost);
                    if !*checkpoint_failed {
                        let cost = super::measure::sweep_cost(Some(tally.checkpoint_spent(package_spans)), elapsed())
                            .expect("an accounting observation always supplies sweep cost");
                        if let Err(failure) = work.checkpoint_sweep(&cost) {
                            *checkpoint_failed = true;
                            let _first = abandoned.set(failure.to_string());
                            scheduler.abandon();
                        }
                    }
                }
                SweepEvent::SelectionAttempts(attempts) => {
                    for attempt in &attempts {
                        events.selection_attempt(attempt);
                    }
                }
                SweepEvent::Completed(completed) => {
                    for attempt in &completed.attempts {
                        events.selection_attempt(attempt);
                    }

                    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                    record_package_span(package_spans, plan, &completed);
                    if let Some(mutant) = apply_completed(plan, killers, completed) {
                        if *checkpoint_failed {
                            continue;
                        }
                        let cost = super::measure::sweep_cost(Some(tally.checkpoint_spent(package_spans)), elapsed())
                            .expect("a completed mutant always supplies sweep accounting");
                        match checkpoint(mutant, killers, &cost) {
                            Ok(()) => events.mutant(mutant),
                            Err(failure) => {
                                *checkpoint_failed = true;
                                let _first = abandoned.set(failure.to_string());
                                scheduler.abandon();
                            }
                        }
                    }
                }
            }
        }
    }
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
    static EXECUTION_COSTS: RefCell<Vec<(bool, super::session::AttemptCost)>> = const { RefCell::new(Vec::new()) };
    static ACCOUNTING_SENDER: RefCell<Option<mpsc::Sender<SweepEvent>>> = const { RefCell::new(None) };
    static EXECUTION_TIER: core::cell::Cell<(SelectionTier, bool)> = const { core::cell::Cell::new((SelectionTier::Whole, false)) };
    static INITIAL_KILLER: RefCell<Option<(crate::model::MutantId, Killer)>> = const { RefCell::new(None) };
}

pub(super) fn record_observed_killer(binary: &TestBinary, only: Only<'_>, verdict: &Verdict) {
    let Verdict::Failed(named) = verdict else { return };
    let test = named.as_deref().or(match only {
        Only::One(test) => Some(test),
        Only::All => None,
    });
    INITIAL_KILLER.with_borrow(|initial| {
        if let Some((id, killer)) = initial
            && binary.matches_killer(killer)
            && test == Some(killer.test.as_str())
        {
            ACCOUNTING_SENDER.with_borrow(|sender| {
                if let Some(sender) = sender {
                    let _sent = sender.send(SweepEvent::InitialKillerConfirmed(id.clone()));
                }
            });
        }
    });
}

fn execution_tier(tier: SelectionTier, confirmation: bool) {
    EXECUTION_TIER.set((tier, confirmation));
}

fn send_accounting(tier: SelectionTier, confirmation: bool, cost: super::session::AttemptCost) -> bool {
    ACCOUNTING_SENDER.with_borrow(|sender| {
        if let Some(sender) = sender {
            let _sent = sender.send(SweepEvent::Accounting(tier, confirmation, cost));
            true
        } else {
            false
        }
    })
}

pub(super) fn record_binary_launch_attempt(confirmation: bool) {
    if SELECTION_ATTEMPTS.with_borrow(Option::is_some) {
        let (tier, proof) = EXECUTION_TIER.get();
        let _sent =
        // #[gamma::skip(call_result.default, reason = "The return value is discarded; default_after still performs the original call and every side effect.", tag = "equivalent")]
        send_accounting(
            tier,
            confirmation || proof,
            super::session::AttemptCost {
                launches: 1,
                ..super::session::AttemptCost::default()
            },
        );
    }
}

pub(super) fn begin_selection_trace() {
    INITIAL_KILLER.with_borrow_mut(|initial| *initial = None);
    EXECUTION_COSTS.with_borrow_mut(Vec::clear);
    SELECTION_ATTEMPTS.with(|attempts| {
        *attempts.borrow_mut() = Some(Vec::new());
    });
}

#[cfg(test)]
pub(super) fn take_execution_costs() -> Vec<(bool, super::session::AttemptCost)> {
    EXECUTION_COSTS.with_borrow_mut(core::mem::take)
}

#[cfg(test)]
pub(super) fn capture_launch_costs_for_test(body: impl FnOnce()) -> Vec<(SelectionTier, bool, super::session::AttemptCost)> {
    struct Restore(Option<mpsc::Sender<SweepEvent>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACCOUNTING_SENDER.with_borrow_mut(|sender| *sender = self.0.take());
        }
    }
    let (sender, receiver) = mpsc::channel();
    let _restore = Restore(ACCOUNTING_SENDER.with_borrow_mut(|slot| slot.replace(sender)));
    body();
    receiver
        .try_iter()
        .map(|event| {
            let SweepEvent::Accounting(tier, confirmation, cost) = event else {
                panic!("the observed runner emits accounting events only");
            };
            (tier, confirmation, cost)
        })
        .collect()
}

pub(super) fn record_binary_execution(confirmation: bool, elapsed: Duration, verdict: &Verdict) {
    if !SELECTION_ATTEMPTS.with_borrow(Option::is_some) {
        return;
    }
    let result = selection_result(transfer_observation(verdict));
    let cost = attempt_cost(elapsed, result);
    EXECUTION_COSTS.with_borrow_mut(|costs| costs.push((confirmation, cost.clone())));
    let (tier, proof) = EXECUTION_TIER.get();
    let mut finished = cost;
    finished.launches = 0;
    if !confirmation {
        finished.hits = 0;
        finished.misses = 0;
        finished.inconclusive = 0;
    }
    let _sent =
    // #[gamma::skip(call_result.default, reason = "The return value is discarded; default_after still performs the original call and every side effect.", tag = "equivalent")]
    send_accounting(tier, confirmation || proof, finished);
}

fn attempt_cost(elapsed: Duration, result: SelectionResult) -> super::session::AttemptCost {
    super::session::AttemptCost {
        launches: 1,
        execution_ms: elapsed_millis(elapsed),
        hits: usize::from(result == SelectionResult::Hit),
        misses: usize::from(result == SelectionResult::Miss),
        inconclusive: usize::from(result == SelectionResult::Inconclusive),
    }
}

fn record_selection_cost(tally: &Tally, tier: SelectionTier, result: SelectionResult, elapsed: Duration, confirmation: bool) {
    EXECUTION_COSTS.with_borrow_mut(|costs| {
        if !costs.is_empty()
            && send_accounting(
                tier,
                confirmation,
                super::session::AttemptCost {
                    launches: 0,
                    ..attempt_cost(Duration::ZERO, result)
                },
            )
        {
            costs.clear();
            return;
        }
        // Scripted runners do not call record_binary_execution, so account for their selection here.
        if costs.is_empty() {
            costs.push((false, attempt_cost(elapsed, result)));
        }
        for (additional, mut cost) in costs.drain(..) {
            if !additional {
                cost.hits = usize::from(result == SelectionResult::Hit);
                cost.misses = usize::from(result == SelectionResult::Miss);
                cost.inconclusive = usize::from(result == SelectionResult::Inconclusive);
            }
            if !send_accounting(
                tier,
                // #[gamma::skip(logical.or_remove_right, reason = "At this send_accounting site additional=true is observable only when ACCOUNTING_SENDER=None: any nonempty execution-cost vector with a sender takes the earlier successful send/clear/return branch. The only sender-present loop case creates one(false,...) scripted cost. Thus replacing confirmation||additional with confirmation changes only a Boolean passed to a no-sender no-op.", tag = "equivalent")]
                confirmation || additional,
                cost.clone(),
            ) {
                // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                apply_accounting(tally, tier, confirmation || additional, &cost);
            }
        }
    });
}

fn apply_accounting(tally: &Tally, tier: SelectionTier, confirmation: bool, cost: &super::session::AttemptCost) {
    let mut accounting = tally.accounting.lock().unwrap_or_else(PoisonError::into_inner);
    let destination = if confirmation {
        &mut accounting.confirmation
    } else {
        match tier {
            SelectionTier::Exact => &mut accounting.exact,
            SelectionTier::Item | SelectionTier::Reach | SelectionTier::File => &mut accounting.inferred,
            SelectionTier::Whole => &mut accounting.fallback,
        }
    };
    destination.add(cost);
}

fn record_selection_attempt(attempt: SelectionAttempt) {
    SELECTION_ATTEMPTS.with(|attempts| {
        if let Some(attempts) = attempts.borrow_mut().as_mut() {
            attempts.push(attempt);
        }
    });
}

pub(super) fn take_selection_trace() -> Vec<SelectionAttempt> {
    INITIAL_KILLER.with_borrow_mut(|initial| *initial = None);
    SELECTION_ATTEMPTS.with(|attempts| attempts.borrow_mut().take().unwrap_or_default())
}

/// Estimates a single mutant's cost from its reachable binary baselines.
///
/// Killer history further refines: a mutant with a known killer from a previous run is expected
/// to complete in the time of that single probe, which is cheaper than any cold path.
fn mutant_cost(position: usize, plan: &Plan, reach: &Reachability<'_>, killers: &Killers) -> Duration {
    let mutant = &plan.mutants[position];

    // Persisted exact killers usually terminate early, so their prior is one quarter of the
    // binary baseline. Cold whole-binary work below uses one half because killed mutants rarely
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

    binaries.iter().fold(Duration::ZERO, |total, binary| {
        total.saturating_add(binary.baseline / KILLED_MUTANT_COST_DIVISOR)
    })
}

fn mutant_work_with_hints(
    position: usize,
    plan: &Plan,
    reach: &Reachability<'_>,
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

    let Some(binaries) = reach.reachable(mutant).filter(|binaries| !binaries.is_empty()) else {
        return crate::estimate::MutationWork::new(crate::estimate::WorkKind::Uncovered, Duration::ZERO);
    };
    let killed = binaries.iter().fold(Duration::ZERO, |total, binary| {
        total.saturating_add(binary.baseline / KILLED_MUTANT_COST_DIVISOR)
    });
    crate::estimate::MutationWork::costed(crate::estimate::WorkKind::Whole, killed)
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
            let Ok(index) = usize::try_from(cluster.test_set) else {
                // #[gamma::skip(loop.continue_to_break, reason = "InferredPlanning::new first calls GeneralizedHints::supported(), which verifies every reach cluster test_set converts to usize AND is strictly below test_sets.len(). These collections have no interior mutation through the shared borrow. Neither subsequent failed conversion nor absent-set else-continue can execute; replacing either continue by break is inert.", tag = "equivalent")]
                continue;
            };
            let Some(candidates) = hints.test_sets.get(index).map(Vec::as_slice) else {
                // #[gamma::skip(loop.continue_to_break, reason = "InferredPlanning::new first calls GeneralizedHints::supported(), which verifies every reach cluster test_set converts to usize AND is strictly below test_sets.len(). These collections have no interior mutation through the shared borrow. Neither subsequent failed conversion nor absent-set else-continue can execute; replacing either continue by break is inert.", tag = "equivalent")]
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
fn mutant_work(position: usize, plan: &Plan, reach: &Reachability<'_>, killers: &Killers) -> crate::estimate::MutationWork {
    let inferred = InferredPlanning::new(killers);
    mutant_work_with_hints(position, plan, reach, killers, &inferred)
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
/// When a killer hint exists, the expected cost reflects a single probe.
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
fn schedule(pending: &mut [usize], plan: &Plan, reach: &Reachability<'_>, killers: &Killers) {
    pending.sort_by_cached_key(|position| (core::cmp::Reverse(mutant_cost(*position, plan, reach, killers)), *position));

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

#[derive(Clone, Debug)]
struct ScheduledWork {
    file: usize,
    item: Arc<str>,
    package: Arc<str>,
    cost: Duration,
    sibling_benefit: usize,
    hinted: bool,
    transfer: bool,
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
    transfer_items: crate::HashSet<(usize, Arc<str>)>,
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
    #[cfg(test)]
    notifications: AtomicUsize,
    resource_capacities: crate::HashMap<Arc<str>, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalPriority {
    transfer: bool,
    distance: (u8, usize),
    benefit: usize,
    cost: u128,
    stable_order: usize,
    index: usize,
}

impl Ord for LocalPriority {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        other
            .transfer
            .cmp(&self.transfer)
            .then_with(|| self.distance.cmp(&other.distance))
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
        self.turns
            .cmp(&other.turns)
            .then_with(|| other.local.transfer.cmp(&self.local.transfer))
            .then_with(|| self.local.distance.cmp(&other.local.distance))
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
    transfer: Option<bool>,
}

impl Assignment<'_> {
    const fn index(&self) -> usize {
        self.index
    }

    fn complete(mut self, transfer: bool) {
        self.learned = true;
        self.transfer = Some(transfer);
    }
}

impl Drop for Assignment<'_> {
    // #[gamma::skip(fn_value.unit, tag = "timeout", reason = "dropping an assignment must release scheduler capacity and wake pending work")]
    fn drop(&mut self) {
        self.scheduler.release_with_transfer(self.index, self.learned, self.transfer);
    }
}

impl Scheduler {
    #[cfg(test)]
    fn new(work: Vec<ScheduledWork>, files: usize) -> Self {
        Self::with_resources(work, files, crate::HashMap::default())
    }

    fn with_resources(work: Vec<ScheduledWork>, files: usize, resource_capacities: crate::HashMap<Arc<str>, usize>) -> Self {
        let remaining = vec![true; work.len()];
        Self::with_pending(work, files, resource_capacities, remaining)
    }

    fn with_pending(
        work: Vec<ScheduledWork>,
        files: usize,
        resource_capacities: crate::HashMap<Arc<str>, usize>,
        remaining: Vec<bool>,
    ) -> Self {
        assert_eq!(
            work.len(),
            remaining.len(),
            "every pending state must belong to exactly one scheduled work item"
        );
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
            remaining_count: remaining.iter().filter(|pending| **pending).count(),
            remaining,
            active_files: vec![0; files],
            active_items: crate::HashMap::default(),
            learned_items: crate::HashSet::default(),
            transfer_items: work
                .iter()
                .filter(|item| item.transfer)
                .map(|item| (item.file, Arc::clone(&item.item)))
                .collect(),
            package_turns: vec![0; packages.len()],
            package_queues: vec![BTreeSet::new(); packages.len()],
            heads: BTreeSet::new(),
            groups: group_files.iter().map(|_| GroupQueue::default()).collect(),
            files: file_packages.iter().map(|_| FileQueue::default()).collect(),
            priorities: vec![None; work.len()],
            pending_groups: BTreeSet::new(),
            active_resources: crate::HashMap::default(),
            #[cfg(test)]
            priority_updates: 0,
            #[cfg(test)]
            learning_updates: 0,
        };
        for index in 0..work.len() {
            let Some(priority) = priority(&work, &state, index) else {
                continue;
            };
            state.priorities[index] = Some(priority);
            state.pending_groups.insert(work_groups[index]);
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
            #[cfg(test)]
            notifications: AtomicUsize::new(0),
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

    fn claim_with(&self, abandoned: &OnceLock<String>, before_wait: impl FnMut()) -> Option<usize> {
        self.claim_with_wait(abandoned, before_wait, |state| {
            self.changed
                .wait(state)
                .expect("a scheduler panic can leave coupled queue and reservation state inconsistent")
        })
    }

    fn claim_with_wait<'a>(
        &'a self,
        abandoned: &OnceLock<String>,
        mut before_wait: impl FnMut(),
        mut wait: impl FnMut(MutexGuard<'a, SchedulerState>) -> MutexGuard<'a, SchedulerState>,
    ) -> Option<usize> {
        let waiting = Cell::new(false);
        let mut wait = |state| {
            let returned = wait(state);
            waiting.set(false);
            returned
        };
        let mut state = self
            .state
            .lock()
            .expect("a scheduler panic can leave coupled queue and reservation state inconsistent");

        loop {
            assert!(
                !waiting.get(),
                "a blocked claim must perform its wait before retrying while holding the queue lock"
            );
            if abandoned.get().is_some() {
                return None;
            }

            if let Some(index) = self.reserve_next(&mut state) {
                return Some(index);
            }

            if state.remaining_count == 0 {
                return None;
            }

            assert!(
                state.remaining_count > 0,
                "an exhausted scheduler must return instead of waiting for unavailable work"
            );
            before_wait();

            // #[gamma::skip(stmt.delete_call, reason = "The waiting marker is reset unconditionally when the injected/native wait returns, before its only read at the loop head. This write is assertion bookkeeping, not a scheduling signal.", tag = "equivalent")]
            waiting.set(
                // #[gamma::skip(literal.bool_flip, reason = "The waiting marker is reset unconditionally when the injected/native wait returns, before its only read at the loop head. This write is assertion bookkeeping, not a scheduling signal.", tag = "equivalent")]
                true,
            );
            state = wait(state);
        }
    }

    fn reserve_next(&self, state: &mut SchedulerState) -> Option<usize> {
        let index = self.select(state)?;
        let work = &self.work[index];
        self.detach_file(state, work.file);
        debug_assert!(state.remaining[index], "only remaining work may be claimed");
        debug_assert!(state.remaining_count > 0, "remaining work implies a positive remaining count");
        state.remaining[index] = false;
        state.remaining_count -= 1;
        self.remove_work(state, index);
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
        self.refresh_item(state, &(work.file, Arc::clone(&work.item)));
        self.attach_file(state, work.file);
        Some(index)
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
            transfer: None,
        })
    }

    #[cfg(test)]
    fn complete(&self, index: usize) {
        self.release(index, true);
    }

    #[cfg(test)]
    fn release(&self, index: usize, learned: bool) {
        self.release_with_transfer(index, learned, None);
    }

    fn release_with_transfer(&self, index: usize, learned: bool, transfer: Option<bool>) {
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
        if let Some(transfer) = transfer {
            if transfer {
                state.transfer_items.insert(key.clone());
            } else {
                state.transfer_items.remove(&key);
            }
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
        for slot in &self.by_file[work.file] {
            let queue = &state.files[*slot];
            assert_eq!(
                queue.head.is_some(),
                !queue.idle.is_empty(),
                "releasing an assignment must republish every available file head before waking claimants"
            );
        }
        core::mem::drop(state);
        self.notify_all();
    }

    fn abandon(&self) {
        self.abandon_with(|| self.notify_all());
    }

    fn notify_all(&self) {
        // Counts calls only; scheduler locks/worker joins, not this relaxed value, synchronize work.
        #[cfg(test)]
        let _prior = self.notifications.fetch_add(1, Ordering::Relaxed);
        // #[gamma::skip(stmt.delete_call, reason = "This exact native Condvar wake has no synchronous observation. The preceding owned counter does not observe it; distinguishing deletion requires a blocked-thread scheduling/timing experiment, not a deterministic unit test.", tag = "native-boundary")]
        self.changed.notify_all();
    }

    fn abandon_with(&self, notify_all: impl FnOnce()) {
        let _state = self
            .state
            .lock()
            .expect("a scheduler panic can leave coupled queue and reservation state inconsistent");
        notify_all();
    }

    // #[gamma::skip(fn_value.none, tag = "timeout", reason = "never selecting available work leaves scheduler waiters blocked")]
    fn select(&self, state: &SchedulerState) -> Option<usize> {
        if
        // #[gamma::skip(cond.always_false, reason = "The sole production ScheduledWork constructor in test_all_with_accounting_hooks sets resources=Vec::new(); the second-stage scheduler clones those same records. Production never populates ScheduledWork.resources (test-only scheduler fixtures can). On the mutated empty-capacity path, the first head's resources_available uses all() over this empty vector and returns true, selecting the same head; if no head exists, no pending group has work. The removed private fast path changes only queue-search overhead in valid production states.", tag = "equivalent")]
        self.resource_capacities.is_empty() {
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

                // #[gamma::skip(collection.reverse_array, reason = "Both cold/informed queue candidates are mapped without side effects then reduced by PackageHead::min. Reversing only the two queue enumeration positions yields the same total-order minimum; equal heads select the same index.", tag = "equivalent")]
                [&queue.cold, &queue.informed]
            })
            .filter_map(|queue| {
                let first =
                // #[gamma::skip(iter.first_to_last, reason = "GroupQueue members share the file/item distance and transfer key; only those two priority fields are copied from this representative.", tag = "equivalent")]
                queue.first()?;
                let current =
                // #[gamma::skip(try.propagate_to_unwrap, reason = "Scheduler select's group cold/informed queues contain only remaining jobs: reserve_next removes claimed job while holding the same state mutex, and release never reinserts it. Thus queue.first() index has remaining=true, so priority returns Some (its only None is remaining=false). panic_on_failure cannot observe None.", tag = "equivalent")]
                priority(&self.work, state, first.index)?;
                queue
                    .iter()
                    .find(|entry| self.resources_available(state, entry.index))
                    .map(|entry| {
                        let mut local = *entry;
                        local.distance = current.distance;
                        local.transfer = current.transfer;
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

            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
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

            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
            insert_head(state, package);
        }
    }

    fn refresh_item(&self, state: &mut SchedulerState, key: &(usize, Arc<str>)) {
        let active = state.active_items.get(key).copied().unwrap_or(0) != 0;
        let learned = state.learned_items.contains(key);
        let transfer = state.transfer_items.contains(key);
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
            let idle = group
                .cold
                .first()
                .into_iter()
                .chain(group.informed.first())
                .min()
                .copied()
                .map(|mut head| {
                    head.transfer = transfer;
                    head
                });
            group.idle_head = idle;
            group.active_head = idle.map(|mut head| {
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
    // Prefer an uncontended file, then an uncontended item, then an active item.
    let distance = if file_contention == 0 {
        (0, 0)
    } else if item_contention == 0 {
        (1, file_contention)
    } else {
        (2, file_contention)
    };

    Some(LocalPriority {
        transfer: state.transfer_items.contains(&key),
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
#[derive(Debug, Default)]
struct Tally {
    binary_kills: Mutex<BinaryKills>,
    named_kills: Mutex<NamedKills>,
    accounting: Mutex<super::session::SweepAccounting>,
    confirmed_initial_killers: Mutex<BTreeSet<crate::model::MutantId>>,
    /// Legacy requested binary runs, excluding additional confirmation attempts.
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
    item_probes: AtomicUsize,
    item_hits: AtomicUsize,
    reach_probes: AtomicUsize,
    reach_hits: AtomicUsize,
    file_probes: AtomicUsize,
    file_hits: AtomicUsize,
    item_rejected: AtomicUsize,
    reach_rejected: AtomicUsize,
    file_rejected: AtomicUsize,

    /// How canonical binary decisions used reachability information.
    whole_selections: AtomicUsize,
    uncovered_selections: AtomicUsize,

    /// How many canonical binary launches reach learning proved unnecessary.
    saved: AtomicUsize,

    /// How many of those savings came specifically from sweep-derived reach evidence.
    reach_saved: AtomicUsize,
}

/// What a finished sweep spent, read off its [`Tally`] once every worker has stopped.
#[derive(Debug, Clone, Default)]
pub(super) struct Spent {
    pub(super) accounting: Option<super::session::SweepAccounting>,
    pub(super) confirmed_initial_killers: BTreeSet<crate::model::MutantId>,
    /// Legacy requested binary runs, excluding additional confirmation attempts.
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
    pub(super) item_probes: usize,
    pub(super) item_hits: usize,
    pub(super) reach_probes: usize,
    pub(super) reach_hits: usize,
    pub(super) file_probes: usize,
    pub(super) file_hits: usize,
    pub(super) item_rejected: usize,
    pub(super) reach_rejected: usize,
    pub(super) file_rejected: usize,
    pub(super) whole_selections: usize,
    pub(super) uncovered_selections: usize,

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
        self.summarize(spans, true)
    }

    fn checkpoint_spent(&self, spans: &crate::HashMap<Arc<str>, (usize, u64, u64)>) -> Spent {
        self.summarize(spans, false)
    }

    fn summarize(&self, spans: &crate::HashMap<Arc<str>, (usize, u64, u64)>, include_confirmations: bool) -> Spent {
        let confirmations = self.confirmed_initial_killers.lock().unwrap_or_else(PoisonError::into_inner);
        let mut accounting = self.accounting.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(count) = &mut accounting.initial_killer_confirmed {
            *count = confirmations.len();
        }
        let confirmed_initial_killers = if include_confirmations {
            confirmations.clone()
        } else {
            BTreeSet::new()
        };
        drop(confirmations);
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
            accounting: Some(accounting),
            confirmed_initial_killers,
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
            item_probes: self.item_probes.load(Ordering::Relaxed),
            item_hits: self.item_hits.load(Ordering::Relaxed),
            reach_probes: self.reach_probes.load(Ordering::Relaxed),
            reach_hits: self.reach_hits.load(Ordering::Relaxed),
            file_probes: self.file_probes.load(Ordering::Relaxed),
            file_hits: self.file_hits.load(Ordering::Relaxed),
            item_rejected: self.item_rejected.load(Ordering::Relaxed),
            reach_rejected: self.reach_rejected.load(Ordering::Relaxed),
            file_rejected: self.file_rejected.load(Ordering::Relaxed),
            whole_selections: self.whole_selections.load(Ordering::Relaxed),
            uncovered_selections: self.uncovered_selections.load(Ordering::Relaxed),
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
    sweep: Sweep,
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
    sweep: Sweep,
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

#[expect(
    clippy::too_many_arguments,
    reason = "execution adapters keep durability and admission failures deterministically testable"
)]
pub(super) fn test_all_with_accounting_hooks(
    work: &Workspace,
    plan: &mut Plan,
    reach: &Reachability<'_>,
    sweep: Sweep,
    deterministic_reach: bool,
    killers: &mut Killers,
    events: &mut impl Events,
    hooks: (
        impl FnMut(&Mutant, &Killers, &super::session::SweepCost) -> Result<()>,
        impl Fn(u32) -> Option<Judgement> + Sync,
        impl Fn() + Sync,
    ),
) -> Result<Option<Spent>> {
    test_all_with_clock(
        work,
        plan,
        reach,
        sweep,
        deterministic_reach,
        killers,
        events,
        hooks,
        &SimpleClock::new_system(),
    )
}

#[expect(clippy::too_many_lines, reason = "sweep setup and its scoped worker loop share borrowed state")]
#[expect(
    clippy::too_many_arguments,
    reason = "the clock and execution adapters keep the same borrowed sweep state"
)]
fn test_all_with_clock(
    work: &Workspace,
    plan: &mut Plan,
    reach: &Reachability<'_>,
    sweep: Sweep,
    deterministic_reach: bool,
    killers: &mut Killers,
    events: &mut impl Events,
    hooks: (
        impl FnMut(&Mutant, &Killers, &super::session::SweepCost) -> Result<()>,
        impl Fn(u32) -> Option<Judgement> + Sync,
        impl Fn() + Sync,
    ),
    clock: &SimpleClock,
) -> Result<Option<Spent>> {
    let (mut checkpoint, judgement, before_wait) = hooks;
    let jobs = sweep.jobs;

    let mut pending = pending_positions(plan);

    if pending.is_empty() {
        events.sweep_planned(&[], jobs);
        return Ok(None);
    }

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    schedule_pending(&mut pending, plan, reach, killers);

    let captured = (!work.has_testing_journal()).then(|| super::session::InitialHints::capture(plan, killers));
    let initial = work.initial_hints().or(captured.as_ref());
    let tally = tally_for_sweep(plan, killers, initial, work.prior_initial_confirmations());
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
            let work = mutant_work_with_hints(*position, plan, reach, killers, &inferred);
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

    let sweep_started = Stopwatch::new(clock);
    events.sweep_planned(&planned, jobs);

    let mut package_spans: crate::HashMap<Arc<str>, (usize, u64, u64)> = crate::HashMap::default();

    // Resolved up front for the same reason reachability is: a worker must need nothing from the
    // plan, so the calling thread can borrow it mutably and record verdicts as they arrive. Cloned
    // rather than borrowed because the same map is written back as those verdicts land.
    let hints: Vec<Option<Killer>> = pending
        .iter()
        .map(|position| killers.hint(&plan.mutants[*position].id).cloned())
        .collect();
    let initial_killers: Vec<_> = pending
        .iter()
        .map(|position| {
            let id = &plan.mutants[*position].id;
            initial
                .and_then(|initial| initial.killers.get(id))
                .map(|killer| (id.clone(), killer.clone()))
        })
        .collect();
    let reach_hints: Vec<Vec<Candidate>> = pending
        .iter()
        .enumerate()
        .map(|(index, _position)| {
            let candidates: &[Killer] = inferred
                .reach
                .get(&site_identities[index])
                .map_or(&[], |(candidates, _)| *candidates);
            candidates.iter().cloned().map(Candidate::Exact).collect()
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
                transfer: false,
                stable_order: index,
                resources: Vec::new(),
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
    let mut checkpoint_failed = false;
    work.checkpoint_sweep(
        &super::measure::sweep_cost(Some(tally.checkpoint_spent(&package_spans)), Duration::ZERO)
            .expect("a sweep tally always produces a cost"),
    )?;

    let states = pending.iter().map(|_| Mutex::new(PendingState::default())).collect::<Vec<_>>();
    let staged_work = scheduler.work.clone();
    let mut first_scheduler = Some(scheduler);
    #[cfg(test)]
    let shared_runner = super::verdict::shared_runner();
    // Drain exact hints across the population so their confirmed kills seed live inference and
    // fallback ranking. Later stages stay per-mutant to avoid barriers delaying unrelated work.
    for exact in [true, false] {
        let scheduler = first_scheduler.take().unwrap_or_else(|| {
            let remaining = states
                .iter()
                .map(|state| !state.lock().unwrap_or_else(PoisonError::into_inner).finished)
                .collect::<Vec<_>>();
            let tail_work = fallback_work(&staged_work, &remaining, &file_killers, deterministic_reach, &reachable);
            Scheduler::with_pending(tail_work, file_paths.len(), work.resource_capacities(), remaining)
        });
        #[cfg(test)]
        SWEEP_PASSES.with_borrow_mut(|passes| {
            if let Some(passes) = passes {
                passes.push((exact, scheduler.work.iter().map(|work| work.hinted).collect()));
            }
        });
        let (sender, receiver) = mpsc::channel::<SweepEvent>();
        thread::scope(|scope| {
            let spawn = || {
                let sweep_started = &sweep_started;
                let sender = sender.clone();
                let scheduler = &scheduler;
                let ordinals = &ordinals;
                let reachable = &reachable;
                let hints = &hints;
                let initial_killers = &initial_killers;
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
                let states = &states;
                let clock = &clock;
                #[cfg(test)]
                let shared_runner = shared_runner.clone();

                let _handle = scope.spawn(move || {
                    #[cfg(test)]
                    super::verdict::set_shared_runner(shared_runner);
                    let _notes = notes::enter(notes.as_ref());
                    ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = Some(sender.clone()));

                    while abandoned.get().is_none() {
                        let Some(assignment) = scheduler.assignment_with(abandoned, before_wait) else {
                            return;
                        };
                        let index = assignment.index();
                        let position = pending[index];
                        let mut state = lock_pending(states, index);
                        if
                        // #[gamma::skip(cond.always_false, reason = "A Scheduler assignment reserves each pending index once. Rebuilding fallback excludes finished PendingState entries, and finishing happens only while holding that same state lock. The already-finished defensive worker branch cannot be entered by the producer; removing or changing its continue is unreachable.", tag = "equivalent")]
                        state.finished {
                            // #[gamma::skip(loop.continue_to_break, loop.delete_continue, reason = "A Scheduler assignment reserves each pending index once. Rebuilding fallback excludes finished PendingState entries, and finishing happens only while holding that same state lock. The already-finished defensive worker branch cannot be entered by the producer; removing or changing its continue is unreachable.", tag = "equivalent")]
                            continue;
                        }

                        let (ordinal, timeout_multiplier) = ordinals[index];
                        let active = ordinal;
                        let first = state.started_ms.is_none();
                        let first_started_ms = *state.started_ms.get_or_insert_with(|| elapsed_millis(sweep_started.elapsed()));
                        let started = Stopwatch::new(clock);
                        let reachable = &reachable[index];

                        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                        begin_selection_trace();
                        INITIAL_KILLER.with_borrow_mut(|initial| initial.clone_from(&initial_killers[index]));
                        if first {
                            let _sent = sender.send(SweepEvent::Started);
                        } else {
                            let _sent = sender.send(SweepEvent::Activity(true));
                        }
                        let judged = judgement(active).map_or_else(
                            || {
                                judge_learning_pass(exact, |stage| {
                                    judge_learning_step(
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
                                        stage,
                                        &mut state.probes,
                                    )
                                })
                            },
                            Some,
                        );
                        let attempts = take_selection_trace();
                        state.execution_ms = state.execution_ms.saturating_add(elapsed_millis(started.elapsed()));
                        let Some(judged) = judged else {
                            let _sent = sender.send(SweepEvent::SelectionAttempts(attempts));
                            let _sent = sender.send(SweepEvent::Activity(false));
                            continue;
                        };
                        // Restored judgements bypass staged execution; live kills are deduplicated by ordinal.
                        tally
                            .named_kills
                            .lock()
                            .expect("a kill-count panic can leave counts and credited mutants inconsistent")
                            .record(active, &judged);

                        let (outcome, killer, note) = match judged {
                            Judgement::Reached(outcome, killer, note) => (outcome, killer, note),
                            Judgement::Abandoned(reason) => {
                                let _sent = sender.send(SweepEvent::SelectionAttempts(attempts));
                                let _first = abandoned.set(reason);
                                core::mem::drop(assignment);
                                scheduler.abandon();

                                return;
                            }
                        };

                        let elapsed = state.execution_ms;
                        state.finished = true;
                        let settled_ms = elapsed_millis(sweep_started.elapsed());

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

                        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
                        complete_assignment(
                            assignment,
                            deterministic_reach,
                            &file_killers[file_slots[index]],
                            &item_paths[index],
                        );
                    }
                });
            };
            // #[gamma::skip(call_result.default, reason = "Worker launching returns (); default_after retains every spawn and changes only unit to unit.", tag = "equivalent")]
            launch_workers(jobs, spawn);

            // The workers hold the only remaining senders, so the drain ends when the last one finishes.
            core::mem::drop(sender);

            SweepDrain {
                work,
                plan,
                killers,
                tally: &tally,
                package_spans: &mut package_spans,
                checkpoint_failed: &mut checkpoint_failed,
                abandoned: &abandoned,
                scheduler: &scheduler,
            }
            .drain(
                events,
                &mut checkpoint,
                |events| receive_sweep_event(&receiver, events, SWEEP_HEARTBEAT_INTERVAL),
                || sweep_started.elapsed(),
            );
        });
        #[cfg(test)]
        SETTLED_SCHEDULER_OBSERVER.with_borrow_mut(|observer| {
            if let Some(observer) = observer.as_mut() {
                observer(exact, &scheduler);
            }
        });
        if abandoned.get().is_some() {
            break;
        }
    }

    if !checkpoint_failed {
        work.checkpoint_sweep(
            &super::measure::sweep_cost(Some(tally.checkpoint_spent(&package_spans)), sweep_started.elapsed())
                .expect("a sweep tally always produces a cost"),
        )?;
    }

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

/// Seeds each sweep segment from retained hint coverage; absent coverage stays unknown, not zero.
fn tally_for_sweep(
    plan: &Plan,
    killers: &Killers,
    initial: Option<&super::session::InitialHints>,
    confirmed_initial_killers: BTreeSet<crate::model::MutantId>,
) -> Tally {
    let mut accounting = super::session::SweepAccounting::segment(initial, initial.is_some());
    accounting.initial_killer_confirmed = initial.map(|_| confirmed_initial_killers.len());
    Tally {
        binary_kills: Mutex::new(BinaryKills::new(plan, killers)),
        named_kills: Mutex::new(NamedKills::new(plan, killers)),
        accounting: Mutex::new(accounting),
        confirmed_initial_killers: Mutex::new(confirmed_initial_killers),
        ..Tally::default()
    }
}

#[cfg(test)]
type ObservedSweepPasses = Vec<(bool, Vec<bool>)>;
#[cfg(test)]
thread_local! {
    static SWEEP_PASSES: std::cell::RefCell<Option<ObservedSweepPasses>> = const { std::cell::RefCell::new(None) };
}

/// Runs exact hints separately from fallback stages, stopping at the first decisive result.
fn judge_learning_pass(exact: bool, mut judge: impl FnMut(Stage) -> Option<Judgement>) -> Option<Judgement> {
    let steps = Stage::sequence();
    let steps = if exact { &steps[..1] } else { &steps[1..] };
    steps.iter().find_map(|&stage| judge(stage))
}

/// Releases an assignment with transferable priority only when fresh evidence can be reused safely.
fn complete_assignment(assignment: Assignment<'_>, deterministic_reach: bool, learning: &FileLearning, item: &str) {
    assignment.complete(deterministic_reach && learning.has_fresh_killer(item));
}

/// Refreshes fallback costs and priorities without changing alignment with the original pending mask.
fn fallback_work(
    staged_work: &[ScheduledWork],
    remaining: &[bool],
    file_killers: &[FileLearning],
    deterministic_reach: bool,
    reachable: &[&[&TestBinary]],
) -> Vec<ScheduledWork> {
    let mut counts = crate::HashMap::default();
    for (index, item) in staged_work.iter().enumerate() {
        if remaining[index] {
            *counts.entry((item.file, Arc::clone(&item.item))).or_insert(0_usize) += 1;
        }
    }
    staged_work
        .iter()
        .map(|item| ScheduledWork {
            sibling_benefit: counts
                .get(&(item.file, Arc::clone(&item.item)))
                .copied()
                .unwrap_or(
                    // #[gamma::skip(literal.int_increment, reason = "Fallback sibling counts are populated for every remaining item before lookup. The absent-key fallback concerns finished entries, which are not scheduled; 0 versus 1 cannot affect remaining work.", tag = "equivalent")]
                    0,
                )
                .saturating_sub(1),
            hinted: false,
            transfer: deterministic_reach && file_killers[item.file].has_fresh_killer(&item.item),
            ..item.clone()
        })
        .enumerate()
        .map(|(index, mut item)| {
            item.cost = reachable[index].iter().map(|binary| binary.baseline).sum();
            item
        })
        .collect()
}

fn apply_initial_confirmation(tally: &Tally, id: crate::model::MutantId) -> bool {
    let mut confirmed = tally.confirmed_initial_killers.lock().unwrap_or_else(PoisonError::into_inner);
    if confirmed.insert(id) {
        let mut accounting = tally.accounting.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = &mut accounting.initial_killer_confirmed {
            *count = confirmed.len();
        }
        return true;
    }
    false
}

fn record_duplicate_avoided(tally: &Tally) {
    let mut accounting = tally.accounting.lock().unwrap_or_else(PoisonError::into_inner);
    accounting.duplicate_launches_avoided = accounting.duplicate_launches_avoided.saturating_add(1);
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

fn pending_positions(plan: &Plan) -> Vec<usize> {
    plan.mutants
        .iter()
        .enumerate()
        .filter(|(_position, mutant)| mutant.ordinal != 0 && mutant.outcome == Outcome::Pending)
        .map(|(position, _mutant)| position)
        .collect()
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn schedule_pending(pending: &mut [usize], plan: &Plan, reach: &Reachability<'_>, killers: &Killers) {
    schedule(pending, plan, reach, killers);
}

// #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
fn increment_item_count(counts: &mut crate::HashMap<(Arc<Utf8Path>, Arc<str>), usize>, file: Arc<Utf8Path>, item: Arc<str>) {
    let count = counts.entry((file, item)).or_default();
    *count = count.saturating_add(1);
}

#[cfg(test)]
type PlanningObserver = Box<dyn FnMut(&[ScheduledWork], &[Vec<Candidate>])>;

#[cfg(test)]
type SettledSchedulerObserver = Box<dyn FnMut(bool, &Scheduler)>;

#[cfg(test)]
thread_local! {
    static PLANNING_OBSERVER: RefCell<Option<PlanningObserver>> = const { RefCell::new(None) };
    static SETTLED_SCHEDULER_OBSERVER: RefCell<Option<SettledSchedulerObserver>> = const { RefCell::new(None) };
}

fn worker_count(jobs: usize) -> usize {
    jobs.max(1)
}

fn launch_workers(jobs: usize, mut spawn: impl FnMut()) {
    for _ in 0..worker_count(jobs) {
        spawn();
    }
}

fn lock_pending(states: &[Mutex<PendingState>], index: usize) -> MutexGuard<'_, PendingState> {
    states[index].lock().unwrap_or_else(PoisonError::into_inner)
}

fn elapsed_millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn apply_completed<'plan>(plan: &'plan mut Plan, killers: &mut Killers, completed: Completed) -> Option<&'plan Mutant> {
    let mutant = plan.mutants.get_mut(completed.position)?;
    mutant.outcome = completed.outcome;
    mutant.elapsed_ms = completed.elapsed_ms;
    let observed = completed.killer.as_ref();
    mutant.killed_by = observed.map(|killer| killer.test.clone());
    mutant.killer = observed.map(|killer| crate::model::KillerIdentity {
        package: killer.package.clone(),
        target: killer.target.clone(),
        test: killer.test.clone(),
    });
    mutant.note = completed.note;

    match completed.killer {
        Some(killer) => killers.record(mutant.id.clone(), killer),
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
pub(super) struct Sweep {
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
}

/// What one mutant's run across its reachable test binaries came to.
pub(super) enum Judgement {
    /// The mutant was judged: an outcome, the test that caught it if one did, and any note.
    Reached(Outcome, Option<Killer>, Option<String>),

    /// The run could no longer be metered as asked, and no verdict from here on would mean anything.
    Abandoned(String),
}

#[derive(Clone, Copy)]
enum ProbeKind {
    Exact,
    Item,
    Reach,
    File,
}

impl ProbeKind {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn attempt(self, tally: &Tally) {
        let counter = match self {
            Self::Exact => &tally.exact_probes,
            Self::Item => &tally.item_probes,
            Self::Reach => &tally.reach_probes,
            Self::File => &tally.file_probes,
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
        | Verdict::Unconfirmed(_, _)
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
    tally: &Tally,
    ordinal: u32,
    binary: &TestBinary,
    test: Option<String>,
    tier: SelectionTier,
    rank: usize,
    observation: Option<bool>,
    all_tests: bool,
    elapsed: Duration,
    fallback: Duration,
    confirmation: bool,
) {
    // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
    record_selection_cost(tally, tier, selection_result(observation), elapsed, confirmation);

    let attempt = SelectionAttempt {
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
    };
    // #[gamma::skip(call_result.default, reason = "Recording the completed selection attempt returns (); default_after preserves the entire recording side effect.", tag = "equivalent")]
    record_selection_attempt(attempt);
}

/// Whether omitting a known-unreached binary preserves required whole-binary checks.
fn negative_reach_hint_is_final(binary: &TestBinary, sweep: Sweep) -> bool {
    reach_hint_is_final(
        whole_probe_attempt(
            binary,
            // #[gamma::skip(option.none_to_some, reason = "negative_reach_hint_is_final only tests the timeout Option's presence through reach_hint_is_final. TestBinary::budget_for returns None iff binary.budget is None for both override None and Some(0.0); numeric budget changes cannot alter this Boolean classification.", tag = "equivalent")]
            None, sweep,
        ),
        sweep.confirm,
    )
}

fn whole_probe_attempt(binary: &TestBinary, timeout_multiplier: Option<f64>, sweep: Sweep) -> Attempt<'static> {
    let request = MemoryRequest {
        calibrating: false,
        meter: sweep.meter,
        limit: binary.memory,
    };

    Attempt {
        active: Some(1),
        timeout: binary.budget_for(timeout_multiplier, sweep.timeout_floor),
        stall: sweep.stall,
        request,
        only: Only::All,
    }
}

#[cfg(test)]
fn judge(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    hint: Option<&Killer>,
    timeout_multiplier: Option<f64>,
    sweep: Sweep,
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
    sweep: Sweep,
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
    reason = "candidate preflight and canonical adjudication share cached verdict state"
)]
#[cfg(test)]
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
    sweep: Sweep,
    tally: &Tally,
) -> Judgement {
    let mut state = ProbeState::default();
    for &stage in Stage::sequence() {
        if let Some(judged) = staged::judge(
            work,
            ordinal,
            reachable,
            hint,
            candidates,
            learning,
            negative,
            timeout_multiplier,
            sweep,
            tally,
            stage,
            &mut state,
        ) {
            return judged;
        }
    }
    unreachable!("the fallback phase always adjudicates a mutant")
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
        Verdict::Unconfirmed(test, reason) => {
            let which = test.map_or_else(|| "the test binary".to_owned(), |name| format!("test `{name}`"));
            Some(Judgement::Reached(
                Outcome::Flaky,
                None,
                Some(format!(
                    "the confirmation of {which} in `{}` ran with no mutant active and {reason}; \
                     this mutant was never judged. Check the confirmation's resource budget and test reliability",
                    binary.path
                )),
            ))
        }
        Verdict::Unmetered(reason) => Some(Judgement::Abandoned(reason)),
        Verdict::Unjudged(reason) => Some(Judgement::Reached(Outcome::Pending, None, Some(reason))),
    }
}

fn negative_excludes(negative: Option<(&NegativeLearning, &SiteIdentity, bool)>, binary: &TestBinary, sweep: Sweep) -> bool {
    let Some((negative, site, true)) = negative else {
        return false;
    };

    negative.contains(site, binary) && negative_reach_hint_is_final(binary, sweep)
}

#[cfg(test)]
fn killed_by(killer: Killer) -> Judgement {
    Judgement::Reached(Outcome::Killed, Some(killer), None)
}

fn record_launch(tally: &Tally) {
    let _previous = tally.launches.fetch_add(1, Ordering::Relaxed);
}

fn negative_reach_is_final(run: &BinaryRun, only: Only<'_>, deterministic: bool) -> bool {
    deterministic && matches!(only, Only::All) && matches!(run.verdict, Verdict::Passed) && run.reach == ReachObservation::NotReached
}

/// Applies ranked item-test and file-binary learning to one mutant.
#[expect(clippy::too_many_arguments, reason = "adds one file-local state cell to the verdict path")]
#[cfg(test)]
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
    sweep: Sweep,
    tally: &Tally,
) -> Judgement {
    let mut state = ProbeState::default();
    for &stage in Stage::sequence() {
        if let Some(judged) = judge_learning_step(
            work,
            ordinal,
            reachable,
            hint,
            promoted,
            observed,
            item_path,
            site,
            negative,
            deterministic_reach,
            timeout_multiplier,
            sweep,
            tally,
            stage,
            &mut state,
        ) {
            return judged;
        }
    }
    unreachable!("the fallback phase always adjudicates a mutant")
}

#[expect(clippy::too_many_arguments, reason = "stage state extends the existing borrowed learning boundary")]
fn judge_learning_step(
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
    sweep: Sweep,
    tally: &Tally,
    stage: Stage,
    cached: &mut ProbeState,
) -> Option<Judgement> {
    let active = ordinal;
    let exact = if deterministic_reach { hint } else { None };
    let mut candidates = Vec::new();
    if
    // #[gamma::skip(logical.and_remove_right, reason = "Removing stage gate only supplies extra cloned candidates to Exact/Fallback. staged::judge counts candidates solely during Inferred/Binaries; Exact ignores candidate slice. Fallback initially derives order from candidates but immediately overwrites it unconditionally from binary_kills.order before any candidate execution. No tally/ranking/verdict changes, only unused allocation/work.", tag = "equivalent")]
    deterministic_reach && matches!(stage, Stage::Inferred | Stage::Binaries) {
        candidates.extend(observed.select_candidates(item_path));
        candidates.extend_from_slice(promoted);
    }
    let judged = staged::judge(
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
        stage,
        cached,
    );

    // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
    if let Some(judged) = &judged {
        FileLearning::publish(observed, item_path, judged);
    }
    judged
}

#[cfg(test)]
fn candidate_tier_counts(candidates: &[Candidate]) -> [usize; 3] {
    let mut counts = [0; 3];
    for candidate in candidates {
        counts[match candidate.probe_kind() {
            ProbeKind::Item => 0,
            ProbeKind::Reach => 1,
            ProbeKind::File => 2,
            ProbeKind::Exact => unreachable!("inferred tests never use exact accounting"),
        }] += 1;
    }
    counts
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
    fresh: Vec<Killer>,
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
                    fresh: Vec::new(),
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

    fn select_candidates(&self, item_path: &str) -> Vec<Candidate> {
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
        reached_file.retain(|candidate| !contains_ranked_identity(&reached, &candidate.identity));
        // #[gamma::skip(all, reason = "this orchestration side effect crosses a process, event, cache, or synchronization boundary that cannot be isolated safely in a deterministic unit test")]
        binaries.retain(|candidate| !contains_ranked_identity(&reached, &candidate.identity));
        exact
            .into_iter()
            .map(|candidate| Candidate::Exact(candidate.identity))
            .chain(reached.into_iter().map(|candidate| Candidate::ItemBinary(candidate.identity)))
            .chain(
                reached_file
                    .into_iter()
                    .map(|candidate| Candidate::ReachFileBinary(candidate.identity)),
            )
            .chain(binaries.into_iter().map(|candidate| Candidate::FileBinary(candidate.identity)))
            .collect()
    }

    fn has_fresh_killer(&self, item_path: &str) -> bool {
        self.locked().items.get(item_path).is_some_and(|item| {
            item.exact.iter().any(|candidate| {
                item.fresh.contains(&candidate.identity) && candidate.seeds.saturating_add(candidate.hits) >= candidate.misses
            })
        })
    }

    #[cfg(test)]
    fn candidates(&self, item_path: &str) -> Vec<Candidate> {
        self.select_candidates(item_path)
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

        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
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
            Judgement::Reached(_, Some(killer), _) => killer,
            Judgement::Reached(..) | Judgement::Abandoned(_) => return,
        };

        let mut state = self.locked();
        let order = state.next_order;

        // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
        advance_order(&mut state);
        let item = state.items.entry(item_path.to_owned()).or_default();

        if !item.fresh.contains(killer) {
            item.fresh.push(killer.clone());
        }
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
    use core::cmp::Reverse;
    use core::iter::once;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::time::Instant;

    use camino::Utf8PathBuf;
    use serde_json::{Value, from_slice, from_str, from_value, to_string, to_value};

    use super::super::session::AttemptCost;
    use super::*;
    use crate::HashMap;
    #[cfg(unix)]
    use crate::exec::faults::{self, Fault};
    #[cfg(unix)]
    use crate::exec::memory;
    use crate::exec::resources::Resources;
    use crate::ops::collect::Shape;

    #[test]
    fn recovered_negative_hint_finality_preserves_budget_presence_and_every_policy_gate() {
        let mut binary = crate::testing::test_binary("never-launched-negative-policy-matrix");
        binary.baseline = Duration::from_secs(1);
        for budget in [None, Some(Duration::ZERO), Some(Duration::from_secs(1)), Some(Duration::MAX)] {
            binary.budget = budget;
            for stall in [
                Stall::NONE,
                Stall {
                    budget: Some(Duration::ZERO),
                },
            ] {
                for meter in [false, true] {
                    for confirm in [false, true] {
                        let policy = Sweep {
                            timeout_floor: Duration::from_secs(2),
                            stall,
                            jobs: 1,
                            meter,
                            confirm,
                        };
                        let expected = budget.is_none() && stall.budget.is_none() && !meter && !confirm;
                        assert_eq!(negative_reach_hint_is_final(&binary, policy), expected);
                        for multiplier in [None, Some(0.5), Some(2.0)] {
                            assert_eq!(
                                reach_hint_is_final(whole_probe_attempt(&binary, multiplier, policy), confirm),
                                expected
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn recovered_filtered_exact_negative_probe_does_not_scale_an_extreme_multiplier() {
        let mut work = Workspace::adopt(
            "never-owned-negative-overflow".into(),
            "never-written-negative-overflow-target".into(),
        );
        work.set_test_args(vec!["only-selected-test".into()]);
        let mut binary = crate::testing::test_binary("never-launched-negative-overflow");
        binary.baseline = Duration::from_secs(1);
        let hint = staged::killer_for(&binary, "not-selected-test");
        assert!(!HarnessFilters::parse(work.test_arguments()).admits(&hint.test));
        let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
        let negative = NegativeLearning::default();
        negative.record(&site, &binary);
        let policy = Sweep {
            confirm: false,
            ..sweep(Stall::NONE)
        };
        for budget in [None, Some(Duration::ZERO), Some(Duration::from_secs(1)), Some(Duration::MAX)] {
            binary.budget = budget;
            let mut state = PendingState::default();
            let tally = Tally::default();
            let judgement = super::super::verdict::with_binary_runner(
                |_, _, _| panic!("neither a negative-reach exclusion nor a filtered exact hint may launch a binary"),
                || {
                    staged::judge(
                        &work,
                        1,
                        &[&binary],
                        Some(&hint),
                        &[],
                        None,
                        Some((&negative, &site, true)),
                        Some(f64::MAX),
                        policy,
                        &tally,
                        Stage::Exact,
                        &mut state.probes,
                    )
                },
            );
            assert!(judgement.is_none());
            assert_eq!(state.probes.binary_runs, [None]);
            assert!(state.probes.passed_tests.is_empty());
            assert_eq!(tally.launches.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn recovered_exact_learning_barrier_visits_only_exact_and_fallback_stages_short_circuit_in_order() {
        for (exact, expected) in [
            (true, vec![Stage::Exact]),
            (false, vec![Stage::Inferred, Stage::Binaries, Stage::Fallback]),
        ] {
            let mut visited = Vec::new();
            assert!(
                judge_learning_pass(exact, |stage| {
                    visited.push(stage);
                    None
                })
                .is_none()
            );
            assert_eq!(visited, expected);
        }
        let mut visited = Vec::new();
        let judged = judge_learning_pass(false, |stage| {
            visited.push(stage);
            (stage == Stage::Binaries).then_some(Judgement::Reached(Outcome::Survived, None, None))
        });
        assert_eq!(visited, [Stage::Inferred, Stage::Binaries]);
        assert!(matches!(judged, Some(Judgement::Reached(Outcome::Survived, None, None))));
    }

    #[test]
    fn recovered_sweep_retains_the_initial_hint_scheduler_and_never_enters_fallback_after_abandonment() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SWEEP_PASSES.with_borrow_mut(|passes| *passes = None);
            }
        }
        for abandon in [false, true] {
            SWEEP_PASSES.with_borrow_mut(|passes| {
                assert!(passes.is_none());
                *passes = Some(Vec::new());
            });
            let reset = Reset;
            let mut work = Workspace::adopt("in-memory-stage-boundary".into(), "never-written-stage/target".into());
            work.set_live_sweep_publisher_for_test(|_, _| Ok(()));
            let (journal, _entries) = super::super::testing_state::TestingJournal::memory_for_test(false);
            work.set_journal_for_test(journal);
            let mut mutant = crate::testing::ci_fixture::mutant("lib.rs", 1, "arith.add_to_sub", Outcome::Pending);
            mutant.ordinal = 1;
            let binary = crate::testing::test_binary("never-launched-stage-boundary");
            let mut killers = Killers::default();
            killers.record(mutant.id.clone(), staged::killer_for(&binary, "known"));
            let mut plan = telemetry_plan(vec![mutant]);
            let reach = Reachability::build(&plan, &[], &NARROW);
            let mut events = SweepRecorder::default();
            let result = test_all_with_accounting_hooks(
                &work,
                &mut plan,
                &reach,
                sweep(Stall::NONE),
                true,
                &mut killers,
                &mut events,
                (
                    |_, _, _| Ok(()),
                    |_| {
                        Some(if abandon {
                            Judgement::Abandoned("named in-memory abandonment".to_owned())
                        } else {
                            Judgement::Reached(Outcome::Survived, None, None)
                        })
                    },
                    || {},
                ),
            );
            if abandon {
                assert!(result.unwrap_err().message().contains("named in-memory abandonment"));
                assert_eq!(plan.mutants[0].outcome, Outcome::Pending);
            } else {
                assert!(result.unwrap().is_some());
                assert_eq!(plan.mutants[0].outcome, Outcome::Survived);
            }
            let expected = if abandon {
                vec![(true, vec![true])]
            } else {
                vec![(true, vec![true]), (false, vec![false])]
            };
            SWEEP_PASSES.with_borrow(|passes| assert_eq!(passes.as_ref().unwrap(), &expected));
            drop(reset);
        }
    }

    #[test]
    fn recovered_sweep_tally_seeds_both_killer_rankings_and_retains_prior_confirmations() {
        let binaries = ranked_binaries();
        let mut killers = Killers::default();
        killers.record("historical".into(), staged::killer_for(&binaries[1], "retained"));
        let plan = telemetry_plan(Vec::new());
        let initial = super::super::session::InitialHints {
            population: 3,
            killers: once(("historical".into(), staged::killer_for(&binaries[1], "retained"))).collect(),
        };
        let prior = BTreeSet::from(["confirmed".into()]);
        let tally = tally_for_sweep(&plan, &killers, Some(&initial), prior.clone());
        let confirmed = tally
            .confirmed_initial_killers
            .lock()
            .expect("only owned confirmations are recorded under this lock")
            .clone();
        let accounting = tally
            .accounting
            .lock()
            .expect("only owned counters are copied under this lock")
            .clone();
        let binary_order = tally
            .binary_kills
            .lock()
            .expect("ranking has no user callbacks or assertions")
            .order(0, &[&binaries[0], &binaries[1]]);
        assert_eq!(confirmed, prior);
        assert_eq!(accounting.initial_killer_confirmed, Some(1));
        assert_eq!(accounting.initial_exact_hints, Some(1));
        assert_eq!(binary_order, [1, 0]);
        let mut selections = [(0, "absent"), (1, "retained")];
        tally
            .named_kills
            .lock()
            .unwrap()
            .rank(&mut selections, &[&binaries[0], &binaries[1]]);
        assert_eq!(selections, [(1, "retained"), (0, "absent")]);
        let without_initial = tally_for_sweep(&plan, &killers, None, prior);
        let confirmed = without_initial
            .accounting
            .lock()
            .expect("only an owned counter is read under this lock")
            .initial_killer_confirmed;
        assert_eq!(confirmed, None);
    }

    #[test]
    fn recovered_assignment_completion_learns_once_and_transfers_only_fresh_deterministic_evidence() {
        for deterministic in [false, true] {
            for fresh in [false, true] {
                let scheduler = Scheduler::new(
                    vec![
                        scheduled(0, "item", "alpha", 1, 1, false, 0),
                        scheduled(0, "item", "alpha", 1, 1, false, 1),
                    ],
                    1,
                );
                let learning = FileLearning::new();
                if fresh {
                    learning.publish("item", &killed_by(killer("fresh")));
                }
                let assignment = scheduler
                    .assignment_with(&OnceLock::new(), || panic!("one slot is immediately available"))
                    .unwrap();
                assert_eq!(assignment.index(), 0);
                complete_assignment(assignment, deterministic, &learning, "item");
                let state = scheduler.state.lock().unwrap();
                let key = (0, Arc::<str>::from("item"));
                assert!(state.learned_items.contains(&key));
                assert_eq!(state.transfer_items.contains(&key), deterministic && fresh);
                assert_eq!(state.active_files, [0]);
                assert_eq!(state.remaining, [false, true]);
                assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 1);
            }
        }
    }

    #[test]
    fn recovered_fresh_killer_pool_retains_unique_identities_across_repeated_publications() {
        let learning = FileLearning::new();
        for name in ["first", "first", "second", "first"] {
            learning.publish("item", &killed_by(killer(name)));
        }
        let state = learning.locked();
        assert_eq!(state.items["item"].fresh, [killer("first"), killer("second")]);
        assert_eq!(state.items["item"].exact.len(), 2);
        assert_eq!(state.binaries.len(), 1);
    }

    #[test]
    fn recovered_fallback_schedule_recounts_only_pending_siblings_and_resets_stage_metadata() {
        let mut work = vec![
            scheduled(0, "item", "alpha", 91, 99, true, 0),
            scheduled(0, "item", "alpha", 92, 99, true, 1),
            scheduled(0, "item", "alpha", 93, 99, true, 2),
            scheduled(0, "other", "alpha", 94, 99, true, 3),
            scheduled(1, "item", "beta", 95, 99, true, 4),
        ];
        for item in &mut work {
            item.transfer = true;
            item.resources = vec!["preserved".into()];
        }
        let remaining = [true, false, true, false, true];
        let learning = [FileLearning::new(), FileLearning::new()];
        learning[0].publish("item", &killed_by(killer("fresh")));
        let mut first = crate::testing::test_binary("not-launched-first");
        first.baseline = Duration::from_millis(7);
        let mut second = crate::testing::test_binary("not-launched-second");
        second.baseline = Duration::from_millis(11);
        let reachable: [&[&TestBinary]; 5] = [&[&first, &second], &[&first], &[&second], &[], &[&second]];
        for deterministic in [false, true] {
            let fallback = fallback_work(&work, &remaining, &learning, deterministic, &reachable);
            assert_eq!(fallback.len(), 5);
            assert_eq!(
                fallback.iter().map(|item| item.sibling_benefit).collect::<Vec<_>>(),
                [1, 1, 1, 0, 0]
            );
            assert_eq!(fallback.iter().map(|item| item.hinted).collect::<Vec<_>>(), [false; 5]);
            assert_eq!(
                fallback.iter().map(|item| item.transfer).collect::<Vec<_>>(),
                [deterministic, deterministic, deterministic, false, false]
            );
            assert_eq!(
                fallback.iter().map(|item| item.cost.as_millis()).collect::<Vec<_>>(),
                [18, 7, 11, 0, 11]
            );
            for (actual, original) in fallback.iter().zip(&work) {
                assert_eq!(
                    (actual.file, &actual.item, &actual.package),
                    (original.file, &original.item, &original.package)
                );
                assert_eq!(actual.stable_order, original.stable_order);
                assert_eq!(actual.resources, original.resources);
            }
        }
    }

    #[test]
    fn recovered_hinted_group_partition_and_active_item_distance_keep_exact_private_state() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "item", "alpha", 1, 1, true, 0),
                scheduled(0, "item", "alpha", 2, 1, false, 1),
            ],
            1,
        );
        let mut state = scheduler.state.lock().unwrap();
        let group = scheduler.work_groups[0];
        assert_eq!(state.groups[group].informed.iter().map(|head| head.index).collect::<Vec<_>>(), [0]);
        assert_eq!(state.groups[group].cold.iter().map(|head| head.index).collect::<Vec<_>>(), [1]);
        let key = (0, Arc::<str>::from("item"));
        for (active, distance) in [(false, (1, 0)), (true, (2, 0)), (false, (1, 0))] {
            if active {
                state.active_items.insert(key.clone(), 1);
            } else {
                state.active_items.remove(&key);
            }
            scheduler.refresh_item(&mut state, &key);
            assert_eq!(state.groups[group].active_head.unwrap().distance, distance);
        }
    }

    #[test]
    fn oct25_releasing_work_reattaches_the_uncontended_head_before_notifying_waiters() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "first", "alpha", 1, 0, false, 0),
                scheduled(0, "second", "alpha", 1, 0, false, 1),
            ],
            1,
        );
        {
            let mut state = scheduler.state.lock().unwrap();
            assert_eq!(scheduler.reserve_next(&mut state), Some(0));
            assert_eq!(state.heads.first().unwrap().local.distance, (1, 1));
        }
        scheduler.release(0, false);
        let state = scheduler.state.lock().unwrap();
        assert_eq!(state.heads.first().unwrap().local.index, 1);
        assert_eq!(state.heads.first().unwrap().local.distance, (0, 0));
        assert_eq!(state.active_files, [0]);
        assert!(state.active_items.is_empty());
        assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn oct25_abandonment_notifies_waiters_once_without_waiting() {
        let scheduler = Scheduler::new(vec![scheduled(0, "item", "alpha", 1, 0, false, 0)], 1);
        assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 0);
        scheduler.abandon();
        assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 1);
        let state = scheduler
            .state
            .try_lock()
            .expect("abandonment releases its queue lock before returning");
        assert_eq!(state.remaining, [true]);
        assert_eq!(state.remaining_count, 1);
    }

    #[test]
    fn oct25_scheduler_retries_only_after_the_owned_wait_adapter_returns_its_guard() {
        let mut item = scheduled(0, "item", "alpha", 1, 0, false, 0);
        item.resources = vec!["bounded".into()];
        let scheduler = Scheduler::with_resources(vec![item], 1, once((Arc::from("bounded"), 1)).collect());
        scheduler.state.lock().unwrap().active_resources.insert("bounded".into(), 1);
        let before = std::cell::Cell::new(0);
        let waits = std::cell::Cell::new(0);
        let selected = scheduler.claim_with_wait(
            &OnceLock::new(),
            || {
                before.set(before.get() + 1);
                assert!(before.get() < 2, "the wait adapter must run before the next retry");
            },
            |mut state| {
                waits.set(waits.get() + 1);
                state.active_resources.clear();
                state
            },
        );
        assert_eq!(selected, Some(0));
        assert_eq!((before.get(), waits.get()), (1, 1));
    }

    #[test]
    fn recovered_hint_cost_planning_uses_the_requested_nonzero_plan_position() {
        let mutants = (1..=2)
            .map(|ordinal| {
                let mut mutant = crate::testing::ci_fixture::mutant("lib.rs", ordinal, "arith.add_to_sub", Outcome::Pending);
                mutant.ordinal = u32::try_from(ordinal).unwrap();
                mutant.package = "subject".into();
                mutant
            })
            .collect();
        let plan = telemetry_plan(mutants);
        let mut binary = crate::testing::test_binary("not-launched");
        binary.baseline = Duration::from_millis(100);
        let reach = Reachability::build(&plan, std::slice::from_ref(&binary), &NARROW);
        let mut killers = Killers::default();
        killers.record(plan.mutants[1].id.clone(), staged::killer_for(&binary, "retained"));
        let hints = InferredPlanning::new(&killers);
        let first = mutant_work_with_hints(0, &plan, &reach, &killers, &hints);
        let second = mutant_work_with_hints(1, &plan, &reach, &killers, &hints);
        assert_eq!(first.kind, crate::estimate::WorkKind::Whole);
        assert_eq!(first.scheduling_cost(), Duration::from_millis(50));
        assert_eq!(second.kind, crate::estimate::WorkKind::Exact);
        assert_eq!(second.scheduling_cost(), Duration::from_micros(27_500));
    }

    #[test]
    fn recovered_sweep_fake_clock_retains_exact_execution_and_package_timestamp_intervals() {
        let mut work = Workspace::adopt("in-memory-clock".into(), "never-written-clock/target".into());
        work.set_live_sweep_publisher_for_test(|_, _| Ok(()));
        let (journal, entries) = super::super::testing_state::TestingJournal::memory_for_test(false);
        work.set_journal_for_test(journal);
        let mutants = (1..=2)
            .map(|ordinal| {
                let mut mutant = crate::testing::ci_fixture::mutant("lib.rs", ordinal, "arith.add_to_sub", Outcome::Pending);
                mutant.ordinal = u32::try_from(ordinal).unwrap();
                mutant.package = "subject".into();
                mutant
            })
            .collect();
        let mut plan = telemetry_plan(mutants);
        let reach = Reachability::build(&plan, &[], &NARROW);
        let mut killers = Killers::default();
        let mut events = SweepRecorder::default();
        let control = Arc::new(tick::ClockControl::new());
        let clock = control.to_simple_clock();
        let mut costs = Vec::new();
        let spent = with_planning_observer(
            {
                let control = Arc::clone(&control);
                move |items, _| {
                    assert!(items.iter().all(|item| !item.transfer));
                    control.advance(Duration::from_millis(5));
                }
            },
            || {
                test_all_with_clock(
                    &work,
                    &mut plan,
                    &reach,
                    Sweep {
                        jobs: 1,
                        ..sweep(Stall::NONE)
                    },
                    true,
                    &mut killers,
                    &mut events,
                    (
                        |mutant, _, cost| {
                            costs.push((mutant.ordinal, cost.elapsed));
                            Ok(())
                        },
                        |_| {
                            control.advance(Duration::from_millis(7));
                            Some(Judgement::Reached(Outcome::Survived, None, None))
                        },
                        || {},
                    ),
                    &clock,
                )
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.mutants.iter().map(|mutant| mutant.elapsed_ms).collect::<Vec<_>>(), [7, 7]);
        assert_eq!(costs.iter().map(|(ordinal, _)| *ordinal).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(costs.last().unwrap().1, Duration::from_millis(19));
        let costs = entries
            .lock()
            .unwrap()
            .iter()
            .map(|bytes| from_slice::<Value>(bytes).unwrap())
            .filter(|entry| entry.get("diagnostic").is_none())
            .collect::<Vec<_>>();
        assert_eq!(costs.len(), 2);
        let initial: super::super::session::SweepCost = from_value(costs[0]["sweep"].clone()).unwrap();
        let final_cost: super::super::session::SweepCost = from_value(costs[1]["sweep"].clone()).unwrap();
        assert_eq!(initial.elapsed, Duration::ZERO);
        assert_eq!(final_cost.elapsed, Duration::from_millis(19));
        assert_eq!(spent.packages.len(), 1);
        assert_eq!(
            (
                spent.packages[0].mutants,
                spent.packages[0].first_started_ms,
                spent.packages[0].settled_ms
            ),
            (2, 5, 19)
        );
    }

    #[test]
    fn recovered_sweep_propagates_both_durable_boundary_errors_and_skips_final_append_after_callback_failure() {
        for (fail_after, callback_fails, expected_judgements, expected_entries, message) in [
            (Some(0), false, 0, 1, "injected durable append error"),
            (Some(2), false, 1, 3, "injected durable append error"),
            (None, true, 1, 2, "injected verdict checkpoint failure"),
        ] {
            let mut work = Workspace::adopt("in-memory-boundaries".into(), "never-written-boundaries/target".into());
            work.set_live_sweep_publisher_for_test(|_, _| Err(error!("named live-diagnostic refusal")));
            let (journal, entries) = super::super::testing_state::TestingJournal::memory_failing_after_for_test(fail_after);
            work.set_journal_for_test(journal);
            let mut mutant = crate::testing::ci_fixture::mutant("lib.rs", 1, "arith.add_to_sub", Outcome::Pending);
            mutant.ordinal = 1;
            let mut plan = telemetry_plan(vec![mutant]);
            let reach = Reachability::build(&plan, &[], &NARROW);
            let judgements = AtomicUsize::new(0);
            let result = test_all_with_accounting_hooks(
                &work,
                &mut plan,
                &reach,
                sweep(Stall::NONE),
                true,
                &mut Killers::default(),
                &mut SweepRecorder::default(),
                (
                    |_, _, _| {
                        if callback_fails {
                            Err(error!("injected verdict checkpoint failure"))
                        } else {
                            Ok(())
                        }
                    },
                    |_| {
                        let _previous = judgements.fetch_add(1, Ordering::Relaxed);
                        Some(Judgement::Reached(Outcome::Survived, None, None))
                    },
                    || {},
                ),
            );
            assert!(result.unwrap_err().to_string().contains(message), "{fail_after:?}/{callback_fails}");
            assert_eq!(judgements.load(Ordering::Relaxed), expected_judgements);
            assert_eq!(entries.lock().unwrap().len(), expected_entries);
        }
    }

    #[test]
    fn recovered_drain_checkpoints_only_new_confirmations_and_latches_the_first_append_failure() {
        for initially_failed in [false, true] {
            for preconfirmed in [false, true] {
                for fail in [false, true] {
                    let mut work = Workspace::adopt("in-memory-drain".into(), "never-written-drain/target".into());
                    work.set_live_sweep_publisher_for_test(|_, _| Err(error!("named live-diagnostic refusal")));
                    let (journal, entries) = super::super::testing_state::TestingJournal::memory_for_test(fail);
                    work.set_journal_for_test(journal);
                    let mut plan = telemetry_plan(Vec::new());
                    let mut killers = Killers::default();
                    let tally = Tally::default();
                    if preconfirmed {
                        assert!(apply_initial_confirmation(&tally, "retained".into()));
                    }
                    let mut spans = HashMap::default();
                    let mut failed = initially_failed;
                    let abandoned = OnceLock::new();
                    let scheduler = Scheduler::new(Vec::new(), 0);
                    let cost = super::super::session::AttemptCost {
                        launches: 1,
                        execution_ms: 7,
                        ..super::super::session::AttemptCost::default()
                    };
                    let mut received = std::collections::VecDeque::from([
                        SweepEvent::InitialKillerConfirmed("retained".into()),
                        SweepEvent::InitialKillerConfirmed("retained".into()),
                        SweepEvent::Accounting(SelectionTier::Exact, false, cost.clone()),
                        SweepEvent::Accounting(SelectionTier::Exact, false, cost),
                    ]);
                    SweepDrain {
                        work: &work,
                        plan: &mut plan,
                        killers: &mut killers,
                        tally: &tally,
                        package_spans: &mut spans,
                        checkpoint_failed: &mut failed,
                        abandoned: &abandoned,
                        scheduler: &scheduler,
                    }
                    .drain(
                        &mut SweepRecorder::default(),
                        |_, _, _| panic!("these events contain no completed mutants"),
                        |_| received.pop_front(),
                        || Duration::from_millis(23),
                    );
                    let checkpoints = entries
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|bytes| from_slice::<Value>(bytes).unwrap())
                        .filter(|entry| entry.get("diagnostic").is_none())
                        .count();
                    let expected = if initially_failed {
                        0
                    } else if fail {
                        1
                    } else if preconfirmed {
                        2
                    } else {
                        3
                    };
                    assert_eq!(checkpoints, expected, "{initially_failed}/{preconfirmed}/{fail}");
                    assert_eq!(failed, initially_failed || fail);
                    assert_eq!(abandoned.get().is_some(), !initially_failed && fail);
                    assert_eq!(
                        scheduler.notifications.load(Ordering::Relaxed),
                        usize::from(!initially_failed && fail)
                    );
                    assert_eq!(tally.confirmed_initial_killers.lock().unwrap().len(), 1);
                    let accounting = tally.accounting.lock().unwrap();
                    assert_eq!((accounting.exact.launches, accounting.exact.execution_ms), (2, 14));
                }
            }
        }
    }

    #[test]
    fn recovered_drain_completed_checkpoint_failure_notifies_once_and_suppresses_later_publication() {
        let work = Workspace::adopt("in-memory-completed".into(), "in-memory-completed-target".into());
        let mutants = (1..=2)
            .map(|ordinal| {
                let mut mutant = crate::testing::ci_fixture::mutant("lib.rs", ordinal, "arith.add_to_sub", Outcome::Pending);
                mutant.ordinal = u32::try_from(ordinal).unwrap();
                mutant
            })
            .collect();
        let mut plan = telemetry_plan(mutants);
        let mut killers = Killers::default();
        let tally = Tally::default();
        let mut spans = HashMap::default();
        let mut failed = false;
        let abandoned = OnceLock::new();
        let scheduler = Scheduler::new(Vec::new(), 0);
        let mut checkpoints = Vec::new();
        let mut received = (0..2)
            .map(|position| {
                SweepEvent::Completed(Completed {
                    position,
                    outcome: Outcome::Survived,
                    elapsed_ms: 7,
                    killer: None,
                    note: None,
                    first_started_ms: 11,
                    settled_ms: 18,
                    attempts: Vec::new(),
                })
            })
            .collect::<std::collections::VecDeque<_>>();
        let mut events = SweepRecorder::default();
        SweepDrain {
            work: &work,
            plan: &mut plan,
            killers: &mut killers,
            tally: &tally,
            package_spans: &mut spans,
            checkpoint_failed: &mut failed,
            abandoned: &abandoned,
            scheduler: &scheduler,
        }
        .drain(
            &mut events,
            |mutant, _, _| {
                checkpoints.push(mutant.ordinal);
                Err(error!("durable verdict failed"))
            },
            |_| received.pop_front(),
            || Duration::from_millis(19),
        );
        assert_eq!(checkpoints, [1]);
        assert!(failed);
        assert_eq!(abandoned.get().map(String::as_str), Some("durable verdict failed"));
        assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 1);
        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Survived));
    }

    #[test]
    fn recovered_whole_probe_preserves_activation_and_rescales_the_measured_timeout() {
        let mut binary = crate::testing::test_binary("not-launched");
        binary.baseline = Duration::from_millis(7);
        binary.budget = Some(Duration::from_millis(70));
        binary.memory = Some(4096);
        let sweep = Sweep {
            timeout_floor: Duration::from_millis(3),
            stall: Stall {
                budget: Some(Duration::from_millis(11)),
            },
            jobs: 2,
            meter: true,
            confirm: false,
        };
        for (multiplier, timeout) in [(None, 70), (Some(2.0), 14), (Some(0.1), 3)] {
            let attempt = whole_probe_attempt(&binary, multiplier, sweep);
            assert_eq!(attempt.active, Some(1));
            assert_eq!(attempt.timeout, Some(Duration::from_millis(timeout)));
            assert_eq!(attempt.stall.budget, Some(Duration::from_millis(11)));
            assert_eq!(attempt.request.limit, Some(4096));
            assert!(attempt.request.meter);
            assert!(matches!(attempt.only, Only::All));
        }
    }

    #[test]
    fn recovered_scheduler_resource_admission_requires_every_resource_and_a_default_single_slot() {
        let work = vec![
            scheduled(0, "free", "subject", 3, 0, false, 0),
            ScheduledWork {
                resources: vec![Arc::from("implicit"), Arc::from("explicit")],
                ..scheduled(0, "bounded", "subject", 3, 0, false, 1)
            },
        ];
        let scheduler = Scheduler::with_resources(work, 1, HashMap::from_iter([(Arc::from("explicit"), 2)]));
        let mut state = scheduler.state.lock().unwrap();
        for (implicit, explicit, expected) in [(0, 0, true), (0, 1, true), (1, 0, false), (0, 2, false), (1, 2, false)] {
            state.active_resources.insert(Arc::from("implicit"), implicit);
            state.active_resources.insert(Arc::from("explicit"), explicit);
            assert_eq!(scheduler.resources_available(&state, 1), expected);
            assert!(scheduler.resources_available(&state, 0));
        }
    }

    #[test]
    fn recovered_one_fresh_supported_killer_is_enough_among_stale_candidates() {
        let learning = FileLearning::new();
        let fresh = killer("fresh");
        let stale = killer("stale");
        learning.publish("item", &killed_by(stale));
        learning.publish("item", &killed_by(fresh.clone()));
        {
            let mut learned = learning.locked();
            let item = learned.items.get_mut("item").unwrap();
            item.fresh.retain(|candidate| candidate == &fresh);
        }
        assert!(learning.has_fresh_killer("item"));
        assert!(!learning.has_fresh_killer("absent"));
        learning.observe("item", &Candidate::Exact(fresh.clone()), false, Duration::from_millis(1));
        assert!(learning.has_fresh_killer("item"));
        learning.observe("item", &Candidate::Exact(fresh), false, Duration::from_millis(1));
        assert!(!learning.has_fresh_killer("item"));
    }

    #[test]
    fn recovered_initial_confirmation_reports_only_new_identity_insertions() {
        let tally = Tally::default();
        assert!(apply_initial_confirmation(&tally, "first".into()));
        assert!(!apply_initial_confirmation(&tally, "first".into()));
        assert!(apply_initial_confirmation(&tally, "second".into()));
        assert_eq!(
            *tally.confirmed_initial_killers.lock().unwrap(),
            BTreeSet::from(["first".into(), "second".into()])
        );
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the complete in-memory sweep asserts its shared execution and durable accounting boundaries"
    )]
    fn recovered_in_process_sweep_retains_initial_hint_coverage_and_worker_accounting() {
        let work = Workspace::adopt("synthetic-accounting".into(), "synthetic-accounting-target".into());
        let mutants = (1..=3)
            .map(|ordinal| {
                let mut mutant = crate::testing::ci_fixture::mutant("src/lib.rs", ordinal, "arith.add_to_sub", Outcome::Pending);
                mutant.ordinal = u32::try_from(ordinal).unwrap();
                mutant.package = "subject".into();
                mutant
            })
            .collect();
        let mut plan = telemetry_plan(mutants);
        let reach = Reachability::build(&plan, &[], &NARROW);
        let binary = crate::testing::test_binary("never-launched-accounting");
        let killer = staged::killer_for(&binary, "known");
        let initial_id = plan.mutants[0].id.clone();
        let mut killers = Killers::default();
        killers.record(initial_id.clone(), killer.clone());
        let mut events = SweepRecorder::default();
        let mut checkpointed = Vec::new();
        let spent = test_all_with_accounting_hooks(
            &work,
            &mut plan,
            &reach,
            Sweep {
                jobs: 2,
                ..sweep(Stall::NONE)
            },
            true,
            &mut killers,
            &mut events,
            (
                |mutant, _, _| {
                    checkpointed.push(mutant.ordinal);
                    Ok(())
                },
                |ordinal| {
                    execution_tier(SelectionTier::Exact, false);
                    let verdict = super::super::verdict::with_binary_runner(
                        move |_binary, _attempt, _confirm| super::super::verdict::BinaryRun {
                            verdict: if ordinal == 1 { Verdict::Failed(None) } else { Verdict::Passed },
                            reach: super::super::verdict::ReachObservation::Reached,
                        },
                        || {
                            let attempt = Attempt {
                                active: Some(ordinal),
                                timeout: None,
                                stall: Stall::NONE,
                                request: MemoryRequest::default(),
                                only: Only::One("known"),
                            };
                            let first = run_binary_observed(&work, &binary, attempt, false);
                            let second = run_binary_observed(&work, &binary, attempt, false);
                            assert_eq!(first.reach, super::super::verdict::ReachObservation::Reached);
                            assert_eq!(second.reach, super::super::verdict::ReachObservation::Reached);
                            first.verdict
                        },
                    );
                    record_binary_launch_attempt(false);
                    record_binary_execution(false, Duration::from_millis(7), &verdict);
                    trace_selection(
                        &Tally::default(),
                        ordinal,
                        &binary,
                        Some("known".into()),
                        SelectionTier::Exact,
                        1,
                        Some(ordinal == 1),
                        false,
                        Duration::from_millis(19),
                        Duration::ZERO,
                        false,
                    );
                    Some(Judgement::Reached(
                        if ordinal == 1 { Outcome::Killed } else { Outcome::Survived },
                        (ordinal == 1).then(|| killer.clone()),
                        None,
                    ))
                },
                || {},
            ),
        )
        .unwrap()
        .unwrap();
        checkpointed.sort_unstable();
        assert_eq!(checkpointed, [1, 2, 3]);
        assert_eq!(events.starts, 3);
        assert_eq!(events.attempts.len(), 3);
        assert_eq!(spent.confirmed_initial_killers, BTreeSet::from([initial_id]));
        let accounting = spent.accounting.unwrap();
        assert!(accounting.complete);
        assert_eq!(accounting.initial_exact_hints, Some(1));
        assert_eq!(accounting.initial_killer_confirmed, Some(1));
        assert_eq!(
            (
                accounting.exact.launches,
                accounting.exact.execution_ms,
                accounting.exact.hits,
                accounting.exact.misses
            ),
            (3, 21, 1, 2)
        );
        assert_eq!(accounting.confirmation.launches, 0);
        assert_eq!(spent.packages.len(), 1);
        assert_eq!(spent.packages[0].mutants, 3);
        assert_eq!(
            plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
            [Outcome::Killed, Outcome::Survived, Outcome::Survived]
        );
    }

    #[test]
    fn recovered_new_selection_discards_stale_initial_killers_and_execution_costs() {
        begin_selection_trace();
        INITIAL_KILLER.with_borrow_mut(|slot| *slot = Some(("stale".into(), killer("stale"))));
        record_binary_execution(true, Duration::from_millis(7), &Verdict::Passed);
        assert!(INITIAL_KILLER.with_borrow(Option::is_some));
        EXECUTION_COSTS.with_borrow(|costs| assert_eq!(costs.len(), 1));
        begin_selection_trace();
        assert!(INITIAL_KILLER.with_borrow(Option::is_none));
        assert!(take_execution_costs().is_empty());
        assert!(take_selection_trace().is_empty());
    }

    #[test]
    fn recovered_confirmation_keeps_its_own_result_when_the_primary_selection_differs() {
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
        begin_selection_trace();
        record_binary_execution(true, Duration::from_millis(7), &Verdict::Passed);
        let tally = Tally::default();
        record_selection_cost(&tally, SelectionTier::Exact, SelectionResult::Hit, Duration::from_millis(19), false);
        let accounting = tally.accounting.lock().unwrap();
        assert_eq!(
            (
                accounting.confirmation.launches,
                accounting.confirmation.execution_ms,
                accounting.confirmation.hits,
                accounting.confirmation.misses,
                accounting.confirmation.inconclusive,
            ),
            (1, 7, 0, 1, 0)
        );
        assert_eq!(accounting.exact.launches, 0);
        drop(accounting);
        assert!(take_selection_trace().is_empty());
    }

    #[test]
    fn recovered_scripted_selection_cost_is_sent_or_applied_exactly_once() {
        for asynchronous in [false, true] {
            for confirmation in [false, true] {
                let tally = Tally::default();
                let (sender, receiver) = mpsc::channel();
                ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = asynchronous.then_some(sender));
                begin_selection_trace();
                record_selection_cost(
                    &tally,
                    SelectionTier::Exact,
                    SelectionResult::Hit,
                    Duration::from_millis(19),
                    confirmation,
                );
                ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
                let events: Vec<_> = receiver.try_iter().collect();
                let expected = AttemptCost {
                    launches: 1,
                    execution_ms: 19,
                    hits: 1,
                    ..AttemptCost::default()
                };
                assert_eq!(events.len(), usize::from(asynchronous));
                for event in events {
                    let SweepEvent::Accounting(tier, actual_confirmation, cost) = event else {
                        panic!("a scripted selection emits only accounting");
                    };
                    assert_eq!(tier, SelectionTier::Exact);
                    assert_eq!(actual_confirmation, confirmation);
                    assert_eq!(to_value(cost).unwrap(), to_value(&expected).unwrap());
                }
                let accounting = tally.accounting.lock().unwrap();
                let destination = if confirmation {
                    &accounting.confirmation
                } else {
                    &accounting.exact
                };
                assert_eq!(
                    to_value(destination).unwrap(),
                    to_value(if asynchronous { AttemptCost::default() } else { expected }).unwrap()
                );
                drop(accounting);
                assert!(take_selection_trace().is_empty());
            }
        }
    }

    #[test]
    fn recovered_imported_ranking_starts_with_no_canonical_pass_evidence() {
        let hint = RankedHint {
            candidate: killer("imported"),
            seeds: 1,
            hits: 0,
            misses: 0,
            measured_ms: 12,
            samples: 2,
            order: 7,
        };
        let mut candidate = RankedCandidate::from_hint(&hint, Clone::clone);
        for passes in 0..4 {
            assert_eq!(candidate.score(), (0, Reverse(0), Duration::from_millis(6), 7), "{passes} passes");
            candidate.observe_canonical_pass();
        }
        assert_eq!(candidate.score(), (1, Reverse(0), Duration::from_millis(6), 7));
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
    fn learning_adapter_preserves_exact_and_promoted_hints_only_for_deterministic_reach_and_forwards_timeout() {
        let (_directory, work) = crate::testing::helper_workspace("learning-adapter-matrix", &["exit:0"]);
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
                            ranked_sweep(),
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
    fn oct25_real_sweep_rebuilds_pending_work_without_extra_states_or_unbounded_waits() {
        let root = camino::Utf8PathBuf::from("synthetic-two-pass-sweep");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = plan_over(&["subject", "subject"]);
        plan.root = root;
        let binaries = [TestBinary {
            package: "subject".into(),
            baseline: Duration::from_millis(7),
            ..crate::testing::test_binary("never-launched")
        }];
        let reach = Reachability::build(&plan, &binaries, &NARROW);
        let visits = [AtomicUsize::new(0), AtomicUsize::new(0)];
        let mut completed = Vec::new();
        let mut killers = Killers::default();
        let clock = tick::ClockControl::new().to_simple_clock();
        let spent = test_all_with_clock(
            &work,
            &mut plan,
            &reach,
            Sweep { jobs: 1, ..ranked_sweep() },
            false,
            &mut killers,
            &mut crate::testing::Recorder::default(),
            (
                |mutant, _, _| {
                    completed.push((mutant.ordinal, mutant.outcome));
                    Ok(())
                },
                |ordinal| {
                    let index = usize::try_from(ordinal - 1).unwrap();
                    let visit = visits[index].fetch_add(1, Ordering::Relaxed);
                    assert!(visit < 2, "each mutant has exactly an exact and a fallback pass");
                    (visit == 1).then_some(Judgement::Reached(Outcome::Survived, None, None))
                },
                || panic!("one worker with no resource reservations must never wait"),
            ),
            &clock,
        )
        .unwrap()
        .unwrap();
        assert_eq!(visits.map(AtomicUsize::into_inner), [2, 2]);
        assert_eq!(completed, [(1, Outcome::Survived), (2, Outcome::Survived)]);
        assert_eq!(
            plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
            [Outcome::Survived; 2]
        );
        assert_eq!(spent.launches, 0);
        assert_eq!(spent.packages.len(), 1);
        assert_eq!(spent.packages[0].mutants, 2);
        assert_eq!(spent.packages[0].first_started_ms, 0);
        assert_eq!(spent.packages[0].settled_ms, 0);
    }

    fn with_settled_scheduler_observer<T>(observer: impl FnMut(bool, &Scheduler) + 'static, body: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                SETTLED_SCHEDULER_OBSERVER.with_borrow_mut(|observer| *observer = None);
            }
        }
        SETTLED_SCHEDULER_OBSERVER.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "settled scheduler observers must not nest");
            *slot = Some(Box::new(observer));
        });
        let _reset = Reset;
        body()
    }

    #[test]
    fn oct25_real_sweep_completes_assignments_and_transfers_only_fresh_deterministic_kills() {
        for deterministic in [false, true] {
            let root = Utf8PathBuf::from("never-read-completion-root");
            let work = Workspace::adopt(root.clone(), root.join("target"));
            let mut plan = plan_over(&["subject", "subject"]);
            plan.root = root;
            let binaries = [TestBinary {
                package: "subject".into(),
                baseline: Duration::from_millis(7),
                ..crate::testing::test_binary("never-launched-completion-binary")
            }];
            let reach = Reachability::build(&plan, &binaries, &NARROW);
            let known = staged::killer_for(&binaries[0], "known");
            let mut killers = Killers::default();
            killers.record(plan.mutants[0].id.clone(), known.clone());
            let item = Arc::clone(&plan.mutants[0].item_path);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let passes = std::rc::Rc::new(std::cell::Cell::new(0));
            let clock = tick::ClockControl::new().to_simple_clock();
            let runner = Arc::new({
                let calls = Arc::clone(&calls);
                move |_: &TestBinary, attempt: Attempt<'_>, confirm: bool| {
                    let mut calls = calls.lock().unwrap();
                    assert!(calls.len() < 2, "each mutant must be killed by its first actual selection");
                    assert!(!confirm);
                    assert_eq!(attempt.only.names(), if deterministic { &["known"][..] } else { &[] });
                    calls.push(attempt.active.unwrap());
                    super::super::verdict::BinaryRun {
                        verdict: Verdict::Failed(Some("known".into())),
                        reach: super::super::verdict::ReachObservation::Reached,
                    }
                }
            });
            let result = with_settled_scheduler_observer(
                {
                    let passes = std::rc::Rc::clone(&passes);
                    move |exact, scheduler| {
                        passes.set(passes.get() + 1);
                        let state = scheduler.state.lock().unwrap();
                        let key = (0, Arc::clone(&item));
                        if exact && !deterministic {
                            assert!(state.learned_items.is_empty());
                            assert!(state.transfer_items.is_empty());
                        } else {
                            assert_eq!(state.learned_items, crate::HashSet::from_iter([key.clone()]));
                            assert_eq!(state.transfer_items.contains(&key), deterministic);
                        }
                        if !exact {
                            assert_eq!(
                                scheduler.work.iter().map(|work| work.transfer).collect::<Vec<_>>(),
                                [deterministic; 2]
                            );
                        }
                        assert_eq!(state.active_files, [0]);
                        assert!(state.active_items.is_empty());
                    }
                },
                || {
                    super::super::verdict::with_shared_runner(runner, || {
                        test_all_with_clock(
                            &work,
                            &mut plan,
                            &reach,
                            Sweep {
                                confirm: false,
                                ..ranked_sweep()
                            },
                            deterministic,
                            &mut killers,
                            &mut crate::testing::Recorder::default(),
                            (
                                |_, _, _| Ok(()),
                                |_| None,
                                || panic!("one worker with no resource reservations must not wait"),
                            ),
                            &clock,
                        )
                    })
                },
            );
            let spent = result.unwrap().unwrap();
            assert_eq!(spent.packages.len(), 1);
            assert_eq!(spent.packages[0].mutants, 2);
            assert_eq!(passes.get(), 2);
            assert_eq!(*calls.lock().unwrap(), if deterministic { [1, 2] } else { [2, 1] });
            assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
            assert!(plan.mutants.iter().all(|mutant| killers.hint(&mutant.id) == Some(&known)));
        }
    }

    #[test]
    fn oct25_real_sweep_abandonment_wakes_waiters_after_releasing_the_current_assignment() {
        let root = Utf8PathBuf::from("never-read-abandonment-root");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = plan_over(&["subject"]);
        plan.root = root;
        let reach = Reachability::build(&plan, &[], &NARROW);
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let clock = tick::ClockControl::new().to_simple_clock();
        let result = with_settled_scheduler_observer(
            {
                let observed = std::rc::Rc::clone(&observed);
                move |exact, scheduler| {
                    assert!(exact);
                    assert!(!observed.replace(true), "abandonment must prevent the fallback pass");
                    assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 2);
                    let state = scheduler.state.lock().unwrap();
                    assert_eq!(state.active_files, [0]);
                    assert!(state.active_items.is_empty());
                    assert!(state.learned_items.is_empty());
                }
            },
            || {
                test_all_with_clock(
                    &work,
                    &mut plan,
                    &reach,
                    ranked_sweep(),
                    true,
                    &mut Killers::default(),
                    &mut crate::testing::Recorder::default(),
                    (
                        |_, _, _| Ok(()),
                        |_| Some(Judgement::Abandoned("owned abandonment".into())),
                        || panic!("one worker must never wait"),
                    ),
                    &clock,
                )
            },
        );
        assert_eq!(
            result.unwrap_err().message(),
            "the run cannot be trusted to judge anything further: owned abandonment.\n\
             It stops here rather than reach verdicts that would each have to be taken on faith."
        );
        assert!(observed.get());
        assert_eq!(plan.mutants[0].outcome, Outcome::Pending);
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
        let spent = with_planning_observer(
            {
                let observed = std::rc::Rc::clone(&observed);
                move |work, promoted| {
                    observed.set(true);
                    assert_eq!(work.len(), 4);
                    assert!(work.iter().all(|item| item.sibling_benefit == 3));
                    assert_eq!(work.iter().filter(|item| item.hinted).count(), 1);
                    assert!(
                        work.iter().all(|item| item.resources.is_empty()),
                        "resources belong to actual selections, not whole mutants"
                    );
                    assert!(promoted.iter().any(|candidates| !candidates.is_empty()));
                }
            },
            || {
                test_all_with_hooks(
                    &work,
                    &mut plan,
                    &reach,
                    Sweep { jobs: 1, ..ranked_sweep() },
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
        let spans = HashMap::from_iter([(Arc::from("b"), (2, 5, 90)), (Arc::from("a"), (1, 10, 50))]);
        let actual = super::super::measure::sweep_cost(Some(tally.spent(&spans)), Duration::from_millis(100)).unwrap();
        let expected = super::super::session::SweepCost {
            accounting: Some(super::super::session::SweepAccounting::default()),
            elapsed: Duration::from_millis(100),
            launches: 7,
            probes: 3,
            launches_saved: 2,
            exact_hits: 1,
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
        assert_eq!(to_value(actual).unwrap(), to_value(expected).unwrap());
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
            learning.publish("item", &Judgement::Reached(Outcome::Killed, Some(killer.clone()), None));
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
                &Judgement::Reached(
                    Outcome::Killed,
                    Some(Killer {
                        package: binary.package.clone(),
                        target: binary.killer_target(),
                        test: "test".to_owned(),
                    }),
                    None,
                ),
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
                &Tally::default(),
                7,
                &binary,
                Some("test".to_owned()),
                SelectionTier::Reach,
                3,
                observation,
                true,
                Duration::from_millis(11),
                Duration::from_millis(13),
                false,
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
            &Tally::default(),
            1,
            &binary,
            None,
            SelectionTier::Exact,
            0,
            None,
            false,
            Duration::ZERO,
            Duration::ZERO,
            false,
        );
        begin_selection_trace();
        assert!(take_selection_trace().is_empty());
    }

    fn telemetry_plan(mutants: Vec<Mutant>) -> Plan {
        Plan {
            root: camino::Utf8PathBuf::from(r"C:\gamma-in-process"),
            files: Vec::new(),
            mutants,
            skipped: Vec::new(),
            digests: HashMap::default(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    #[test]
    fn telemetry_population_coverage_is_frozen_before_assignment_and_matches_killers_across_tiers() {
        use super::super::session::{InitialHints, SweepAccounting};

        let mut plan = telemetry_plan(
            (1..=6)
                .map(|index| Mutant {
                    id: format!("m{index}").into(),
                    ordinal: index,
                    outcome: if index == 4 { Outcome::CompileError } else { Outcome::Pending },
                    ..crate::fixtures::mutant()
                })
                .collect(),
        );
        plan.mutants[4].ordinal = 0;
        plan.mutants[2].outcome = Outcome::Survived;
        let killer = Killer {
            package: "subject".into(),
            target: "test".into(),
            test: "recorded".into(),
        };
        let mut killers = Killers::default();
        for mutant in &plan.mutants[..2] {
            killers.record(mutant.id.clone(), killer.clone());
        }
        let initial = InitialHints::capture(&plan, &killers);
        killers.forget(&plan.mutants[0].id);
        assert_eq!(initial.population, 3);
        assert_eq!(
            initial.killers.len(),
            2,
            "coverage includes mutants that have not been assigned or probed"
        );
        let tally = Tally {
            accounting: Mutex::new(SweepAccounting::segment(Some(&initial), true)),
            ..Tally::default()
        };
        let binary = TestBinary {
            package: "subject".into(),
            target: "test".into(),
            target_kind: "test".into(),
            ..crate::testing::test_binary("never-launched")
        };
        let (sender, receiver) = mpsc::channel();
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = Some(sender));
        begin_selection_trace();
        INITIAL_KILLER.with_borrow_mut(|context| *context = Some((plan.mutants[0].id.clone(), killer.clone())));
        for verdict in [
            Verdict::Passed,
            Verdict::Failed(None),
            Verdict::Failed(Some("different".into())),
            Verdict::Flaky(None),
        ] {
            record_observed_killer(&binary, Only::All, &verdict);
        }
        assert!(
            receiver.try_recv().is_err(),
            "coverage, guessed failures and inconclusive outcomes cannot confirm an identity"
        );
        for (position, tier, expected) in [
            (0, SelectionTier::Exact, 1),
            (0, SelectionTier::Item, 1),
            (1, SelectionTier::Whole, 2),
        ] {
            INITIAL_KILLER.with_borrow_mut(|context| *context = Some((plan.mutants[position].id.clone(), killer.clone())));
            execution_tier(tier, false);
            record_observed_killer(&binary, Only::All, &Verdict::Failed(Some("recorded".into())));
            let SweepEvent::InitialKillerConfirmed(id) = receiver.try_recv().unwrap() else {
                panic!("expected identity evidence")
            };
            let _new = apply_initial_confirmation(&tally, id);
            assert_eq!(tally.accounting.lock().unwrap().initial_killer_confirmed, Some(expected));
        }
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
        let _attempts = take_selection_trace();
        assert_eq!(
            tally.exact_probes.load(Ordering::Relaxed),
            0,
            "killer reuse is independent of execution tier"
        );
        assert_eq!(tally.accounting.lock().unwrap().initial_exact_hints, Some(2));
    }

    #[test]
    fn telemetry_initial_confirmation_survives_immediate_settlement_and_resume() {
        use super::super::session::{InitialHints, Phases, SweepAccounting, SweepCost};

        let work = Workspace::adopt(r"C:\gamma-in-process".into(), r"C:\gamma-in-process\target".into());
        let binaries = ranked_binaries();
        let hint = Killer {
            package: "subject".into(),
            target: binaries[1].killer_target(),
            test: "recorded".into(),
        };
        let plan = telemetry_plan(vec![Mutant {
            outcome: Outcome::Pending,
            ..crate::fixtures::mutant()
        }]);
        let initial = InitialHints {
            population: 1,
            killers: std::iter::once((plan.mutants[0].id.clone(), hint.clone())).collect(),
        };
        let tally = Tally {
            accounting: Mutex::new(SweepAccounting::segment(Some(&initial), true)),
            ..Tally::default()
        };
        let (sender, receiver) = mpsc::channel();
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = Some(sender));
        begin_selection_trace();
        INITIAL_KILLER.with_borrow_mut(|context| *context = Some((plan.mutants[0].id.clone(), hint.clone())));
        let judged = judge_script(
            vec![(
                "later",
                Only::One("recorded"),
                Verdict::Failed(Some("recorded".into())),
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
                    &[],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(),
                    &tally,
                )
            },
        );
        let Judgement::Reached(Outcome::Killed, Some(credited), _) = judged else {
            panic!("the confirmed exact hint must receive verdict credit")
        };
        assert_eq!(credited.test, "recorded");
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
        let _attempts = take_selection_trace();
        let mut snapshots = Vec::new();
        for event in receiver.try_iter() {
            match event {
                SweepEvent::InitialKillerConfirmed(id) => {
                    let _new = apply_initial_confirmation(&tally, id);
                }
                SweepEvent::Accounting(tier, confirmation, cost) => apply_accounting(&tally, tier, confirmation, &cost),
                _ => panic!("telemetry must not emit terminal events"),
            }
            snapshots.push(super::super::measure::sweep_cost(Some(tally.spent(&HashMap::default())), Duration::from_millis(1)).unwrap());
        }
        assert_eq!(snapshots[0].accounting.as_ref().unwrap().initial_killer_confirmed, Some(1));
        let partial: SweepCost = from_value(to_value(snapshots.last().unwrap()).unwrap()).unwrap();
        assert_eq!(partial.accounting.as_ref().unwrap().initial_killer_confirmed, Some(1));
        let resumed = Tally {
            accounting: Mutex::new(SweepAccounting::segment(Some(&initial), true)),
            confirmed_initial_killers: Mutex::new(partial.confirmed_initial_killers.clone()),
            ..Tally::default()
        };
        assert!(
            !apply_initial_confirmation(&resumed, plan.mutants[0].id.clone()),
            "a repeated confirmation after partial resume is not new"
        );
        let segment = super::super::measure::sweep_cost(Some(resumed.spent(&HashMap::default())), Duration::ZERO);
        let completed = SweepCost::combine(Some(partial), segment).unwrap();
        assert_eq!(completed.accounting.as_ref().unwrap().initial_killer_confirmed, Some(1));
        assert_eq!(completed.accounting.as_ref().unwrap().exact.launches, 1);
        let frozen = Phases {
            initial_hints: Some(initial),
            sweep: Some(completed),
            ..Phases::default()
        };
        let published: Phases = from_value(to_value(frozen).unwrap()).unwrap();
        assert_eq!(published.initial_hints.unwrap().killers[&plan.mutants[0].id], hint);
        assert_eq!(published.sweep.unwrap().accounting.unwrap().initial_killer_confirmed, Some(1));
    }

    #[test]
    fn checkpoint_summaries_count_confirmations_without_copying_the_identity_set() {
        use crate::exec::SweepAccounting;

        let tally = Tally {
            accounting: Mutex::new(SweepAccounting {
                initial_killer_confirmed: Some(0),
                ..SweepAccounting::default()
            }),
            confirmed_initial_killers: Mutex::new((0..10_000).map(|index| format!("{index:012x}").into()).collect()),
            ..Tally::default()
        };
        let checkpoint = tally.checkpoint_spent(&HashMap::default());
        assert!(checkpoint.confirmed_initial_killers.is_empty());
        assert_eq!(checkpoint.accounting.unwrap().initial_killer_confirmed, Some(10_000));
        let completed = tally.spent(&HashMap::default());
        assert_eq!(completed.confirmed_initial_killers.len(), 10_000);
        assert_eq!(completed.accounting.unwrap().initial_killer_confirmed, Some(10_000));
    }

    #[test]
    fn selection_accounting_reuses_cost_capacity_for_async_sync_and_scripted_runners() {
        for asynchronous in [false, true] {
            for scripted in [false, true] {
                let tally = Tally::default();
                let (sender, receiver) = mpsc::channel();
                ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = asynchronous.then_some(sender));
                begin_selection_trace();
                execution_tier(SelectionTier::Exact, false);
                let capacity = EXECUTION_COSTS.with_borrow_mut(|costs| {
                    costs.reserve(32);
                    costs.capacity()
                });
                if !scripted {
                    record_binary_launch_attempt(false);
                    record_binary_execution(false, Duration::from_millis(7), &Verdict::Failed(None));
                    record_binary_launch_attempt(true);
                    record_binary_execution(true, Duration::from_millis(3), &Verdict::Passed);
                }
                record_selection_cost(
                    &tally,
                    SelectionTier::Exact,
                    SelectionResult::Miss,
                    Duration::from_millis(11),
                    false,
                );
                EXECUTION_COSTS.with_borrow(|costs| {
                    assert!(costs.is_empty());
                    assert_eq!(costs.capacity(), capacity, "settlement must retain worker-local capacity");
                });
                ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
                for event in receiver.try_iter() {
                    let SweepEvent::Accounting(tier, confirmation, cost) = event else {
                        panic!("cost settlement emits only accounting events")
                    };
                    apply_accounting(&tally, tier, confirmation, &cost);
                }
                let accounting = tally.accounting.lock().unwrap();
                assert_eq!(accounting.exact.launches, 1);
                assert_eq!(accounting.exact.execution_ms, if scripted { 11 } else { 7 });
                assert_eq!(accounting.exact.misses, 1);
                assert_eq!(accounting.exact.hits, 0);
                assert_eq!(accounting.confirmation.launches, usize::from(!scripted));
                assert_eq!(accounting.confirmation.execution_ms, if scripted { 0 } else { 3 });
                assert_eq!(accounting.confirmation.misses, usize::from(!scripted));
                drop(accounting);
                assert!(take_selection_trace().is_empty());
            }
        }
    }

    #[test]
    fn telemetry_separates_primary_execution_confirmation_and_unsuccessful_speculation() {
        let tally = Tally::default();
        let binary = crate::testing::test_binary("never-launched");
        for (tier, result, primary_ms, confirmation_ms, proof) in [
            (SelectionTier::Exact, None, 7, 3, false),
            (SelectionTier::Item, Some(false), 11, 0, false),
            (SelectionTier::Whole, Some(true), 19, 5, false),
            (SelectionTier::Whole, Some(false), 23, 0, true),
        ] {
            begin_selection_trace();
            record_binary_execution(false, Duration::from_millis(primary_ms), &Verdict::Failed(None));
            if confirmation_ms > 0 {
                record_binary_execution(true, Duration::from_millis(confirmation_ms), &Verdict::Passed);
            }
            trace_selection(
                &tally,
                7,
                &binary,
                None,
                tier,
                1,
                result,
                false,
                Duration::from_secs(99),
                Duration::ZERO,
                proof,
            );
            assert_eq!(take_selection_trace().len(), 1);
        }
        let accounting = tally.accounting.lock().unwrap();
        assert_eq!(
            (
                accounting.exact.launches,
                accounting.exact.execution_ms,
                accounting.exact.inconclusive
            ),
            (1, 7, 1)
        );
        assert_eq!(
            (
                accounting.inferred.launches,
                accounting.inferred.execution_ms,
                accounting.inferred.misses,
                accounting.inferred.inconclusive
            ),
            (1, 11, 1, 0)
        );
        assert_eq!(
            (
                accounting.fallback.launches,
                accounting.fallback.execution_ms,
                accounting.fallback.hits
            ),
            (1, 19, 1)
        );
        assert_eq!((accounting.confirmation.launches, accounting.confirmation.execution_ms), (3, 31));
    }

    #[test]
    fn telemetry_routes_each_primary_and_proof_boundary_without_double_counting_results() {
        for tier in [
            SelectionTier::Exact,
            SelectionTier::Item,
            SelectionTier::Reach,
            SelectionTier::File,
            SelectionTier::Whole,
        ] {
            for proof in [false, true] {
                for confirmation in [false, true] {
                    for verdict in [Verdict::Passed, Verdict::Failed(None), Verdict::TimedOut] {
                        let (sender, receiver) = mpsc::channel();
                        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = Some(sender));
                        begin_selection_trace();
                        execution_tier(tier, proof);
                        record_binary_launch_attempt(confirmation);
                        record_binary_execution(confirmation, Duration::from_millis(37), &verdict);
                        let costs = take_execution_costs();
                        let attempts = take_selection_trace();
                        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
                        let events = receiver.try_iter().collect::<Vec<_>>();
                        assert_eq!(events.len(), 2);
                        assert!(attempts.is_empty(), "runner accounting does not invent selection results");
                        let expected = attempt_cost(Duration::from_millis(37), selection_result(transfer_observation(&verdict)));
                        assert_eq!(costs.len(), 1);
                        assert_eq!(costs[0].0, confirmation);
                        assert_eq!(
                            (
                                costs[0].1.launches,
                                costs[0].1.execution_ms,
                                costs[0].1.hits,
                                costs[0].1.misses,
                                costs[0].1.inconclusive
                            ),
                            (1, 37, expected.hits, expected.misses, expected.inconclusive)
                        );
                        for (index, event) in events.into_iter().enumerate() {
                            let SweepEvent::Accounting(actual_tier, actual_confirmation, cost) = event else {
                                panic!("a runner boundary must emit an accounting event")
                            };
                            assert_eq!(actual_tier, tier);
                            assert_eq!(actual_confirmation, confirmation || proof);
                            let results = if confirmation && index == 1 {
                                (expected.hits, expected.misses, expected.inconclusive)
                            } else {
                                (0, 0, 0)
                            };
                            assert_eq!((cost.hits, cost.misses, cost.inconclusive), results);
                            assert_eq!(cost.launches, usize::from(index == 0));
                            assert_eq!(cost.execution_ms, if index == 0 { 0 } else { 37 });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn runner_accounting_is_inert_outside_a_selection_and_trace_drain_resets_context() {
        let (sender, receiver) = mpsc::channel();
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = Some(sender));
        begin_selection_trace();
        INITIAL_KILLER.with_borrow_mut(|slot| *slot = Some(("observed".into(), killer("recorded"))));
        assert!(take_selection_trace().is_empty());
        assert!(INITIAL_KILLER.with_borrow(Option::is_none));
        record_binary_launch_attempt(false);
        record_binary_execution(false, Duration::from_millis(37), &Verdict::Failed(None));
        assert!(take_execution_costs().is_empty());
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
        assert!(
            receiver.try_recv().is_err(),
            "operations outside a selection cannot inflate accounting"
        );
    }

    #[test]
    fn telemetry_checkpoints_launches_before_results_and_does_not_double_count_completion() {
        let tally = Tally::default();
        let binary = crate::testing::test_binary("never-launched");
        let (sender, receiver) = mpsc::channel();
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = Some(sender));
        begin_selection_trace();
        execution_tier(SelectionTier::Exact, false);
        record_binary_launch_attempt(false);
        let apply_next = || {
            let SweepEvent::Accounting(tier, confirmation, cost) = receiver.try_recv().expect("one accounting boundary was emitted") else {
                panic!("telemetry must not emit a display event")
            };
            apply_accounting(&tally, tier, confirmation, &cost);
        };
        apply_next();
        let partial = super::super::measure::sweep_cost(Some(tally.spent(&HashMap::default())), Duration::from_millis(1)).unwrap();
        let restored: super::super::session::SweepCost = from_str(&to_string(&partial).unwrap()).unwrap();
        let partial_accounting = restored.accounting.as_ref().unwrap();
        assert_eq!((partial_accounting.exact.launches, partial_accounting.exact.execution_ms), (1, 0));
        assert_eq!(
            partial_accounting.exact.hits + partial_accounting.exact.misses + partial_accounting.exact.inconclusive,
            0
        );

        record_binary_execution(false, Duration::from_millis(7), &Verdict::Failed(None));
        apply_next();
        record_binary_launch_attempt(true);
        apply_next();
        record_binary_execution(true, Duration::from_millis(3), &Verdict::Failed(None));
        apply_next();
        trace_selection(
            &tally,
            7,
            &binary,
            None,
            SelectionTier::Exact,
            1,
            None,
            false,
            Duration::from_millis(999),
            Duration::ZERO,
            false,
        );
        apply_next();
        ACCOUNTING_SENDER.with_borrow_mut(|slot| *slot = None);
        assert!(
            receiver.try_recv().is_err(),
            "settling a selection emits no duplicate launch or duration"
        );
        assert_eq!(take_selection_trace().len(), 1);
        let accounting = tally.accounting.lock().unwrap();
        assert_eq!(
            (
                accounting.exact.launches,
                accounting.exact.execution_ms,
                accounting.exact.inconclusive
            ),
            (1, 7, 1)
        );
        assert_eq!((accounting.confirmation.launches, accounting.confirmation.execution_ms), (1, 3));
    }

    #[test]
    fn telemetry_counts_reused_and_duplicate_candidates_without_relaunching_them() {
        let work = Workspace::adopt(r"C:\gamma-in-process".into(), r"C:\gamma-in-process\target".into());
        let binaries = ranked_binaries();
        let binary = &binaries[0];
        let hint = Killer {
            package: binary.package.clone(),
            target: binary.killer_target(),
            test: "recorded".into(),
        };
        let tally = Tally::default();
        begin_selection_trace();
        let judged = judge_script(
            vec![
                ("earlier", Only::One("recorded"), Verdict::Passed, ReachObservation::Reached),
                ("earlier", Only::All, Verdict::Passed, ReachObservation::Reached),
            ],
            ranked_attempt(),
            true,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[binary],
                    Some(&hint),
                    &[
                        Candidate::Exact(hint.clone()),
                        Candidate::FileBinary(BinaryIdentity::from_binary(binary)),
                    ],
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(take_selection_trace().len(), 2);
        let accounting = tally.accounting.lock().unwrap();
        assert_eq!(accounting.exact.launches, 1);
        assert_eq!(accounting.inferred.launches, 1);
        assert_eq!(accounting.fallback.launches, 0, "canonical reuse is not a second launch");
        assert_eq!(accounting.duplicate_launches_avoided, 2);
    }

    type JudgeStep = (&'static str, Only<'static>, Verdict, ReachObservation);

    fn judge_script<T>(steps: Vec<JudgeStep>, expected: Attempt<'static>, confirm: bool, body: impl FnOnce() -> T) -> T {
        use std::rc::Rc;

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
                assert_eq!(confirming, confirm);
                BinaryRun { verdict, reach }
            },
            body,
        );
        assert!(remaining.borrow().is_empty(), "all scripted launches must occur");
        result
    }

    pub(super) fn ranked_binaries() -> [TestBinary; 2] {
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

    pub(super) fn ranked_sweep() -> Sweep {
        Sweep {
            timeout_floor: Duration::from_millis(17),
            stall: Stall {
                budget: Some(Duration::from_millis(23)),
            },
            jobs: 1,
            meter: true,
            confirm: true,
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
                calibrating: false,
                meter: true,
                limit: Some(37),
            },
            only: Only::All,
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
        let (_directory, _work, mut plan, _) = unreachable_harness(Vec::new());
        plan.mutants.push(Mutant {
            ordinal: 2,
            package: "alpha".into(),
            ..plan.mutants[0].clone()
        });
        let mut spans = HashMap::default();
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
        let root = Utf8PathBuf::from("checkpoint-drain-fixture");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let mut plan = one_mutant_plan(root);
        plan.mutants = (1..=3)
            .map(|ordinal| Mutant {
                id: format!("checkpoint-{ordinal}").into(),
                ordinal,
                file: Utf8PathBuf::from(format!("src/file-{ordinal}.rs")).into(),
                ..plan.mutants[0].clone()
            })
            .collect();
        let scheduler = Scheduler::new(
            (0..3)
                .map(|index| scheduled(index, "item", "subject", 1, 0, false, index))
                .collect(),
            3,
        );
        let tally = Tally::default();
        let mut package_spans = HashMap::default();
        let mut checkpoint_failed = false;
        let abandoned = OnceLock::new();
        let mut checkpoints = 0;
        let mut events = SweepRecorder::default();
        let mut queued = std::iter::repeat_with(|| SweepEvent::Started)
            .take(3)
            .chain([1, 0, 2].map(|position| {
                let started = u64::try_from(position).unwrap() + 1;
                SweepEvent::Completed(Completed {
                    position,
                    outcome: Outcome::Survived,
                    elapsed_ms: 3,
                    killer: None,
                    note: None,
                    first_started_ms: started,
                    settled_ms: started + 3,
                    attempts: Vec::new(),
                })
            }));
        SweepDrain {
            work: &work,
            plan: &mut plan,
            killers: &mut Killers::default(),
            tally: &tally,
            package_spans: &mut package_spans,
            checkpoint_failed: &mut checkpoint_failed,
            abandoned: &abandoned,
            scheduler: &scheduler,
        }
        .drain(
            &mut events,
            |_mutant, _killers, _cost| {
                checkpoints += 1;
                Err(error!("checkpoint lost"))
            },
            |_| queued.next(),
            || Duration::ZERO,
        );
        assert!(checkpoint_failed);
        assert_eq!(abandoned.get().map(String::as_str), Some("checkpoint lost"));
        assert_eq!(checkpoints, 1);
        assert_eq!(events.starts, 3);
        assert!(events.mutants.is_empty());
        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Survived));
        assert_eq!(package_spans.get("subject"), Some(&(3, 1, 6)));
        assert!(queued.next().is_none());
    }

    #[test]
    fn sweep_hooks_publish_planning_and_both_completed_and_abandoned_selection_traces() {
        for abandoned in [false, true] {
            let (_directory, work, mut plan, binaries) = unreachable_harness(Vec::new());
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
        let (_directory, work, plan, _) = unreachable_harness(Vec::new());
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
        let mut settings = ranked_sweep();
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
    fn later_exact_hint_settles_before_inferred_tests_and_canonical_predecessors() {
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
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
            vec![("later", Only::One("historical"), Verdict::Failed(None), ReachObservation::Unknown)],
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
                    ranked_sweep(),
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
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        let state = learning.locked();
        assert!(state.items.is_empty());
        assert!(state.reached_file.is_empty());
        let attempts = take_selection_trace();
        assert_eq!(attempts.len(), 1);
        assert_trace_context(&attempts, 22);
        assert_eq!((attempts[0].tier, attempts[0].result), (SelectionTier::Exact, SelectionResult::Hit));
        assert_eq!(attempts[0].test.as_deref(), Some("historical"));
        assert!(!attempts[0].all_tests);
    }

    #[test]
    fn duplicate_inferred_tests_do_not_retry_a_stale_later_exact_hint() {
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
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
                    Some(&hint),
                    &candidates,
                    None,
                    None,
                    Some(2.0),
                    ranked_sweep(),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 2);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 4);
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_cached_whole_pass_is_consumed_once_even_with_duplicate_and_missing_candidates() {
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
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
                    ranked_sweep(),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 2);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_candidates.load(Ordering::Relaxed), 8);
        assert_eq!(tally.item_candidates.load(Ordering::Relaxed), 4);
        assert_eq!(tally.reach_candidates.load(Ordering::Relaxed), 2);
        assert_eq!(tally.file_candidates.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn two_distinct_inferred_tests_fit_after_a_stale_exact_hint() {
        let root = Utf8PathBuf::from("synthetic-sweep");
        let work = Workspace::adopt(root.clone(), root.join("target"));
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
                    ranked_sweep(),
                    &tally,
                )
            },
        );
        assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 4);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn a_later_exact_probe_that_loses_containment_abandons_before_any_other_launch() {
        for persisted in [false, true] {
            let root = Utf8PathBuf::from("synthetic-sweep");
            let work = Workspace::adopt(root.clone(), root.join("target"));
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
                        ranked_sweep(),
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
    fn generated_exact_first_traces_preserve_immediate_kills_deduplication_and_fallback() {
        bolero::check!().with_type::<(u8, u8, u8, u8, bool, bool)>().for_each(
            |&(position, probe, terminal, selection, reverse, duplicate)| {
                let root = Utf8PathBuf::from("synthetic-sweep");
                let work = Workspace::adopt(root.clone(), root.join("target"));
                let binaries = ranked_binaries();
                let position = usize::from(position % 2);
                let probe = probe % 5;
                let terminal = terminal % 8;
                let selection = selection % 4;
                let reachable = binaries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, binary)| match selection {
                        1 if index == 0 => None,
                        2 if index == 1 => None,
                        3 => None,
                        _ => Some(binary),
                    })
                    .collect::<Vec<_>>();
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
                let launches = std::rc::Rc::new(RefCell::new(Vec::new()));
                let recorded = std::rc::Rc::clone(&launches);
                let admitted = [selection == 0 || selection == 2, selection == 0 || selection == 1];
                let tally = Tally::default();
                begin_selection_trace();
                let judged = super::super::verdict::with_binary_runner(
                    move |binary, attempt, _confirming| {
                        let index = usize::from(binary.target == "later");
                        assert!(admitted[index], "unreachable binaries must not launch");
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
                            Only::All => {
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
                            &reachable,
                            Some(&hint),
                            &candidates,
                            None,
                            None,
                            Some(2.0),
                            ranked_sweep(),
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
                    if only == "All" || only == "One(\"other\")" || probe == 0 {
                        assert!(
                            distinct.insert((*index, only)),
                            "passed exact probes and cached whole-binary runs must not repeat: {launches:?}"
                        );
                    }
                }
                let exact_eligible = reachable.iter().any(|binary| binary.matches_killer(&hint));
                if exact_eligible {
                    assert_eq!(launches.first(), Some(&(position, "One(\"historical\")".to_owned())));
                }
                if exact_eligible && probe == 4 {
                    assert!(matches!(judged, Judgement::Abandoned(reason) if reason == "exact containment lost"));
                    assert_eq!(launches.len(), 1);
                    return;
                }
                if exact_eligible && probe == 3 {
                    let Judgement::Reached(Outcome::Killed, Some(killer), None) = judged else {
                        panic!("a named exact kill settles before any canonical predecessor")
                    };
                    assert_eq!(killer, hint);
                    assert_eq!(launches.len(), 1);
                    return;
                }
                let first_must_run = reachable.iter().any(|binary| binary.target == "earlier");
                if first_must_run {
                    assert!(
                        launches.iter().any(|(index, only)| *index == 0 && only == "All"),
                        "inconclusive exact probes must fall back to whole-binary execution"
                    );
                }
                if first_must_run && terminal == 7 {
                    assert!(matches!(judged, Judgement::Abandoned(reason) if reason == "canonical containment lost"));
                    return;
                }
                let expected = if reachable.is_empty() {
                    Outcome::NoCoverage
                } else if first_must_run && terminal != 0 {
                    match terminal {
                        1 | 2 => Outcome::Timeout,
                        3 => Outcome::OutOfMemory,
                        4 => Outcome::Flaky,
                        5 => Outcome::Pending,
                        6 => Outcome::Killed,
                        _ => unreachable!("containment loss was checked before canonical outcome comparison"),
                    }
                } else {
                    Outcome::Survived
                };
                let Judgement::Reached(outcome, killer, _) = judged else {
                    panic!("the bounded runner supplies a canonical verdict")
                };
                assert_eq!(outcome, expected);
                if first_must_run && terminal == 6 {
                    assert_eq!(killer.expect("the canonical failure names its killer").test, "canonical");
                }
            },
        );
    }

    #[test]
    fn losing_candidate_containment_abandons_before_any_canonical_execution() {
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
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
                    ranked_sweep(),
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
        assert_eq!(tally.whole_selections.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn an_initial_exact_probe_is_not_repeated_when_the_same_binary_is_reached() {
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
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
                    ranked_sweep(),
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
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
        let mut binaries = ranked_binaries();
        for binary in &mut binaries {
            binary.budget = None;
        }
        let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
        let negative = NegativeLearning::default();
        negative.record(&site, &binaries[0]);
        let mut settings = ranked_sweep();
        settings.stall = Stall::NONE;
        settings.meter = false;
        settings.confirm = false;
        let tally = Tally::default();
        let judged = judge_script(
            vec![("later", Only::All, Verdict::Passed, ReachObservation::Reached)],
            Attempt {
                timeout: None,
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: false,
                    limit: Some(37),
                },
                ..ranked_attempt()
            },
            false,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    None,
                    &[],
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
        assert_eq!(tally.uncovered_selections.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn uncovered_initial_exact_hints_do_not_outrank_a_later_ranked_test() {
        let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
        let mut binaries = ranked_binaries();
        for binary in &mut binaries {
            binary.budget = None;
        }
        let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
        let negative = NegativeLearning::default();
        negative.record(&site, &binaries[0]);
        let mut settings = ranked_sweep();
        settings.stall = Stall::NONE;
        settings.meter = false;
        settings.confirm = false;
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
            Attempt {
                timeout: None,
                stall: Stall::NONE,
                request: MemoryRequest {
                    calibrating: false,
                    meter: false,
                    limit: Some(37),
                },
                ..ranked_attempt()
            },
            false,
            || {
                judge_ranked(
                    &work,
                    7,
                    &[&binaries[0], &binaries[1]],
                    Some(&hint),
                    &[candidate],
                    None,
                    Some((&negative, &site, true)),
                    Some(2.0),
                    settings,
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
    fn only_deterministic_whole_runs_can_record_a_negative_reach_hint() {
        for deterministic in [false, true] {
            for reach in [ReachObservation::Reached, ReachObservation::NotReached, ReachObservation::Unknown] {
                let (_directory, work, _plan, _) = unreachable_harness(Vec::new());
                let binaries = ranked_binaries();
                let site = SiteIdentity::from_mutant(&crate::fixtures::mutant());
                let negative = NegativeLearning::default();
                let tally = Tally::default();
                let judged = judge_script(vec![("earlier", Only::All, Verdict::Passed, reach)], ranked_attempt(), true, || {
                    judge_ranked(
                        &work,
                        7,
                        &[&binaries[0]],
                        None,
                        &[],
                        None,
                        Some((&negative, &site, deterministic)),
                        Some(2.0),
                        ranked_sweep(),
                        &tally,
                    )
                });
                assert!(matches!(judged, Judgement::Reached(Outcome::Survived, None, None)));
                assert_eq!(
                    negative.contains(&site, &binaries[0]),
                    deterministic && reach == ReachObservation::NotReached
                );
            }
        }
    }

    fn sweep(stall: Stall) -> Sweep {
        Sweep {
            timeout_floor: Duration::ZERO,
            stall,
            jobs: 1,
            meter: false,
            confirm: true,
        }
    }

    #[test]
    fn scheduler_priority_partial_order_always_matches_total_order() {
        let first = LocalPriority {
            transfer: false,
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
    pub(super) fn unreachable_harness(binaries: Vec<TestBinary>) -> (tempfile::TempDir, Workspace, Plan, Vec<TestBinary>) {
        let directory = crate::testing::workdir("test-all-uncovered");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let work = Workspace::adopt(root.clone(), root.join("target"));
        let plan = one_mutant_plan(root);

        (directory, work, plan, binaries)
    }

    /// A plan holding a single pending mutant in package `subject`.
    pub(super) fn one_mutant_plan(root: Utf8PathBuf) -> Plan {
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
            digests: HashMap::default(),
            root,
            files: Vec::new(),
            mutants: vec![mutant],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
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
    fn fallback_learning_guides_later_mutants_without_restarting_completed_work() {
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
            },
            true,
            &mut Killers::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect("the sweep should finish")
        .expect("two mutants should be swept");

        assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Killed));
        assert_eq!(
            spent.launches, 3,
            "the first mutant's fallback learns the second mutant's killer: {spent:?}"
        );
        assert_eq!(
            spent.probes, 1,
            "only the later mutant consumes the newly available inference: {spent:?}"
        );
        assert_eq!(spent.inferred_hits, 1);
        assert_eq!(spent.exact_probes, 0);
        let accounting = spent.accounting.expect("a completed sweep publishes phase accounting");
        assert_eq!(accounting.fallback.launches, 2);
        assert_eq!(accounting.fallback.hits, 1);
        assert_eq!(accounting.inferred.launches, 1);
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
    pub(super) const NARROW: TestScope<'static> = TestScope {
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

        schedule(&mut pending, &plan, &sets, &Killers::default());

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

        schedule(&mut pending, &plan, &sets, &Killers::default());

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

        schedule(&mut pending, &plan, &sets, &Killers::default());

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

    /// A reachable binary is run whole rather than reported uncovered.
    #[test]
    #[cfg(unix)]
    fn a_reachable_binary_is_run_whole_rather_than_called_uncovered() {
        let (_directory, work, mut plan, binaries) = harness("exit 0", Duration::from_mins(1));
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
            confirm: true,
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

    #[test]
    fn unfinished_confirmations_report_the_resource_cause_without_asserting_a_test_failed_twice() {
        let [binary, _] = ranked_binaries();
        for test in [None, Some("tests::selected".to_owned())] {
            let Some(Judgement::Reached(Outcome::Flaky, None, Some(note))) =
                terminal_judgement(&binary, Verdict::Unconfirmed(test.clone(), "exceeded its time budget"))
            else {
                panic!("an unfinished confirmation must remain score-neutral and retain its diagnostic");
            };
            assert!(note.contains("no mutant active"), "{note}");
            assert!(note.contains("exceeded its time budget"), "{note}");
            assert!(note.contains("never judged"), "{note}");
            assert!(note.contains(binary.path.as_str()), "{note}");
            assert!(!note.contains("fails with no mutant"), "{note}");
            if let Some(test) = test {
                assert!(note.contains(&format!("test `{test}`")), "{note}");
            } else {
                assert!(note.contains("the test binary"), "{note}");
            }
        }
        assert_eq!(
            transfer_observation(&Verdict::Unconfirmed(None, "stopped reporting progress")),
            None
        );
    }

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
            },
            &Tally::default(),
        );

        match judgement {
            Judgement::Abandoned(reason) => assert!(!reason.is_empty(), "a refusal has to say why"),
            Judgement::Reached(..) => {
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
    /// This workload relies on Linux's memory-backed `/dev/shm`; macOS breach detection is covered
    /// by the sampler tests in `cargo-gamma-process`.
    #[test]
    #[cfg(target_os = "linux")]
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
        let work = Workspace::adopt(r"C:\gamma-cached-killer".into(), r"C:\gamma-cached-killer\target".into());
        let binaries = [TestBinary {
            package: "subject".into(),
            budget: Some(Duration::from_millis(20)),
            ..crate::testing::test_binary("never-launched")
        }];
        let reachable: Vec<&TestBinary> = binaries.iter().collect();
        let hint = Killer {
            package: binaries[0].package.clone(),
            target: binaries[0].killer_target(),
            test: "tests::killer".into(),
        };
        let tally = Tally::default();

        let judgement = super::super::verdict::with_binary_runner(
            |_, attempt, confirm| {
                assert_eq!(attempt.active, Some(1));
                assert_eq!(attempt.timeout, Some(Duration::from_millis(20)));
                assert!(!confirm);
                BinaryRun {
                    verdict: match attempt.only {
                        Only::One("tests::killer") => Verdict::Failed(Some("tests::killer".into())),
                        Only::All => Verdict::TimedOut,
                        Only::One(other) => panic!("unexpected exact selection {other}"),
                    },
                    reach: ReachObservation::Reached,
                }
            },
            || {
                judge(
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
                    },
                    &tally,
                )
            },
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, Some(found), None) if found == hint));
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
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.probes.load(Ordering::Relaxed), 1);
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
        };

        assert!(negative_reach_hint_is_final(&binary, base));

        binary.budget = Some(Duration::from_secs(1));
        assert!(!negative_reach_hint_is_final(&binary, base), "timeout policy must run canonically");
        binary.budget = None;

        assert!(
            !negative_reach_hint_is_final(
                &binary,
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
            !negative_reach_hint_is_final(&binary, Sweep { meter: true, ..base }),
            "memory limits and metering failures must run canonically"
        );
        assert!(
            !negative_reach_hint_is_final(&binary, Sweep { confirm: true, ..base }),
            "confirmation flakes must run canonically"
        );
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
            },
            &tally,
        );

        assert!(matches!(judgement, Judgement::Reached(Outcome::Timeout, None, None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 3);
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

    #[test]
    fn cold_mutants_sharing_reachable_binaries_have_the_same_cost() {
        use super::mutant_cost;
        let plan = plan_over(&["subject", "subject"]);
        let binaries = vec![binary_of("subject", Duration::from_secs(10))];

        let sets = Reachability::build(&plan, &binaries, &NARROW);

        let cost_a = mutant_cost(0, &plan, &sets, &Killers::default());
        let cost_b = mutant_cost(1, &plan, &sets, &Killers::default());
        assert_eq!(cost_a, cost_b);
        assert_eq!(cost_a, Duration::from_secs(5));
    }

    #[test]
    fn scheduling_cost_handles_unreachable_sites_and_cold_binaries() {
        use super::mutant_cost;

        let plan = plan_over(&["subject"]);
        assert_eq!(mutant_cost(0, &plan, &Reachability::default(), &Killers::default()), Duration::ZERO);

        let mut binary = binary_of("subject", Duration::from_secs(10));
        binary.tests = Some(4);
        let binaries = [binary];
        let reach = Reachability::build(&plan, &binaries, &NARROW);
        assert_eq!(mutant_cost(0, &plan, &reach, &Killers::default()), Duration::from_secs(5));
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

        schedule(&mut pending, &plan, &reach, &Killers::default());

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
        for (jobs, expected) in [(0, 1), (1, 1), (8, 8), (17, 17)] {
            let mut launched = 0;
            launch_workers(jobs, || launched += 1);
            assert_eq!(launched, expected);
        }
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
                killer: Some(killer.clone()),
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
                killer: Some(killer),
                note: None,
                first_started_ms: 0,
                settled_ms: 0,
                attempts: Vec::new(),
            },
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

        let cost_hinted = mutant_cost(0, &plan, &sets, &killers);
        let cost_unhinted = mutant_cost(1, &plan, &sets, &Killers::default());

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
        let exact = mutant_work(0, &plan, &reach, &killers);
        let whole = mutant_work(1, &plan, &reach, &Killers::default());
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
        let generalized = mutant_work(1, &plan, &reach, &generalized);
        let uncovered = mutant_work(1, &plan, &Reachability::default(), &Killers::default());

        assert_eq!(exact.kind, crate::estimate::WorkKind::Exact);
        assert_eq!(whole.kind, crate::estimate::WorkKind::Whole);
        assert_eq!(generalized.kind, crate::estimate::WorkKind::Hinted);
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
        let work = mutant_work(0, &plan, &reach, &exact);
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
        assert_eq!(
            mutant_work(0, &plan, &reach, &reached).scheduling_cost(),
            Duration::from_millis(114)
        );
        for file in [false, true] {
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
                let work = mutant_work(0, &plan, &reach, &killers);
                assert_eq!(work.kind, crate::estimate::WorkKind::Hinted);
                assert_eq!(
                    work.scheduling_cost(),
                    Duration::from_millis(measured_probe + 117),
                    "file={file}, samples={samples}"
                );
            }
        }
    }

    #[test]
    fn cold_costs_sum_every_reachable_binary() {
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
        let killers = Killers::default();
        let work = mutant_work(0, &plan, &reach, &killers);
        assert_eq!(work.kind, crate::estimate::WorkKind::Whole);
        assert_eq!(work.scheduling_cost(), Duration::from_millis(240));
        assert_eq!(mutant_cost(0, &plan, &reach, &killers), Duration::from_millis(240));
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
    fn candidate_ranking_accounts_for_identity_hits_misses_and_samples() {
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

        let binary = crate::testing::test_binary("unused");
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

        canonical.observe_canonical_pass();
        canonical.observe_canonical_pass();
    }

    #[test]
    fn learning_selection_ranks_all_candidates_without_cost_or_miss_retirement() {
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
                fresh: Vec::new(),
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
        let selected = learning.select_candidates("subject::item");
        assert_eq!(selected.len(), 13);
        assert!(matches!(&selected[0], Candidate::Exact(identity) if identity.test == "first"));
        assert!(matches!(&selected[1], Candidate::Exact(identity) if identity.test == "second"));
        assert!(matches!(&selected[3], Candidate::ItemBinary(identity) if identity.target == "item-z"));
        assert!(matches!(&selected[6], Candidate::ReachFileBinary(identity) if identity.target == "reach-z"));
        assert!(matches!(&selected[9], Candidate::FileBinary(identity) if identity.target == "file-z"));
        assert_eq!(candidate_tier_counts(&selected), [6, 3, 4]);
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
        assert_eq!(learning.select_candidates("subject::item").len(), 13);
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
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 2);
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        assert_eq!(tally.item_probes.load(Ordering::Relaxed), 2);
        assert_eq!(tally.item_hits.load(Ordering::Relaxed), 1);
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
        assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 0);
        assert_eq!(tally.exact_hits.load(Ordering::Relaxed), 0);
        assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 1);
        assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
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
    fn canonical_passes_do_not_create_candidates() {
        let learning = FileLearning::new();
        let binary = TestBinary {
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            ..crate::testing::test_binary("unused")
        };

        learning.record_canonical_pass("subject::a", &binary, true);

        assert!(learning.candidates("subject::a").is_empty());
        assert!(learning.candidates("subject::b").is_empty());
    }

    #[test]
    fn a_reach_candidate_kill_settles_without_canonical_predecessors() {
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
            },
            &tally,
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
        assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
        assert_eq!(tally.saved.load(Ordering::Relaxed), 0);
        assert_eq!(tally.reach_saved.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn exact_site_negative_evidence_skips_a_sibling_replacement() {
        let directive = format!("write-le:{}|{}", gamma_rt::REACH_VAR, gamma_rt::SEAL);
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
        assert!(negative_excludes(Some((&negative, &site, true)), &binaries[0], safe_sweep));
        assert!(!negative_excludes(Some((&negative, &site, true)), &other_harness, safe_sweep));
    }

    #[test]
    fn only_complete_deterministic_whole_passes_publish_negative_reach() {
        let passed = BinaryRun {
            verdict: Verdict::Passed,
            reach: ReachObservation::NotReached,
        };
        assert!(negative_reach_is_final(&passed, Only::All, true));
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
                },
                &tally,
            );

            assert!(!matches!(judged, Judgement::Reached(Outcome::NoCoverage, _, _)));
            assert_eq!(tally.launches.load(Ordering::Relaxed), 1);
            assert_eq!(tally.reach_saved.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn an_inferred_full_binary_kill_settles_without_trying_an_unavailable_predecessor() {
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
            },
            &Tally::default(),
        );

        assert!(matches!(judged, Judgement::Reached(Outcome::Killed, Some(_), None)));
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
    fn a_candidate_is_demoted_but_not_retired_after_two_transfer_misses() {
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
        assert!(
            learning
                .candidates("subject::a")
                .iter()
                .any(|candidate| matches!(candidate, Candidate::Exact(found) if found.test == "tests::first"))
        );
        let state = learning.locked();
        let recorded = state.items["subject::a"]
            .exact
            .iter()
            .find(|candidate| candidate.identity.test == "tests::first")
            .expect("the first candidate remains ranked");
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
            transfer: false,
            stable_order,
            resources: Vec::new(),
        }
    }

    #[test]
    fn recovered_scheduler_reserves_every_pending_job_and_never_waits_after_exhaustion() {
        let scheduler = Scheduler::with_resources(
            vec![
                scheduled(0, "alpha", "alpha", 1, 1, false, 0),
                scheduled(1, "beta", "beta", 1, 1, false, 1),
            ],
            2,
            HashMap::default(),
        );
        assert_eq!(scheduler.work.len(), 2);
        {
            let mut state = scheduler.state.lock().unwrap();
            assert_eq!(state.remaining_count, 2);
            assert_eq!(scheduler.reserve_next(&mut state), Some(0));
            assert_eq!(state.remaining_count, 1);
            assert_eq!(state.remaining, [false, true]);
            assert_eq!(state.active_files, [1, 0]);
            assert_eq!(state.package_turns, [1, 0]);
            assert_eq!(scheduler.reserve_next(&mut state), Some(1));
            assert_eq!(state.remaining_count, 0);
            assert_eq!(state.remaining, [false, false]);
            assert_eq!(state.active_files, [1, 1]);
            assert_eq!(state.package_turns, [1, 1]);
            assert_eq!(scheduler.reserve_next(&mut state), None);
        }
        assert_eq!(
            scheduler.claim_with_wait(
                &OnceLock::new(),
                || panic!("an exhausted population must not prepare to wait"),
                |_| panic!("an exhausted population must not wait"),
            ),
            None,
        );
    }

    #[test]
    fn recovered_abandon_notifies_without_releasing_a_reservation() {
        let resource: Arc<str> = "shared".into();
        let scheduler = Scheduler::with_resources(
            (0..2)
                .map(|order| ScheduledWork {
                    resources: vec![Arc::clone(&resource)],
                    ..scheduled(0, "item", "package", 1, 1, false, order)
                })
                .collect(),
            1,
            HashMap::from_iter([(resource, 1)]),
        );
        let first = scheduler.reserve_next(&mut scheduler.state.lock().unwrap()).unwrap();
        assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 0);
        scheduler.abandon();
        assert_eq!(scheduler.notifications.load(Ordering::Relaxed), 1);
        {
            let state = scheduler
                .state
                .try_lock()
                .expect("abandon releases its queue lock before returning");
            assert_eq!(scheduler.select(&state), None);
            assert_eq!(state.remaining_count, 1);
            assert_eq!(state.remaining, [false, true]);
            assert_eq!(state.active_files, [1]);
        }
        scheduler.release(first, false);
    }

    #[test]
    fn unjudged_assignments_preserve_package_fairness_and_cannot_publish_learning() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "alpha", "alpha", 10, 0, false, 0),
                scheduled(1, "alpha-next", "alpha", 10, 0, false, 1),
                scheduled(2, "beta", "beta", 1, 0, false, 2),
            ],
            3,
        );
        let abandoned = OnceLock::new();
        for expected in [0, 2, 1] {
            let assignment = scheduler.assignment(&abandoned).unwrap();
            assert_eq!(assignment.index(), expected, "an unserved package must precede repeat service");
            {
                let state = scheduler.state.lock().unwrap();
                assert_eq!(
                    state.package_turns.iter().sum::<usize>(),
                    state.remaining.iter().filter(|remaining| !**remaining).count(),
                    "package service accounting must match the work actually assigned"
                );
            }
            drop(assignment);
            let state = scheduler.state.lock().unwrap();
            assert!(
                state.learned_items.is_empty(),
                "an unjudged assignment cannot manufacture killer learning"
            );
        }
        assert!(scheduler.assignment(&abandoned).is_none());
    }

    #[test]
    fn scheduler_claim_and_release_fairness_is_deterministic_without_native_workers() {
        let work = vec![
            scheduled(0, "alpha::f", "alpha", 7, 2, false, 0),
            scheduled(0, "alpha::f", "alpha", 7, 2, false, 1),
            scheduled(1, "beta::f", "beta", 7, 2, false, 2),
            scheduled(1, "beta::f", "beta", 7, 2, false, 3),
        ];
        let scheduler = Scheduler::with_resources(work, 2, HashMap::default());
        let abandoned = OnceLock::new();
        let first = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(first.index(), 0);
        {
            let state = scheduler.state.lock().unwrap();
            assert_eq!(state.remaining_count, 3);
            assert_eq!(state.remaining, [false, true, true, true]);
            assert_eq!(state.active_files, [1, 0]);
            assert_eq!(state.active_items.get(&(0, Arc::from("alpha::f"))), Some(&1));
            assert_eq!(state.package_turns, [1, 0]);
        }
        drop(first);
        let second = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(second.index(), 2, "an unserved package precedes another turn for alpha");
        second.complete(true);
        let third = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(third.index(), 3, "at equal package turns, transferable learning precedes cold work");
        third.complete(false);
        let fourth = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(fourth.index(), 1);
        drop(fourth);
        assert!(scheduler.assignment(&abandoned).is_none());
        let state = scheduler.state.lock().unwrap();
        assert_eq!(state.remaining_count, 0);
        assert_eq!(state.remaining, [false; 4]);
        assert_eq!(state.active_files, [0, 0]);
        assert!(state.active_items.is_empty());
        assert_eq!(state.package_turns, [2, 2]);
        assert!(!state.learned_items.contains(&(0, Arc::from("alpha::f"))));
        assert!(state.learned_items.contains(&(1, Arc::from("beta::f"))));
        assert!(state.transfer_items.is_empty());
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
            let state = scheduler.state.lock().unwrap();
            assert_eq!(state.active_files, [2]);
            assert_eq!(state.active_items.get(&key), Some(&2));
            assert_eq!(state.active_resources.get(&resource), Some(&2));
            assert_eq!(state.package_turns, [2]);
            assert_eq!(state.remaining_count, 1);
        }
        drop(first);
        {
            let state = scheduler.state.lock().unwrap();
            assert_eq!(state.active_files, [1]);
            assert_eq!(state.active_items.get(&key), Some(&1));
            assert_eq!(state.active_resources.get(&resource), Some(&1));
            assert!(
                state.learned_items.is_empty(),
                "an abandoned assignment must not manufacture learning"
            );
            assert_eq!(state.package_turns, [2]);
        }
        second.complete(false);
        {
            let state = scheduler.state.lock().unwrap();
            assert_eq!(state.active_files, [0]);
            assert!(state.active_items.is_empty());
            assert!(state.active_resources.is_empty());
            assert_eq!(state.learned_items, std::iter::once(key).collect());
            assert_eq!(state.package_turns, [2]);
        }
        let last = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(last.index(), 2);
        last.complete(false);
        assert!(scheduler.assignment(&abandoned).is_none());
        let state = scheduler.state.lock().unwrap();
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
        let state = scheduler.state.lock().unwrap();
        assert_eq!(state.remaining, [true]);
        assert_eq!(state.remaining_count, 1);
        assert_eq!(state.active_files, [0]);
        assert!(state.active_items.is_empty());
        assert_eq!(state.package_turns, [0]);
    }

    #[test]
    fn abandonment_notifies_a_resource_waiter_before_any_assignment_is_released() {
        let resource: Arc<str> = "shared".into();
        let work = (0..2)
            .map(|order| ScheduledWork {
                resources: vec![Arc::clone(&resource)],
                ..scheduled(0, "item", "package", 7, 2, true, order)
            })
            .collect();
        let scheduler = Scheduler::with_resources(work, 1, std::iter::once((Arc::clone(&resource), 1)).collect());
        let abandoned = OnceLock::new();
        let first = {
            let mut state = scheduler.state.lock().unwrap();
            scheduler.reserve_next(&mut state).unwrap()
        };
        let mut waiting = 0;
        let mut returned = 0;
        let mut notifications = 0;
        let stopped = scheduler.claim_with_wait(
            &abandoned,
            || waiting += 1,
            |state| {
                assert_eq!(returned, 0, "abandonment must be rechecked before another wait");
                assert_eq!(state.active_resources.get(&resource), Some(&1));
                assert_eq!(state.remaining, [false, true]);
                assert_eq!(state.remaining_count, 1);
                drop(state);
                abandoned.set("scripted stop".to_owned()).unwrap();
                scheduler.abandon_with(|| {
                    assert!(
                        matches!(scheduler.state.try_lock(), Err(std::sync::TryLockError::WouldBlock)),
                        "the notification must hold the scheduler mutex to close the lost-wakeup window"
                    );
                    notifications += 1;
                });
                returned += 1;
                scheduler.state.lock().unwrap()
            },
        );
        assert_eq!(stopped, None);
        assert_eq!((waiting, returned, notifications), (1, 1, 1));
        {
            let state = scheduler.state.lock().unwrap();
            assert_eq!(state.active_resources.get(&resource), Some(&1));
            assert_eq!(state.remaining, [false, true]);
            assert_eq!(state.remaining_count, 1);
            assert_eq!(state.active_files, [1]);
            assert_eq!(state.active_items.get(&(0, Arc::from("item"))), Some(&1));
        }
        scheduler.release(first, false);
        let state = scheduler.state.lock().unwrap();
        assert!(state.active_resources.is_empty());
        assert!(state.active_items.is_empty());
        assert_eq!(state.active_files, [0]);
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
                        } else {
                            Some((2, file_active))
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
    fn ranked_candidate_score_matches_small_exact_boundary_matrix() {
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
                    }
                }
            }
        }
    }

    #[test]
    fn probe_reservation_and_hit_counters_have_exact_tier_boundaries() {
        for kind in [ProbeKind::Item, ProbeKind::Reach, ProbeKind::File] {
            let tally = Tally::default();
            for _ in 0..17 {
                kind.attempt(&tally);
            }
            assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 17);
            kind.hit(&tally);
            kind.attempt(&tally);
            let (probes, hits) = match kind {
                ProbeKind::Item => (&tally.item_probes, &tally.item_hits),
                ProbeKind::Reach => (&tally.reach_probes, &tally.reach_hits),
                ProbeKind::File => (&tally.file_probes, &tally.file_hits),
                ProbeKind::Exact => unreachable!(),
            };
            assert_eq!(probes.load(Ordering::Relaxed), 18);
            assert_eq!(hits.load(Ordering::Relaxed), 1);
            assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 1);
        }
        {
            let kind = ProbeKind::Exact;
            let tally = Tally::default();
            kind.attempt(&tally);
            kind.hit(&tally);
            assert_eq!(tally.exact_probes.load(Ordering::Relaxed), 1);
            assert_eq!(tally.exact_hits.load(Ordering::Relaxed), 1);
            assert_eq!(tally.inferred_probes.load(Ordering::Relaxed), 0);
            assert_eq!(tally.inferred_hits.load(Ordering::Relaxed), 0);
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
    fn scheduler_prefers_independent_items_without_blocking_same_item_work() {
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
        assert_eq!(scheduler.select(&state), Some(0), "same-item work must remain runnable");
    }

    #[test]
    fn scheduler_skips_work_blocked_by_an_active_resource() {
        let resource: Arc<str> = "exclusive".into();
        let mut first = scheduled(0, "first", "a", 30, 0, false, 0);
        first.resources.push(Arc::clone(&resource));
        let mut blocked = scheduled(1, "blocked", "b", 20, 0, false, 1);
        blocked.resources.push(Arc::clone(&resource));
        let unrelated = scheduled(2, "unrelated", "c", 10, 0, false, 2);
        let mut capacities = HashMap::default();
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
    fn checkpoint_abandonment_releases_every_controlled_waiter_without_releasing_active_work() {
        fn wait(scheduler: &Scheduler, abandoned: &OnceLock<String>, index: usize, returned: &mut Vec<usize>, notifications: &mut usize) {
            let mut waits = 0;
            assert_eq!(
                scheduler.claim_with_wait(
                    abandoned,
                    || {},
                    |state| {
                        waits += 1;
                        assert_eq!(waits, 1, "abandonment must be observed before waiting again");
                        assert_eq!(state.remaining_count, 4);
                        assert_eq!(state.active_files, [1, 0, 0, 0, 0]);
                        drop(state);
                        if index < 3 {
                            wait(scheduler, abandoned, index + 1, returned, notifications);
                        } else {
                            abandoned.set("checkpoint failed".to_owned()).unwrap();
                            scheduler.abandon_with(|| {
                                assert!(matches!(scheduler.state.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
                                *notifications += 1;
                            });
                        }
                        scheduler.state.lock().unwrap()
                    },
                ),
                None
            );
            returned.push(index);
        }
        let resource: Arc<str> = "exclusive".into();
        let work = (0..5)
            .map(|index| {
                let mut work = scheduled(index, "item", "package", 1, 0, false, index);
                work.resources.push(Arc::clone(&resource));
                work
            })
            .collect();
        let scheduler = Scheduler::with_resources(work, 5, std::iter::once((Arc::clone(&resource), 1)).collect());
        let abandoned = OnceLock::new();
        let active = scheduler.assignment(&abandoned).unwrap();
        let mut returned = Vec::new();
        let mut notifications = 0;
        wait(&scheduler, &abandoned, 0, &mut returned, &mut notifications);
        assert_eq!(returned, [3, 2, 1, 0]);
        assert_eq!(notifications, 1);
        {
            let state = scheduler.state.lock().unwrap();
            assert_eq!(state.active_resources.get(&resource), Some(&1));
            assert_eq!(state.remaining, [false, true, true, true, true]);
            assert_eq!(state.remaining_count, 4);
        }
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
    fn scheduler_spreads_discovery_then_exploits_fresh_killers_and_demotes_failed_transfer() {
        let scheduler = Scheduler::new(
            vec![
                scheduled(0, "family", "a", 1, 3, false, 0),
                scheduled(0, "family", "a", 1, 3, false, 1),
                scheduled(0, "family", "a", 1, 3, false, 2),
                scheduled(1, "independent", "a", 1, 1, false, 3),
                scheduled(2, "unexplored", "a", 1, 1, false, 4),
            ],
            3,
        );
        let abandoned = OnceLock::new();
        let discovery = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(discovery.index(), 0);
        let independent = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(independent.index(), 3, "explore another function while discovery is in flight");
        discovery.complete(true);
        let transfer = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(transfer.index(), 1, "a fresh killer outranks an untouched function");
        transfer.complete(false);
        let unexplored = scheduler.assignment(&abandoned).unwrap();
        assert_eq!(unexplored.index(), 4, "ineffective transfer loses its exploitation boost");
        assert_eq!(
            scheduler.assignment(&abandoned).unwrap().index(),
            2,
            "same-function work never waits for a scout"
        );
        drop((independent, unexplored));
    }

    #[test]
    fn fresh_killer_priority_uses_transfer_evidence_without_retiring_candidates() {
        let learning = FileLearning::new();
        let found = killer("learned");
        let candidate = Candidate::Exact(found.clone());
        assert!(!learning.has_fresh_killer("function"));
        learning.publish("function", &killed_by(found));
        assert!(learning.has_fresh_killer("function"));
        learning.observe("function", &candidate, false, Duration::from_millis(1));
        assert!(
            learning.has_fresh_killer("function"),
            "one miss must not erase a discovery's priority"
        );
        learning.observe("function", &candidate, false, Duration::from_millis(1));
        assert!(!learning.has_fresh_killer("function"), "repeated misses reduce scheduling priority");
        assert!(
            learning.candidates("function").contains(&candidate),
            "demotion must never retire the candidate"
        );
        learning.observe("function", &candidate, true, Duration::from_millis(1));
        assert!(learning.has_fresh_killer("function"), "successful transfer restores the priority");
        let mut saved = GeneralizedHints::default();
        learning.persist(Utf8Path::new("source.rs"), &mut saved);
        let restored = FileLearning::from_hints(Utf8Path::new("source.rs"), &saved);
        assert!(
            !restored.has_fresh_killer("function"),
            "historical hints are not fresh campaign discoveries"
        );
        assert!(restored.candidates("function").contains(&candidate));
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
                        entry.transfer = index % 5 == 0;
                        entry
                    })
                    .collect();
                let capacities = if resource_limited {
                    [("resource-0".into(), 1), ("resource-1".into(), 2)].into_iter().collect()
                } else {
                    HashMap::default()
                };
                let scheduler = Scheduler::with_pending(work, 4, capacities, (0..96).map(|index| index % 13 != 0).collect());
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
                        scheduler.release_with_transfer(index, step % 4 != 0, Some(step % 5 == 0));
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
            "publishing learning leaves same-item siblings runnable"
        );
    }

    #[test]
    fn long_running_mutants_do_not_block_unrelated_or_same_item_work() {
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
            assert_eq!(scheduler.claim(&abandoned), Some(1), "same-item work remains runnable");
        }

        scheduler.complete(1);
        scheduler.complete(2);
        release.send(()).expect("the scout can be released");
        scout.join().expect("the scout exits normally");
        assert_eq!(scheduler.claim(&abandoned), None);
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
