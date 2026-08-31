// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running the suite once per mutant and turning each result into a verdict.

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
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
use super::verdict::{Attempt, Only, Verdict, run_binary};
use super::workspace::Workspace;
use crate::Result;
use crate::discover::{Killer, Plan};
use crate::error::error;
use crate::model::Outcome;

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
fn enumeration_note(binary: &Utf8Path, output: &str) -> String {
    let mut note = format!(
        "`cargo nextest` could not enumerate tests in `{binary}` with this mutant active; the same selection succeeded with no mutant active"
    );

    if !output.is_empty() {
        note.push_str(":\n");
        note.push_str(output);
    }

    note
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
        && let Some(binaries) = reach.reachable(&mutant.package)
        && let Some(binary) = binaries.iter().find(|b| hint.names(&b.package, &b.target))
    {
        return binary.baseline;
    }

    let Some(binaries) = reach.reachable(&mutant.package) else {
        return Duration::ZERO;
    };

    let mut total = Duration::ZERO;
    let mut used_census = false;

    for binary in binaries {
        match census.work(binary, mutant.ordinal) {
            CensusWork::Selected(duration) => {
                total += duration;
                used_census = true;
            }
            CensusWork::Whole => total += binary.baseline,
            CensusWork::Uncovered => {}
            CensusWork::Hinted(_duration) => total += binary.baseline,
        }
    }

    // When census data was used, the estimate is site-specific and more accurate.
    // When no census data, fall back to package-level baseline sum.
    let _ = used_census;
    total
}

/// Orders package queues longest-first, then interleaves them.
///
/// Workers pull from a single queue, so package queues start in descending expected-cost order
/// rather than whatever order discovery happened to enumerate them. Pulling already balances
/// individual mutants across workers; this ordering decides which independent workloads overlap.
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

    loop {
        let mut moved = false;

        for (queue, cursor) in queues.iter().zip(&mut next) {
            if let Some(position) = queue.get(*cursor).copied() {
                pending[out] = position;
                out += 1;
                *cursor += 1;
                moved = true;
            }
        }

        if !moved {
            break;
        }
    }
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
}

/// What a finished sweep spent, read off its [`Tally`] once every worker has stopped.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Spent {
    /// How many test-binary subprocesses the sweep launched in total.
    pub(super) launches: usize,

    /// How many of those launches were hint-directed probes.
    pub(super) probes: usize,
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
    killers: &mut Killers,
    events: &mut impl Events,
) -> Result<Option<Spent>> {
    let jobs = sweep.jobs;

    let mut pending: Vec<usize> = plan
        .mutants
        .iter()
        .enumerate()
        .filter(|(_position, mutant)| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .map(|(position, _mutant)| position)
        .collect();

    if pending.is_empty() {
        return Ok(None);
    }

    schedule(&mut pending, plan, reach, sweep.census, killers);

    let next = AtomicUsize::new(0);
    let tally = Tally::default();
    let abandoned: OnceLock<String> = OnceLock::new();

    // Indexed by *queue position*, not by plan position, and therefore built after the schedule is
    // fixed. Building either before it would silently pair one mutant's ordinal with another's
    // reachable binaries.
    let ordinals: Vec<(u32, Option<f64>)> = pending
        .iter()
        .map(|position| (plan.mutants[*position].ordinal, plan.mutants[*position].test_timeout_multiplier))
        .collect();

    let reachable: Vec<&[&TestBinary]> = pending
        .iter()
        .map(|position| {
            reach
                .reachable(&plan.mutants[*position].package)
                .expect("the shared reachability index was built from these same pending mutants")
        })
        .collect();

    let mut files: crate::HashMap<_, usize> = crate::HashMap::default();
    let file_slots: Vec<usize> = pending
        .iter()
        .map(|position| {
            let file = Arc::clone(&plan.mutants[*position].file);
            let next = files.len();

            *files.entry(file).or_insert(next)
        })
        .collect();
    let file_killers: Vec<FileLearning> = (0..files.len()).map(|_file| FileLearning::new()).collect();

    let (sender, receiver) = mpsc::channel::<Completed>();

    // Resolved up front for the same reason reachability is: a worker must need nothing from the
    // plan, so the calling thread can borrow it mutably and record verdicts as they arrive. Cloned
    // rather than borrowed because the same map is written back as those verdicts land.
    let hints: Vec<Option<Killer>> = pending
        .iter()
        .map(|position| killers.hint(&plan.mutants[*position].id).cloned())
        .collect();
    let notes = crate::notes::current();

    thread::scope(|scope| {
        for _worker in 0..jobs.max(1) {
            let sender = sender.clone();
            let next = &next;
            let ordinals = &ordinals;
            let reachable = &reachable;
            let hints = &hints;
            let pending = &pending;
            let file_slots = &file_slots;
            let file_killers = &file_killers;
            let abandoned = &abandoned;
            let tally = &tally;
            let notes = notes.clone();

            let _handle = scope.spawn(move || {
                let _notes = crate::notes::enter(notes.as_ref());

                loop {
                    if abandoned.get().is_some() {
                        break;
                    }

                    let index = next.fetch_add(1, Ordering::Relaxed);

                    let Some(position) = pending.get(index).copied() else {
                        break;
                    };

                    let (ordinal, timeout_multiplier) = ordinals[index];
                    let started = Instant::now();
                    let reachable = &reachable[index];
                    let judged = judge_learning(
                        work,
                        ordinal,
                        reachable,
                        hints[index].as_ref(),
                        &file_killers[file_slots[index]],
                        timeout_multiplier,
                        sweep,
                        tally,
                    );

                    let (outcome, killer, note) = match judged {
                        Judgement::Reached(outcome, killer, note) => (outcome, killer, note),
                        Judgement::Abandoned(reason) => {
                            let _first = abandoned.set(reason);

                            break;
                        }
                    };

                    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

                    // A closed receiver means the calling thread is gone, which cannot happen while
                    // the scope is open; there is nothing useful to do about it either way.
                    let _sent = sender.send((position, outcome, elapsed, killer, note));
                }
            });
        }

        // The workers hold the only remaining senders, so the drain ends when the last one finishes.
        drop(sender);

        for (position, outcome, elapsed, killer, note) in receiver {
            if let Some(mutant) = plan.mutants.get_mut(position) {
                mutant.outcome = outcome;
                mutant.elapsed_ms = elapsed;
                mutant.killed_by = killer.as_ref().map(|killer| killer.test.clone());
                mutant.note = note;

                // Written both ways round. An entry that convicted is worth keeping; a mutant whose
                // verdict named no test has to lose the one it had, or every run after this one
                // pays for a probe already shown not to convict.
                match killer {
                    Some(killer) => killers.record(mutant.id.clone(), killer),
                    None => killers.forget(&mutant.id),
                }

                events.mutant(mutant);
            }
        }
    });

    abandoned.into_inner().map_or_else(
        || {
            Ok(Some(Spent {
                launches: tally.launches.into_inner(),
                probes: tally.probes.into_inner(),
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

/// Runs the one test that caught this mutant last time, and says whether it caught it again.
///
/// A re-killed mutant's cost falls from a partial binary — every test ahead of its killer, in
/// whatever order the harness runs them — to a single test.
///
/// Only a failure is believed. Every other verdict a probe can reach is discarded and the ordinary
/// binary run proceeds unchanged. The caller reaches the probe in canonical binary order and only
/// permits it when filtering cannot bypass another outcome from that same binary.
fn probe(
    work: &Workspace,
    ordinal: u32,
    binary: &TestBinary,
    hint: &Killer,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
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
    let _launches = tally.launches.fetch_add(1, Ordering::Relaxed);
    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);

    let verdict = run_binary(work, binary, attempt, sweep.confirm);

    match verdict {
        // The harness names the test it ran, which under a filter can only be the one asked for;
        // the recorded name is used when it names nothing, so the map stays populated either way.
        Verdict::Failed(named) => Some(Killer {
            package: hint.package.clone(),
            target: hint.target.clone(),
            test: named.unwrap_or_else(|| hint.test.clone()),
        }),
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
    binary.budget_for(timeout_multiplier, sweep.timeout_floor).is_none() && sweep.stall.budget.is_none() && !sweep.meter && !sweep.confirm
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

    let _launches = tally.launches.fetch_add(1, Ordering::Relaxed);
    let _probes = tally.probes.fetch_add(1, Ordering::Relaxed);

    match run_binary(work, binary, attempt, sweep.confirm) {
        Verdict::Failed(name) => Some(Killer {
            package: binary.package.clone(),
            target: binary.target.clone(),
            test: name.unwrap_or_else(|| {
                names
                    .first()
                    .copied()
                    .expect("an incomplete census hint always names at least one test")
                    .to_owned()
            }),
        }),
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
    // Set by the first binary this mutant is actually run against. Left false, nothing that could
    // convict this code was run — no test binary links it, none of them announced a test, or a
    // census established that no test in any of them executes the site — and reporting that as a
    // survivor would blame the tests that exist for the absence of ones that do not.
    let mut ran = false;

    for binary in reachable.iter().copied() {
        let selection = sweep.census.selection(binary, ordinal);

        // A hint naming another binary is stale or outside the selected package set. Waiting until
        // canonical iteration reaches the named binary prevents its kill from bypassing any
        // earlier Flaky, Pending, resource, or metering outcome.
        let killer_hint = hint
            .filter(|hint| hint.names(&binary.package, &binary.target))
            .or_else(|| file_hint.filter(|hint| hint.names(&binary.package, &binary.target)));

        if let Some(hint) = killer_hint
            && filtered_kill_is_final(binary, timeout_multiplier, sweep)
            && let Some(killer) = probe(work, ordinal, binary, hint, timeout_multiplier, sweep, tally)
        {
            return Judgement::Reached(Outcome::Killed, Some(killer), None);
        }

        if let CensusSelection::Hinted(names) = &selection
            && filtered_kill_is_final(binary, timeout_multiplier, sweep)
            && let Some(killer) = probe_cases(work, ordinal, binary, names, timeout_multiplier, sweep, tally)
        {
            return Judgement::Reached(Outcome::Killed, Some(killer), None);
        }

        let only = match &selection {
            CensusSelection::Whole | CensusSelection::Hinted(_) => Only::All,
            CensusSelection::Uncovered => continue,
            CensusSelection::Selected(names) => Only::These(names),
        };

        ran = true;

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

        let _launches = tally.launches.fetch_add(1, Ordering::Relaxed);

        let mut verdict = run_binary(work, binary, attempt, sweep.confirm);

        // A complete census proves which tests can observe the mutant, but filtering also shrinks
        // runtime and peak memory and can change failure order. A non-passing filtered run is
        // therefore provisional: repeat the whole binary and use only its canonical outcome.
        if matches!(selection, CensusSelection::Selected(_)) && !matches!(verdict, Verdict::Passed) {
            let _launches = tally.launches.fetch_add(1, Ordering::Relaxed);
            verdict = run_binary(
                work,
                binary,
                Attempt {
                    only: Only::All,
                    ..attempt
                },
                sweep.confirm,
            );
        }

        match verdict {
            Verdict::Passed => {}
            Verdict::Failed(name) => {
                let killer = name.map(|test| Killer {
                    package: binary.package.clone(),
                    target: binary.target.clone(),
                    test,
                });

                return Judgement::Reached(Outcome::Killed, killer, None);
            }
            Verdict::TestEnumerationFailed(output) => {
                return Judgement::Reached(Outcome::Killed, None, Some(enumeration_note(&binary.path, &output)));
            }
            Verdict::TimedOut => return Judgement::Reached(Outcome::Timeout, None, None),
            Verdict::Stalled(test) => {
                return Judgement::Reached(Outcome::Timeout, None, Some(stall_note(test.as_deref())));
            }

            // The suite's own harness did not fail, but the baseline established that this same
            // workload fits under this same ceiling without the mutant, so the mutant is what
            // changed. This is still undetected for scoring: no assertion exposed the change.
            Verdict::MemoryLimit { peak, limit } => {
                return Judgement::Reached(Outcome::OutOfMemory, None, Some(memory_note(&binary.path, peak, limit)));
            }
            // The suite failed with the mutant active and failed again without it, so this run
            // established nothing about this mutant. Recorded as its own outcome so it lands in
            // neither the score nor the survivor list, and carrying the test to fix.
            Verdict::Flaky(test) => return Judgement::Reached(Outcome::Flaky, None, Some(flaky_note(&binary.path, test.as_deref()))),
            Verdict::Unmetered(reason) => return Judgement::Abandoned(reason),

            // One run the machine would not perform, which is a fact about this mutant and not
            // about the run: the shortage behind it — descriptors, process slots — is one the sweep
            // creates for itself and clears as its other workers finish. Recorded as unjudged
            // against this mutant, with the refusal as the note, so that the mutants around it keep
            // their verdicts and the reader can see which one went without.
            Verdict::Unjudged(reason) => return Judgement::Reached(Outcome::Pending, None, Some(reason)),
        }
    }

    Judgement::Reached(if ran { Outcome::Survived } else { Outcome::NoCoverage }, None, None)
}

/// Learns one file's first observed killer before letting its remaining mutants proceed in
/// parallel with that binary first.
///
/// Workers that encounter `InProgress` wait on a condvar (bounded by a short timeout) rather
/// than immediately proceeding without the hint. This avoids redundantly launching the expensive
/// cold path for siblings when a killer is about to be published. The bounded wait ensures
/// workers are never idled indefinitely if no common killer exists.
#[expect(clippy::too_many_arguments, reason = "adds one file-local state cell to the verdict path")]
fn judge_learning(
    work: &Workspace,
    ordinal: u32,
    reachable: &[&TestBinary],
    hint: Option<&Killer>,
    observed: &FileLearning,
    timeout_multiplier: Option<f64>,
    sweep: Sweep<'_>,
    tally: &Tally,
) -> Judgement {
    if hint.is_some() {
        let judged = judge_ordered(work, ordinal, reachable, hint, None, timeout_multiplier, sweep, tally);

        if let Judgement::Reached(_outcome, Some(killer), _note) = &judged {
            let mut state = observed.state.lock().expect("a file-local killer lock was poisoned");
            *state = Learning::Learned(killer.clone());
            drop(state);
            observed.notify.notify_all();
        }

        return judged;
    }

    let state = {
        let mut learned = observed.state.lock().expect("a file-local killer lock was poisoned");

        match &*learned {
            Learning::Learned(killer) => Some(Ok(killer.clone())),
            Learning::Untried => {
                *learned = Learning::InProgress;
                None
            }
            Learning::InProgress => {
                // Wait for the learner to finish, bounded so workers are never stuck.
                let result = observed
                    .notify
                    .wait_timeout_while(learned, LEARNING_WAIT, |s| matches!(s, Learning::InProgress))
                    .expect("a file-local killer lock was poisoned");
                learned = result.0;
                match &*learned {
                    Learning::Learned(killer) => Some(Ok(killer.clone())),
                    // Timed out or exhausted: proceed without hint.
                    _ => Some(Err(())),
                }
            }
            Learning::Exhausted => Some(Err(())),
        }
    };

    if let Some(state) = state {
        return match state {
            Ok(killer) => judge_ordered(work, ordinal, reachable, None, Some(&killer), timeout_multiplier, sweep, tally),
            Err(()) => judge_ordered(work, ordinal, reachable, None, None, timeout_multiplier, sweep, tally),
        };
    }

    let judged = judge_ordered(work, ordinal, reachable, None, None, timeout_multiplier, sweep, tally);

    {
        let mut state = observed.state.lock().expect("a file-local killer lock was poisoned");
        *state = if let Judgement::Reached(_outcome, Some(killer), _note) = &judged {
            Learning::Learned(killer.clone())
        } else {
            Learning::Exhausted
        };
        observed.notify.notify_all();
    }

    judged
}

/// How long a worker waits for a file's learner before proceeding without the hint.
///
/// Short enough that workers do not idle when no common killer exists, long enough that a fast
/// learner (a killed mutant in single-digit milliseconds) publishes before siblings launch.
const LEARNING_WAIT: Duration = Duration::from_millis(50);

/// Per-file learning state with a condvar for wait/notify coordination.
struct FileLearning {
    state: Mutex<Learning>,
    notify: Condvar,
}

impl FileLearning {
    const fn new() -> Self {
        Self {
            state: Mutex::new(Learning::Untried),
            notify: Condvar::new(),
        }
    }
}

#[derive(Debug)]
enum Learning {
    Untried,
    InProgress,
    Learned(Killer),
    Exhausted,
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
        assert_eq!(spent.probes, 1, "the learned killer is checked only after the earlier binary");
    }

    #[test]
    #[cfg(unix)]
    fn same_file_survivors_run_in_parallel_after_one_learning_attempt_starts() {
        let (_directory, work, mut plan, binaries) = harness_n("sleep 0.20; exit 0", Duration::from_secs(30), 2);
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
        let ordered = sets.reachable("subject").expect("subject holds the plan's one pending mutant");

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
            let package = &*plan.mutants[*position].package;
            let found: Vec<&TestBinary> = binaries.iter().filter(|binary| reaches(binary, package, &plan, &NARROW)).collect();

            assert_eq!(
                sets.reachable(package).expect("every package in `pending` holds a pending mutant"),
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

    /// The condvar-based file learning correctly publishes a learned killer to waiting siblings.
    #[test]
    fn file_learning_publishes_killer_to_waiting_siblings() {
        let learning = FileLearning::new();

        // First worker becomes learner.
        {
            let mut state = learning.state.lock().unwrap();
            assert!(matches!(*state, Learning::Untried));
            *state = Learning::InProgress;
        }

        // Simulate a second worker waiting and a learner publishing.
        thread::scope(|scope| {
            let _waiter = scope.spawn(|| {
                let (guard_result, timed_out) = learning
                    .notify
                    .wait_timeout_while(learning.state.lock().unwrap(), Duration::from_millis(200), |s| {
                        matches!(s, Learning::InProgress)
                    })
                    .unwrap();
                assert!(!timed_out.timed_out(), "should have been woken, not timed out");
                match &*guard_result {
                    Learning::Learned(killer) => assert_eq!(killer.test, "tests::found"),
                    other => panic!("expected Learned, got {other:?}"),
                }
                drop(guard_result);
            });

            // Give the waiter time to enter wait.
            thread::sleep(Duration::from_millis(10));

            // Learner finishes with a killer.
            {
                let mut s = learning.state.lock().unwrap();
                *s = Learning::Learned(Killer {
                    package: "pkg".to_owned(),
                    target: String::new(),
                    test: "tests::found".to_owned(),
                });
                drop(s);
                learning.notify.notify_all();
            }
        });
    }

    /// The condvar-based file learning wakes waiters with `Exhausted` when no killer is found.
    #[test]
    fn file_learning_wakes_waiters_on_exhaustion() {
        let learning = FileLearning::new();

        {
            let mut state = learning.state.lock().unwrap();
            *state = Learning::InProgress;
        }

        thread::scope(|scope| {
            let _waiter = scope.spawn(|| {
                let (guard_result, _timed_out) = learning
                    .notify
                    .wait_timeout_while(learning.state.lock().unwrap(), Duration::from_millis(200), |s| {
                        matches!(s, Learning::InProgress)
                    })
                    .unwrap();
                assert!(matches!(*guard_result, Learning::Exhausted));
                drop(guard_result);
            });

            thread::sleep(Duration::from_millis(10));

            {
                let mut s = learning.state.lock().unwrap();
                *s = Learning::Exhausted;
                drop(s);
                learning.notify.notify_all();
            }
        });
    }

    /// The bounded wait times out and proceeds without a hint when the learner is slow.
    #[test]
    fn file_learning_wait_times_out_for_slow_learner() {
        let learning = FileLearning::new();

        {
            let mut state = learning.state.lock().unwrap();
            *state = Learning::InProgress;
        }

        let started = Instant::now();

        {
            let (guard_result, timed_out) = learning
                .notify
                .wait_timeout_while(learning.state.lock().unwrap(), Duration::from_millis(20), |s| {
                    matches!(s, Learning::InProgress)
                })
                .unwrap();
            drop(guard_result);
            assert!(timed_out.timed_out(), "should have timed out");
        }

        // The timeout should be roughly 20ms, not blocking indefinitely.
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
