// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::time::Instant;

use camino::Utf8PathBuf;

use super::cargo_options::BuildLimits;
use super::events::Events;
use super::test_binary::{TestBinary, test_binaries};
use super::verdict::tail;
use super::workspace::Workspace;
use crate::discover::Plan;
use crate::error::{Error, error};
use crate::model::{Mutant, Outcome};
use crate::schema::Guard;
use crate::{HashMap, HashSet, Result};

mod blame;
mod complaints;
mod invoke;
mod messages;
mod splices;

#[cfg(all(test, not(miri)))]
mod tests;

use blame::blame;
use complaints::{DIAGNOSTIC_LIMIT, complaints, diagnostics, leading, manifests_of, prioritize};
use invoke::run_cargo;
use messages::compiled_sources;
use splices::Splices;

/// Where each live mutant's guard landed, by ordinal, paired with the file it landed in.
type Guards = HashMap<u32, (Utf8PathBuf, Guard)>;

/// A test-only stand-in for one proof build: a verdict on which spliced ordinals fail to compile.
///
/// Mirrors [`Converger::subset_fails`]'s own return: `Some(true)` failed, `Some(false)` compiled,
/// `None` could not be told (a timeout).
#[cfg(test)]
type SubsetOracle = fn(&HashSet<u32>) -> Option<bool>;

/// What the stale build-ordering hints actually did, counted rather than modelled.
///
/// Every figure here is something that happened. There is deliberately no "rounds saved", because
/// that number does not exist: it is the length of a convergence that was never run, over a mutant
/// population that was never offered to the compiler in that shape, and any figure printed for it
/// would be a model of a counterfactual dressed up as a measurement. What *can* be measured is how
/// many mutants the hints put in front of the compiler early and how many of those the compiler
/// then refused, and those two together say whether the hints are worth their round: `offered`
/// close to `confirmed` is a hint set that is paying, and `confirmed` near zero is one that is
/// costing a build per stage and buying nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrderingHints {
    /// How many hinted mutants were put in front of the compiler in a probe round.
    pub offered: usize,

    /// How many of those the compiler then blamed, which is the hint turning out to be right.
    ///
    /// A hinted mutant that compiles is not an error and is not withheld: it stays live and is
    /// judged by the run exactly as if it had never been hinted. This counts only the ones the
    /// compiler independently refused.
    pub confirmed: usize,

    /// How many probe rounds the run spent, which is the cost side of the trade.
    pub rounds: u32,
}

/// How many hinted mutants make a probe round worth the build it costs.
///
/// A probe round is one extra cargo invocation. It repays that by putting the mutants likeliest to
/// fail in front of the compiler with nothing else to mask them, so they are blamed together
/// instead of a few per wave. Below a handful there is nothing to unmask — the ordinary rounds
/// would have found them just as fast — and the build would be spent for nothing, so the round is
/// simply not taken. The number is a judgement rather than a measurement, which is exactly why the
/// run reports what the probes offered and confirmed instead of claiming a saving.
const PROBE_FLOOR: usize = 4;

/// What one round of a build cost, and what it bought.
///
/// A run reports the series rather than a total because the two ends mean opposite things. The
/// first round is what compiling this workspace costs at all, which no amount of mutant selection
/// will avoid; every round after it exists only because some mutant did not compile, and its time
/// is the price of that mutant. A total conflates the two and so points at the wrong remedy.
#[derive(Debug, Clone)]
pub struct Round {
    /// How long the round's cargo invocation took.
    pub elapsed: Duration,

    /// How many mutants the round withdrew, which is zero for the round that finally compiled.
    pub withdrew: usize,
}

/// How many mutants one rustc error code withdrew from one mutator.
///
/// The pairing is what makes the figure actionable. A code on its own says what kind of code the
/// instrumented tree contained; a mutator on its own says which mutator is expensive; the two
/// together say *which mutator emits which mistake*, which is the form a heuristic can be written
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    /// The rustc error code, or empty for a diagnostic that carried none.
    pub code: String,

    /// The mutator whose mutants the code was reported against.
    pub mutator: String,

    /// How many mutants — not how many diagnostics — the pair accounts for.
    pub mutants: usize,
}

/// What a build round produced.
#[derive(Debug, Default)]
pub(super) struct Build {
    /// What each round of this run's builds cost, oldest first.
    pub(super) history: Vec<Round>,

    pub(super) binaries: Vec<TestBinary>,
    pub(super) withdrawn: usize,

    /// How many rollback rounds the whole run spent, summed over its builds.
    pub(super) rounds: u32,

    /// Whether a narrowed build was abandoned and the whole workspace built instead.
    pub(super) widened: bool,

    /// Why the withdrawn mutants were withdrawn, densest pair first.
    pub(super) census: Vec<Withdrawal>,

    /// The population given up on when this build could not be made to compile at all.
    ///
    /// `None` for a build that converged, which is the ordinary case. When it is set the binaries
    /// are empty — there is nothing to run a mutant against — and the run reports what it knows
    /// rather than exiting with nothing at all.
    pub(super) stuck: Option<Abandoned>,

    /// What the stale build-ordering hints put in front of the compiler, and what came of it.
    pub(super) ordering: OrderingHints,
}

/// What one build arrived at.
///
/// A build that cannot be made to compile is not the same kind of event as a build that could not
/// be started. The first is a fact about this tree and these mutants, which a run can record,
/// report and carry on from; the second is a fact about the machine, which nothing downstream can
/// say anything useful about. Only the first is modelled here — everything else stays an `Err`.
#[derive(Debug)]
enum Convergence {
    /// The build succeeded, carrying cargo's JSON stream from the round that succeeded.
    Built(String),

    /// The build failed and no further mutant can be withdrawn to change that.
    Stuck(Error),
}

/// What a proof build isolated after rustc's spans could not identify a cause.
enum Isolation {
    /// One or more mutants failed even without any other mutant from their item.
    Blamed(Vec<u32>),

    /// Mutants in one item only fail in combination, so none can honestly be blamed alone.
    Item(Vec<u32>),
}

/// The mutants a build gave up on, and why.
///
/// The reason is the text of the error that would otherwise abort the whole run. It is preserved
/// word for word — the rollback-limit advice about a falling or flat withdrawal series, the excerpt
/// of what cargo said — because that text is the only thing that tells a reader whether the answer
/// is to raise a limit, to fix a build script, or to look somewhere else entirely.
#[derive(Debug)]
pub(super) struct Abandoned {
    /// The diagnostic explaining why the build could not be converged.
    pub(super) reason: String,

    /// The ordinals of the mutants that will never be run because of it, ascending.
    pub(super) ordinals: Vec<u32>,
}

/// Drives the build, withdrawing mutants that cannot compile until what is asked for compiles.
///
/// A run converges the workspace one stage at a time and then once as a whole, and every one of
/// those builds shares the same withdrawal set and budget reference. Sharing the withdrawal set is
/// what lets a stage inherit what earlier stages already ruled out: a mutant already known to be
/// unbuildable stays withdrawn for the rest of the run.
///
/// The round counter is not shared. `--rollback-rounds` caps the rounds one build may spend
/// converging, so it is reset for each build; a cumulative counter would let early stages spend the
/// budget and leave the build that decides the run with no chance to converge at all.
#[derive(Debug, Default)]
pub(super) struct Converger {
    withdrawn: HashSet<u32>,

    /// The subset of `withdrawn` that was given up on rather than blamed.
    ///
    /// A withdrawn mutant is one the compiler pointed at: it is unviable, and saying so is a
    /// verdict. A mutant in here was never accused of anything — the build it belonged to could
    /// not be made to compile, so its whole population was taken out of the tree to let the run
    /// carry on. Conflating the two would report a mutant the tool never judged as one the tool
    /// judged unbuildable, which is the exact confusion this run is trying to avoid.
    abandoned: HashSet<u32>,

    /// How many rounds the build currently converging has spent, reset at the start of each one.
    rounds: u32,

    /// How many rounds the whole run has spent, which is what the run reports.
    total_rounds: u32,

    /// How many mutants each failed round of the current build blamed, oldest first.
    ///
    /// Kept so that a build which hits the limit can say whether it was converging, which is the
    /// only thing that decides whether raising the limit would have helped. Reset with `rounds`,
    /// so that the advice describes the build that just failed rather than the ones before it.
    ///
    /// Counts what a round blamed rather than what it withdrew, because the two differ in exactly
    /// the round the reader is being told about: the round that hits the limit blames mutants and
    /// is then stopped before it can withdraw them. Recording the withdrawal would leave that round
    /// out of its own diagnostic, and the trend the advice reads is a trend in what the rounds are
    /// finding.
    per_round: Vec<usize>,

    /// What every round of every build in this run cost, oldest first.
    ///
    /// Unlike `per_round`, this is never reset: it describes the whole run, because what a reader
    /// wants to know is where the run's build time went, not where one stage's did.
    history: Vec<Round>,

    /// How long the first build of the run took, which every later budget is scaled from.
    ///
    /// Set once and never reset. A stage builds a fraction of the workspace, so letting a small
    /// stage set this reference would leave every later stage — and the whole-workspace build that
    /// follows them — with a budget derived from a build that was never comparable.
    first_round: Option<Duration>,

    /// What the tree already holds, so a round rewrites only the files it changed.
    splices: Splices,

    /// Source files named by successful staged and final builds.
    ///
    /// The final test-target build does not necessarily compile a package's default target: a
    /// library with `test = false` and no integration tests is one example. Staged default-target
    /// builds still prove those sources compiled, so their dep-info must remain part of the final
    /// inventory rather than letting the test-target artifact stream erase them.
    compiled: Option<HashSet<Utf8PathBuf>>,

    /// The rustc error code that first named each withdrawn mutant.
    ///
    /// The count of withdrawals says whether the number is large; only the codes say whether it is
    /// worth acting on. A run dominated by `E0308` is one where a mutator produces ill-typed code
    /// and could be taught not to, while one dominated by the borrow checker is a cost of the
    /// schema itself. Distinguishing them by hand meant patching this file every time, which is why
    /// it is kept rather than derived on demand.
    census: HashMap<u32, String>,

    /// Mutants that failed to compile for some earlier run whose build context no longer matches.
    ///
    /// Held by content id rather than by ordinal because ordinals are handed out stage by stage as
    /// the run scans, so most of them do not exist yet when this is set.
    ///
    /// This is evidence about *order* and nothing else. Not one mutant in here is withheld,
    /// excluded, settled or scored on the strength of it: every one is spliced into the tree and
    /// offered to the compiler, and the only thing the hint decides is that it is offered early,
    /// on its own, where a mutant that really is unviable is blamed with nothing else masking it.
    /// A hint that turns out to be wrong costs exactly the round it was probed in, and the mutant
    /// goes on to be built and judged as if it had never been named. That is what makes the tier
    /// safe under a context that no longer matches, which filtering would not be.
    hinted: HashSet<crate::model::MutantId>,

    /// Ordinals already put through a probe round, so no build pays for the same probe twice.
    probed: HashSet<u32>,

    /// What the probe rounds offered and what the compiler made of it.
    ordering: OrderingHints,

    /// Whether every build this run makes must compile the whole workspace.
    ///
    /// Set when the preflight check could only be made to pass by widening: the tree the run works
    /// in compiles under cargo's feature unification over every member and does not compile under
    /// any subset of them. A narrowed build after that is a build already known to fail, and its
    /// failure would be attributed to whichever mutants the rollback loop happened to blame — so
    /// the same feature unification the preflight proved is what every later build asks for.
    whole_workspace: bool,

    /// A test-only stand-in for the proof build in [`Self::subset_fails`].
    ///
    /// Reaching [`Isolation::Item`] needs a subset that compiles alone but fails only in
    /// combination, which no cheap real mutation fixture produces. When set, each proof build asks
    /// this function — a pure verdict on which ordinals are spliced — instead of invoking cargo, so
    /// a test can drive isolation to any branch deterministically without a real interaction bug.
    #[cfg(test)]
    subset_oracle: Option<SubsetOracle>,
}

/// What a preflight check settled: the scope it needed, and what it cost to pass at all.
#[derive(Debug)]
pub(super) struct Preflight {
    /// Whether only a whole-workspace build was shown to compile.
    ///
    /// Carried rather than discarded because a narrowed build after a wide-only success is a build
    /// already known to fail — and its failure would be blamed on mutants, settling valid ones as
    /// unbuildable and quietly shrinking the population the score is taken over.
    pub(super) whole_workspace: bool,

    /// The packages the check had to give up on, empty in the ordinary case.
    pub(super) dropped: Vec<String>,
}

impl Preflight {
    /// The result of a check that passed in the scope it was asked about.
    const fn narrow(dropped: Vec<String>) -> Self {
        Self {
            whole_workspace: false,
            dropped,
        }
    }
}

impl Converger {
    /// A converger that front-loads the mutants an out-of-context record says would not compile.
    ///
    /// `hinted` holds mutant content ids. Passing ids that name nothing in this run is harmless:
    /// they resolve to no ordinal and no probe round is taken for them.
    pub(super) fn guided(hinted: HashSet<crate::model::MutantId>) -> Self {
        Self { hinted, ..Self::default() }
    }

    /// Records that only a whole-workspace build has been shown to compile.
    ///
    /// Called with the preflight's own answer, so that the scope which proved the tree sound is the
    /// scope every later build uses. See [`Self::whole_workspace`].
    pub(super) const fn require_whole_workspace(&mut self) {
        self.whole_workspace = true;
    }

    /// Invalidates position-based splice indexes after the plan is sorted.
    pub(super) fn plan_reordered(&mut self) {
        self.splices.plan_reordered();
    }

    /// The package selection a build may actually use.
    ///
    /// Every narrowing goes through here, so there is one place that can answer "may this build be
    /// narrowed at all" and no build can be narrowed by forgetting to ask.
    const fn scoped<'names>(&self, select: Option<&'names [String]>) -> Option<&'names [String]> {
        if self.whole_workspace { None } else { select }
    }

    /// Instruments the tree and builds it until it compiles, withdrawing whatever stands in the way.
    ///
    /// `select` names the packages to build, or is `None` for the whole workspace. `verb` is the
    /// cargo command and its flags.
    ///
    /// Returns cargo's JSON stream from the build that finally succeeded, or the diagnostic for a
    /// build that could not be made to compile at all. That second case is returned rather than
    /// raised because it is a result: the run can withdraw the population it belongs to, keep every
    /// verdict it has already reached, and still produce a report.
    fn converge(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Convergence> {
        // The round budget is per build: what earlier stages spent converging is not this build's
        // to answer for, and the withdrawal series the limit error reads has to describe the build
        // that failed. The withdrawal set is deliberately left alone — a mutant already known not
        // to compile stays withdrawn for the rest of the run.
        self.rounds = 0;
        self.per_round.clear();

        // Before the first ordinary round, and only ever before it. Whatever the probe withdraws is
        // withdrawn by the compiler's own accusation in a real build, so the loop below starts from
        // a tree the compiler has already ruled on rather than from a guess.
        self.probe(work, plan, select, verb, limits, events)?;

        loop {
            self.rounds = self.rounds.saturating_add(1);
            self.total_rounds = self.total_rounds.saturating_add(1);

            let guards = self.splices.instrument(work, plan, &self.withdrawn)?;

            let started = Instant::now();
            let outcome = run_cargo(work, plan, verb, select, limits, self.first_round, events)?;
            let elapsed = started.elapsed();

            let Some(stdout) = outcome.stdout else {
                let budget = limits.budget(self.first_round).unwrap_or(elapsed);

                return Err(Self::build_timeout_error(budget));
            };

            if self.first_round.is_none() {
                self.first_round = Some(elapsed);
            }

            if outcome.succeeded {
                self.history.push(Round { elapsed, withdrew: 0 });

                return Ok(Convergence::Built(stdout));
            }

            let blamed = blame(&stdout, &work.root, &guards);

            if blamed.is_empty() {
                if let Some(isolated) = self.isolate(work, plan, select, verb, limits, events)? {
                    let ordinals = match &isolated {
                        Isolation::Blamed(ordinals) | Isolation::Item(ordinals) => ordinals,
                    };

                    self.history.push(Round {
                        elapsed,
                        withdrew: ordinals.len(),
                    });

                    for ordinal in ordinals {
                        let _ = self.withdrawn.insert(*ordinal);
                        let _ = self.census.entry(*ordinal).or_default();
                    }

                    if let Isolation::Item(ordinals) = isolated {
                        self.abandoned.extend(ordinals);
                    }

                    continue;
                }

                self.history.push(Round { elapsed, withdrew: 0 });

                return Ok(Convergence::Stuck(Self::unattributed_build_error(work, &stdout, &outcome.stderr)));
            }

            // This round joins the series before the limit is checked, because the round the limit
            // stops is the one the diagnostic is about: it blamed these mutants and was refused the
            // chance to withdraw them. Reading the series without it leaves a one-round budget with
            // nothing to report and the advice saying the last round found nothing, which is the
            // opposite of what happened.
            self.per_round.push(blamed.len());

            if self.rounds >= limits.rounds() {
                let error = Self::rollback_limit_error(self.rounds, limits.rounds(), &self.per_round, work, &stdout);

                // Nothing was withdrawn: `history` is what the run reports its build time against,
                // and this round ended without applying its blame.
                self.history.push(Round { elapsed, withdrew: 0 });

                return Ok(Convergence::Stuck(error));
            }

            self.history.push(Round {
                elapsed,
                withdrew: blamed.len(),
            });

            for (ordinal, code) in blamed {
                let _ = self.withdrawn.insert(ordinal);
                let _ = self.census.entry(ordinal).or_insert(code);
            }
        }
    }

    /// Uses proof builds to isolate a failure whose diagnostic spans name no guard.
    ///
    /// The pristine stage is tried first so a linker, build script or native dependency failure is
    /// never blamed on whichever mutant happens to be bisected last. A real schema failure is then
    /// narrowed by enclosing item and finally by ordinal. The extra builds are rare, warm, and
    /// logarithmic for the ordinary single-mutant case; their cost is preferable to discarding a
    /// package's entire population.
    fn isolate(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Option<Isolation>> {
        let mut candidates: Vec<&Mutant> = plan
            .mutants
            .iter()
            .filter(|mutant| {
                mutant.ordinal > 0
                    && !self.withdrawn.contains(&mutant.ordinal)
                    && select.is_none_or(|packages| packages.iter().any(|package| package.as_str() == &*mutant.package))
            })
            .collect();

        if candidates.is_empty() || self.subset_fails(work, plan, select, verb, limits, events, &candidates, &[])? != Some(false) {
            return Ok(None);
        }

        candidates.sort_by(|left, right| left.item_path.cmp(&right.item_path).then_with(|| left.ordinal.cmp(&right.ordinal)));
        let population = candidates.clone();

        let mut items: Vec<Vec<&Mutant>> = Vec::new();

        for mutant in candidates {
            if items
                .last()
                .and_then(|item| item.first())
                .is_some_and(|first| first.item_path == mutant.item_path)
            {
                items
                    .last_mut()
                    .unwrap_or_else(|| unreachable!("the item was just observed"))
                    .push(mutant);
            } else {
                items.push(vec![mutant]);
            }
        }

        while items.len() > 1 {
            let middle = items.len() / 2;
            let left = items[..middle].concat();
            let right = items[middle..].concat();

            if self.subset_fails(work, plan, select, verb, limits, events, &population, &left)? == Some(true) {
                items.truncate(middle);
                continue;
            }

            if self.subset_fails(work, plan, select, verb, limits, events, &population, &right)? == Some(true) {
                drop(items.drain(..middle));
                continue;
            }

            // Neither half fails alone, so the failure is an interaction. Remove one item at a
            // time from the failing set and keep the first removal proven to restore the build.
            for item in &items {
                let active: Vec<&Mutant> = population
                    .iter()
                    .copied()
                    .filter(|candidate| !item.iter().any(|removed| removed.ordinal == candidate.ordinal))
                    .collect();

                if self.subset_fails(work, plan, select, verb, limits, events, &population, &active)? == Some(false) {
                    return Ok(Some(Isolation::Item(item.iter().map(|mutant| mutant.ordinal).collect())));
                }
            }

            return Ok(None);
        }

        let item = items.pop().unwrap_or_default();
        let mut narrowed = item.clone();

        while narrowed.len() > 1 {
            let middle = narrowed.len() / 2;
            let left = &narrowed[..middle];
            let right = &narrowed[middle..];

            if self.subset_fails(work, plan, select, verb, limits, events, &population, left)? == Some(true) {
                narrowed.truncate(middle);
            } else if self.subset_fails(work, plan, select, verb, limits, events, &population, right)? == Some(true) {
                drop(narrowed.drain(..middle));
            } else {
                return Ok(Some(Isolation::Item(item.iter().map(|mutant| mutant.ordinal).collect())));
            }
        }

        Ok(Some(Isolation::Blamed(narrowed.iter().map(|mutant| mutant.ordinal).collect())))
    }

    /// Builds one chosen subset of a candidate population.
    #[expect(
        clippy::too_many_arguments,
        reason = "a proof build needs the same complete context as convergence"
    )]
    fn subset_fails(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
        population: &[&Mutant],
        active: &[&Mutant],
    ) -> Result<Option<bool>> {
        let active: HashSet<u32> = active.iter().map(|mutant| mutant.ordinal).collect();

        #[cfg(test)]
        if let Some(oracle) = self.subset_oracle {
            return Ok(oracle(&active));
        }

        let mut withdrawn = self.withdrawn.clone();

        for mutant in population {
            if !active.contains(&mutant.ordinal) {
                let _ = withdrawn.insert(mutant.ordinal);
            }
        }

        let _guards = self.splices.instrument(work, plan, &withdrawn)?;
        let started = Instant::now();
        let outcome = run_cargo(work, plan, verb, select, limits, self.first_round, events)?;
        let elapsed = started.elapsed();

        self.total_rounds = self.total_rounds.saturating_add(1);
        self.history.push(Round { elapsed, withdrew: 0 });

        Ok(outcome.stdout.map(|_stdout| !outcome.succeeded))
    }

    /// Builds only the mutants an out-of-context record expects to fail, before anything else.
    ///
    /// This is the whole of what a stale unviability tier is allowed to do. It does not withhold a
    /// mutant, settle one, exclude one, or touch the population in any way — every live mutant in
    /// the selection is still built and still judged. What it changes is the *order* the compiler
    /// meets them in: the hinted ones go first, alone, so that a genuinely unviable mutant is
    /// blamed with no other mutant's error masking it, and the ordinary convergence below starts
    /// with them already out of the tree instead of discovering them a wave at a time.
    ///
    /// Everything it withdraws is withdrawn on the compiler's evidence, in a real build, exactly as
    /// an ordinary round withdraws. A hint that was wrong simply produces a mutant that compiles:
    /// it stays live, it is spliced back in by the next round, and it is judged as if it had never
    /// been hinted at all. That is the property that makes the tier safe when the context no longer
    /// matches, and it is why the probe is allowed to run without any envelope check.
    ///
    /// Best-effort throughout. A probe that times out, that cannot be attributed, or that fails for
    /// reasons no mutant can be blamed for is abandoned without a word: the ordinary convergence
    /// that follows asks the same question properly and is the one whose answer the run reports.
    fn probe(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        verb: &[&str],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<()> {
        let (candidates, deferred) = self.probe_sets(plan, select);

        if candidates.len() < PROBE_FLOOR {
            return Ok(());
        }

        events.build_progress(&format!(
            "probing {} that did not compile for an earlier run before building the rest",
            crate::report::quantity(candidates.len(), "mutant")
        ));

        // Marked before the build rather than after it, so a probe that fails in any of the ways
        // below is still never repeated: the cost of a wasted round is bounded at one per mutant
        // for the whole run.
        self.probed.extend(candidates.iter().copied());
        self.ordering.offered = self.ordering.offered.saturating_add(candidates.len());
        self.ordering.rounds = self.ordering.rounds.saturating_add(1);

        let guards = self.splices.instrument(work, plan, &deferred)?;

        let started = Instant::now();
        let outcome = run_cargo(work, plan, verb, select, limits, self.first_round, events)?;
        let elapsed = started.elapsed();

        // Counted as a round of this run's build time because that is what it is, and hiding it
        // would make the reported build total disagree with the clock. It is deliberately not
        // charged against `--rollback-rounds`, which caps how many times a build may withdraw
        // before it is declared unconvergeable: the probe is an extra round the run chose to
        // spend, and letting it eat that budget could turn a build that would have converged into
        // an abandoned population.
        self.total_rounds = self.total_rounds.saturating_add(1);

        let Some(stdout) = outcome.stdout else {
            self.history.push(Round { elapsed, withdrew: 0 });

            return Ok(());
        };

        // Deliberately not allowed to set `first_round`, which every later build timeout is scaled
        // from. A probe compiles a fraction of the mutants and usually stops on the first errors,
        // so its elapsed time is not what a full round of this build costs — adopting it as the
        // reference would set every later budget from a build that was never comparable, and the
        // run would start timing out builds that are merely honest about their size.

        if outcome.succeeded {
            // Every hint was wrong: nothing here is unviable now. Nothing is withdrawn and nothing
            // is recorded against these mutants — the round bought only the knowledge that it did
            // not need to be taken, which `offered` against `confirmed` is what reports.
            self.history.push(Round { elapsed, withdrew: 0 });

            return Ok(());
        }

        let blamed = blame(&stdout, &work.root, &guards);

        self.history.push(Round {
            elapsed,
            withdrew: blamed.len(),
        });

        self.ordering.confirmed = self.ordering.confirmed.saturating_add(blamed.len());

        for (ordinal, code) in blamed {
            let _ = self.withdrawn.insert(ordinal);
            let _ = self.census.entry(ordinal).or_insert(code);
        }

        Ok(())
    }

    /// The mutants to probe, and the exclusion set that leaves only them in the tree.
    ///
    /// Restricted to the packages this build actually compiles. A mutant outside the selection
    /// contributes no diagnostic to this build, so deferring it would buy nothing and would rewrite
    /// a file some later stage is about to want instrumented again — churn that costs rebuilds
    /// without answering anything.
    fn probe_sets(&self, plan: &Plan, select: Option<&[String]>) -> (Vec<u32>, HashSet<u32>) {
        let mine = |mutant: &Mutant| select.is_none_or(|names| names.iter().any(|name| name.as_str() == &*mutant.package));

        let mut candidates: Vec<u32> = Vec::new();
        let mut deferred = self.withdrawn.clone();

        for mutant in &plan.mutants {
            if mutant.ordinal == 0 || self.withdrawn.contains(&mutant.ordinal) || !mine(mutant) {
                continue;
            }

            if self.hinted.contains(&mutant.id) && !self.probed.contains(&mutant.ordinal) {
                candidates.push(mutant.ordinal);
            } else {
                let _ = deferred.insert(mutant.ordinal);
            }
        }

        // Sorted so that the probe a run takes depends only on the plan and the hints, never on the
        // iteration order of a set. The build keys by ordinal, but the reported counts and any
        // future tie-break would otherwise vary between two runs over an identical tree.
        candidates.sort_unstable();

        (candidates, deferred)
    }

    fn build_timeout_error(budget: Duration) -> Error {
        error!(
            "the build was still running after {budget:.0?} and was stopped. A run builds once, so a \
             build that does not finish costs the whole run; raise --build-timeout if this one is simply slow."
        )
    }

    fn unattributed_build_error(work: &Workspace, stdout: &str, stderr: &str) -> Error {
        let diagnostics = diagnostics(stdout);

        // A build that produced no diagnostics at all did not fail the way this message assumes.
        // The compiler was never reached — a build script panicked, a native library is missing, a
        // dependency would not resolve, a package spec was ambiguous — and every one of those is
        // explained on stderr and nowhere else. Saying "does not compile" here would send the
        // reader hunting for a broken mutant that was never generated.
        if diagnostics.is_empty() {
            return error!(
                "the instrumented tree failed to build, and the compiler reported nothing, so no \
                 mutant can be blamed for it. The cause is usually something cargo hit before it \
                 reached the code — a build script, a missing native dependency, a bad invocation — \
                 and it is almost always in what cargo said:\n\n{}\n\n{}",
                tail(&complaints(stderr), 30),
                work.inspect_hint()
            );
        }

        error!(
            "the instrumented tree does not compile and the failure could not be attributed to a mutant.\n\
             {}\n\n{}",
            work.inspect_hint(),
            leading(&diagnostics, DIAGNOSTIC_LIMIT)
        )
    }

    /// Explains a build that ran out of rollback rounds.
    ///
    /// `per_round` is what each round of this build blamed, oldest first, and it includes the round
    /// the limit stopped. Every entry is non-zero: a round that blames nothing is not a rollback
    /// failure at all and is reported by `unattributed_build_error` instead.
    fn rollback_limit_error(rounds: u32, limit: u32, per_round: &[usize], work: &Workspace, stdout: &str) -> Error {
        let blamed: usize = per_round.iter().sum();

        // Whether the rounds were still making progress is the one thing that decides what to do
        // next, and it is invisible from a total. A falling tail means the cap was simply too low
        // for this tree; a flat one means each round is uncovering as much as the last, and more
        // rounds will not help.
        let recent: Vec<String> = per_round.iter().rev().take(5).rev().map(usize::to_string).collect();

        error!(
            "the instrumented tree still does not compile after {rounds} of the {limit} rollback rounds \
             this build is allowed, having blamed unviable mutants in each of them ({blamed} blamed \
             during this build, the last round's among them not withdrawn because the limit stopped it).\n\
             Mutants blamed in the last rounds of this build: {}.\n\
             If those counts are falling, the tree was converging and --rollback-rounds is simply too \
             low for it. If they are flat, each round is uncovering as much as the last and raising \
             the limit will only make the failure slower.\n\
             {}\n\n{}",
            recent.join(", "),
            work.inspect_hint(),
            leading(&diagnostics(stdout), DIAGNOSTIC_LIMIT)
        )
    }

    fn missing_guard_error(missing: &Mutant) -> Error {
        error!(
            "internal error: no guard was emitted for the mutant at {}:{}, so it could not \
             be tested. Please report this.\n  {}",
            missing.file,
            missing.line,
            missing.describe()
        )
    }

    /// Checks that the copied tree compiles before a single mutant is applied to it.
    ///
    /// This is what makes every later compiler error attributable. The tree that the staged builds
    /// and the baseline compile is this same tree with guards written into it, so once this passes,
    /// an error that appears afterwards was introduced by a mutant and nothing else. Without it,
    /// gamma cannot tell a broken mutant from code that never compiled, and reports the second as
    /// though it were the first — which sends the reader hunting through their own source for a
    /// fault that was there before the tool arrived.
    ///
    /// It runs `cargo check` rather than a build because it is a question about the code and not
    /// about artifacts: no codegen, no linking, and nothing it produces is kept. What it cannot see
    /// is exactly what `check` never reaches — link failures and post-monomorphization errors — so
    /// passing here is a strong precondition rather than a total one, and the later builds still
    /// report a failure they cannot pin on any mutant instead of absorbing it.
    ///
    /// `--tests` is not optional. The baseline build compiles test targets, so leaving them out
    /// here would clear the libraries and let a broken test target fail later, unattributably, in
    /// the middle of a run that had already paid for instrumentation.
    pub(super) fn preflight(
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        mutating: &[String],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Preflight> {
        match Self::check(work, plan, select, mutating, limits, events) {
            Ok(()) => Ok(Preflight::narrow(Vec::new())),

            // A narrowed check is not a smaller version of the whole one: cargo unifies features
            // over the packages it is told to build, so a target that only compiles because some
            // package outside the selection switches a feature on fails here through no fault of
            // the tree. Accusing the caller's code of not compiling on that evidence would be the
            // very mistake this check exists to prevent, so the selection is abandoned and the
            // question asked again of the whole workspace, which is how `finish` treats the same
            // trap.
            Err(narrow) if select.is_some() => {
                events.build_progress("the selected packages alone did not build, checking the whole workspace instead");

                // The wider check is asked one question only: does the narrow failure survive real
                // feature unification? Its own diagnostics are not reported, because a workspace
                // this size usually has something broken in a package the caller never mentioned,
                // and answering "your tree does not compile" with errors from a crate they did not
                // choose to mutate sends them to fix the wrong thing. What they can act on is the
                // failure in their own selection.
                //
                // Which scope answered is carried back rather than discarded. A wide success is not
                // "the tree compiles"; it is "the tree compiles when cargo unifies features over
                // every member", and a later build that narrowed again would reproduce the very
                // failure this branch has just proved is not any mutant's doing.
                if Self::check(work, plan, None, mutating, limits, events).is_ok() {
                    return Ok(Preflight {
                        whole_workspace: true,
                        dropped: Vec::new(),
                    });
                }

                Self::retreat(work, plan, select, mutating, limits, events, narrow)
            }

            Err(error) => Err(error),
        }
    }

    /// The last attempt: check only the packages this run is actually mutating.
    ///
    /// Both wider questions have now failed, and neither answers the one that matters. The
    /// selection is everything that can reach a mutant, and the workspace is everything at all, so
    /// a single package that does not compile — a `sys` crate without its native library, a
    /// sibling broken by somebody else's commit — fails both while saying nothing about the code
    /// the caller asked to measure. Refusing to run on that evidence turns a workspace's unrelated
    /// breakage into a tool that cannot be used at all, when narrowing the scope by hand would
    /// have worked: a flag the caller had no reason to know they needed.
    ///
    /// Succeeding here is not free, and the cost is not the tool's to hide. The packages dropped
    /// are the ones whose *tests* can no longer convict anything, so a mutant one of them would
    /// have killed now survives — a survivor that reads as a gap in the suite and is nothing of
    /// the kind. They are returned so the run can narrow its oracle to match what it checked, and
    /// so the report can name them.
    ///
    /// The narrow error is what a failure here reports, not this attempt's own. Features are not
    /// unified the way the real build unifies them over a selection this small, so its diagnostics
    /// are the least trustworthy of the three, while the narrow ones are about the selection the
    /// builds will genuinely compile.
    fn retreat(
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        mutating: &[String],
        limits: BuildLimits,
        events: &mut dyn Events,
        narrow: Error,
    ) -> Result<Preflight> {
        let dropped: Vec<String> = select
            .unwrap_or_default()
            .iter()
            .filter(|package| !mutating.contains(package))
            .cloned()
            .collect();

        // The selection was already nothing but the mutated packages, so this attempt is the first
        // one again and would fail again. There is no narrower run to retreat to.
        if dropped.is_empty() {
            return Err(narrow);
        }

        events.build_progress("the whole workspace did not build either, checking only the packages being mutated");

        Self::check(work, plan, Some(mutating), mutating, limits, events).map_err(|_last| narrow)?;

        Ok(Preflight::narrow(dropped))
    }

    /// Runs one preflight check over the packages named, or the whole workspace when none are.
    fn check(
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        mutating: &[String],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<()> {
        let outcome = run_cargo(work, plan, &["check", "--tests", "--keep-going"], select, limits, None, events)?;

        let Some(stdout) = outcome.stdout else {
            return Err(Self::build_timeout_error(limits.budget(None).unwrap_or_default()));
        };

        if outcome.succeeded {
            return Ok(());
        }

        let mut diagnostics = diagnostics(&stdout);

        prioritize(&mut diagnostics, &manifests_of(plan, &work.root, mutating));

        // Nothing on the JSON stream means the compiler was never reached, which is the same class
        // of failure `unattributed_build_error` describes and wants the same explanation. Saying
        // "does not compile" over an empty diagnostic list would be a lie about a build script or a
        // missing native library.
        if diagnostics.is_empty() {
            return Err(error!(
                "the tree could not be checked, and the compiler reported nothing, so the cause is \
                 something cargo hit before it reached the code — a build script, a missing native \
                 dependency, a bad invocation:\n\n{}",
                tail(&complaints(&outcome.stderr), 30)
            ));
        }

        Err(error!(
            "this tree does not compile before any mutation is applied, so there is nothing to \
             measure against.\n\
             These are the compiler's own errors, on the unmodified sources. Note that `cargo build` \
             alone would not show them, because it does not build test targets; `cargo check --tests` \
             reproduces them. A feature selection that leaves a test target's dependencies switched \
             off is the usual cause.\n\n{}",
            leading(&diagnostics, DIAGNOSTIC_LIMIT)
        ))
    }

    /// Compiles one stage's libraries, so its mutants are ruled on before anything downstream.
    ///
    /// Only libraries and binaries are built, never test targets. Every mutant lives in one of
    /// those, so this sees every diagnostic a mutant can cause, and it avoids the one thing a
    /// subset build cannot reproduce: cargo resolves features over the packages being built, so a
    /// test target that relies on a feature some other package switches on does not compile on its
    /// own. Those targets are left to the whole-workspace build, where the features are the real
    /// ones.
    /// A stage that cannot be made to compile does not stop the run. Its own mutants are taken out
    /// of the tree — which restores exactly the sources the preflight check already proved compile
    /// — and what it gave up on is returned so the run can report it. See [`Self::abandon`].
    pub(super) fn stage(
        &mut self,
        work: &Workspace,
        plan: &mut Plan,
        packages: &[String],
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Option<Abandoned>> {
        // Nothing is narrated from inside a stage: the stage reports what it found and what it
        // withdrew as one line when it is done, and a round-by-round commentary underneath that
        // would bury the sequence the whole arrangement exists to show.
        match self.converge(work, plan, self.scoped(Some(packages)), &["build", "--keep-going"], limits, events)? {
            Convergence::Built(stdout) => {
                self.remember_compiled(&stdout, &work.root);
                Ok(None)
            }
            Convergence::Stuck(reason) => Ok(Some(self.abandon(plan, Some(packages), &reason))),
        }
    }

    /// Takes a population out of the run because the build holding it could not be made to compile.
    ///
    /// `packages` names whose mutants to give up on, or is `None` for every one still live.
    ///
    /// Each mutant is recorded as [`Outcome::NotBuilt`] rather than [`Outcome::CompileError`]: the
    /// compiler never accused it of anything, and reporting a mutant nobody judged as unviable
    /// would be inventing a verdict. They are added to the withdrawal set as well, so the next
    /// build instruments the tree without them — that is what makes carrying on possible, since a
    /// tree with every one of them withdrawn is the pristine tree the preflight check cleared.
    fn abandon(&mut self, plan: &mut Plan, packages: Option<&[String]>, reason: &Error) -> Abandoned {
        let mut ordinals = Vec::new();

        for mutant in &mut plan.mutants {
            let mine = packages.is_none_or(|packages| packages.iter().any(|package| package.as_str() == &*mutant.package));

            if mutant.ordinal == 0 || !mine || self.withdrawn.contains(&mutant.ordinal) {
                continue;
            }

            mutant.outcome = Outcome::NotBuilt;
            mutant.note = Some("the build this mutant belongs to could not be made to compile, so it was never run".to_owned());

            ordinals.push(mutant.ordinal);
        }

        self.withdrawn.extend(ordinals.iter().copied());
        self.abandoned.extend(ordinals.iter().copied());
        ordinals.sort_unstable();

        Abandoned {
            reason: reason.to_string(),
            ordinals,
        }
    }

    /// Writes the withdrawal verdicts back onto the plan.
    ///
    /// Only mutants the compiler actually blamed are called unviable; what was abandoned wholesale
    /// already carries [`Outcome::NotBuilt`] from [`Self::abandon`] and keeps it.
    pub(super) fn settle(&self, plan: &mut Plan) {
        for mutant in &mut plan.mutants {
            if self.abandoned.contains(&mutant.ordinal) {
                mutant.outcome = Outcome::NotBuilt;
                mutant.note =
                    Some("the instrumented forms in this item could not compile together, so its mutants were not run".to_owned());
            } else if self.withdrawn.contains(&mutant.ordinal) {
                mutant.outcome = Outcome::CompileError;
            }
        }
    }

    /// Compiles the test targets of `select`, or of the whole workspace when it is `None`.
    ///
    /// Returns cargo's JSON stream, whose artifact messages name the test binaries.
    fn compile(
        &mut self,
        work: &Workspace,
        plan: &Plan,
        select: Option<&[String]>,
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Convergence> {
        // `cargo build --tests` emits the same compiler-artifact executable messages consumed by
        // `test_binaries`, while `--keep-going` lets convergence collect diagnostics from siblings
        // after one target fails. Reusing this stream avoids a second cache-hit Cargo invocation.
        self.converge(
            work,
            plan,
            select,
            &["build", "--tests", "--examples", "--keep-going"],
            limits,
            events,
        )
    }

    /// Builds the whole workspace, which is what decides the run.
    ///
    /// The staged builds before this one are a way of ruling on mutants early and of saying what is
    /// happening while it happens; this is the build whose feature resolution matches the one
    /// `cargo test` would use, and the binaries come from it for that reason.
    ///
    /// A build that cannot be made to compile leaves no test binary to judge anything with, so
    /// every mutant still live is abandoned and the returned [`Build`] says so. The run reports
    /// what it has rather than exiting with nothing.
    pub(super) fn finish(
        mut self,
        work: &Workspace,
        plan: &mut Plan,
        select: Option<&[String]>,
        limits: BuildLimits,
        events: &mut dyn Events,
    ) -> Result<Build> {
        let select = self.scoped(select);
        let mut widened = false;

        let converged = match self.compile(work, plan, select, limits, events)? {
            Convergence::Built(stdout) => Convergence::Built(stdout),

            // A narrowed build is not merely a smaller version of the whole one: cargo unifies
            // features over the packages it is told to build, so a test target that only compiles
            // because a package left out of the selection switches a feature on will fail here and
            // will fail in a way no mutant can be blamed for. That is a wrong answer to the
            // question the run is asking, so the selection is abandoned rather than reported.
            Convergence::Stuck(narrow) if select.is_some() => {
                widened = true;

                match self.compile(work, plan, None, limits, events)? {
                    Convergence::Built(stdout) => Convergence::Built(stdout),
                    Convergence::Stuck(_whole) => Convergence::Stuck(narrow),
                }
            }

            Convergence::Stuck(reason) => Convergence::Stuck(reason),
        };

        let stdout = match converged {
            Convergence::Built(stdout) => stdout,

            Convergence::Stuck(reason) => {
                let stuck = self.abandon(plan, None, &reason);

                self.settle(plan);

                return Ok(Build {
                    history: self.history.clone(),
                    census: self.tally(plan),
                    binaries: Vec::new(),
                    withdrawn: self.withdrawn.len().saturating_sub(self.abandoned.len()),
                    rounds: self.total_rounds,
                    widened,
                    stuck: Some(stuck),
                    ordering: self.ordering,
                });
            }
        };

        self.settle(plan);
        self.remember_compiled(&stdout, &work.root);

        // Runs after the withdrawal above so that a mutant which genuinely failed to compile keeps
        // that more specific verdict; see [`withdraw_uncompiled`] for why the set is only trusted
        // when it agrees with the survey at all.
        if let Some(compiled) = &self.compiled {
            withdraw_uncompiled(plan, compiled);
        }

        Ok(Build {
            history: self.history.clone(),
            census: self.tally(plan),
            binaries: test_binaries(&stdout),
            withdrawn: self.withdrawn.len().saturating_sub(self.abandoned.len()),
            rounds: self.total_rounds,
            widened,
            stuck: None,
            ordering: self.ordering,
        })
    }

    /// How many mutants have been withdrawn so far.
    pub(super) fn withdrawn(&self) -> usize {
        self.withdrawn.len()
    }

    /// Adds one successful Cargo invocation's dep-info to the run-wide source inventory.
    fn remember_compiled(&mut self, stdout: &str, root: &camino::Utf8Path) {
        if let Some(found) = compiled_sources(stdout, root) {
            self.compiled.get_or_insert_with(HashSet::default).extend(found);
        }
    }

    /// Groups the withdrawals by rustc error code and mutator, densest pair first.
    ///
    /// Counts distinct ordinals, because a diagnostic is not a mutant: one unviable mutant can draw
    /// a four-figure count of follow-on complaints, so anything tallying rows rather than mutants
    /// overstates the answer by an order of magnitude.
    fn tally(&self, plan: &Plan) -> Vec<Withdrawal> {
        let mut mutators: HashMap<u32, &str> = HashMap::default();

        for mutant in &plan.mutants {
            let _ = mutators.insert(mutant.ordinal, &mutant.mutator);
        }

        let mut counts: HashMap<(&str, &str), usize> = HashMap::default();

        for (ordinal, code) in &self.census {
            let mutator = mutators.get(ordinal).copied().unwrap_or("");

            *counts.entry((code.as_str(), mutator)).or_default() += 1;
        }

        let mut census: Vec<Withdrawal> = counts
            .into_iter()
            .map(|((code, mutator), mutants)| Withdrawal {
                code: code.to_owned(),
                mutator: mutator.to_owned(),
                mutants,
            })
            .collect();

        // Descending by weight, then by name, so that the line worth reading is the first one and
        // two runs over the same tree print the same thing.
        census.sort_by(|left, right| {
            right
                .mutants
                .cmp(&left.mutants)
                .then_with(|| left.code.cmp(&right.code))
                .then_with(|| left.mutator.cmp(&right.mutator))
        });

        census
    }
}

/// Marks every pending mutant whose file the compiler never read as not built.
///
/// A mutant in a file no compilation opened cannot be judged by any test, so it is taken out of the
/// run here rather than left to be reported as a survivor later.
///
/// The agreement check in front of the loop exists because the failure this could otherwise cause
/// is silent and expensive: if dep-info ever spelled its paths differently from the way the survey
/// spells them, nothing would match, every mutant would be excused, and the run would report a
/// flattering score with no sign that anything had gone wrong. A set that names not one file the
/// survey found is a set we do not understand, so nothing is concluded from it.
///
/// The check is deliberately whole-set and cannot be tightened to a per-file one: "this file is
/// missing from the compiled set" is exactly the question being asked, so a per-file guard would
/// answer it with itself. That makes the check blind to a spelling difference that affects only
/// *some* paths, which is why [`messages::compiled_sources`] has to decode the dep-info escaping
/// correctly rather than rely on being caught here.
fn withdraw_uncompiled(plan: &mut Plan, compiled: &HashSet<Utf8PathBuf>) {
    if !plan.files.iter().any(|file| compiled.contains(&file.path)) {
        return;
    }

    for mutant in &mut plan.mutants {
        if mutant.outcome == Outcome::Pending && !compiled.contains(&*mutant.file) {
            mutant.outcome = Outcome::NotBuilt;
        }
    }
}
