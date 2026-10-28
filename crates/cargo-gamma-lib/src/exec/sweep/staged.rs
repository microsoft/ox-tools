// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-mutant execution stages and intermediate probe state.

use super::*;

/// Campaign-wide execution barriers, from exact evidence through speculative probes to fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Exact,
    Inferred,
    Binaries,
    Fallback,
}

impl Stage {
    pub(super) const fn sequence() -> &'static [Self] {
        &[Self::Exact, Self::Inferred, Self::Binaries, Self::Fallback]
    }
}

/// Per-mutant outcomes retained across stages to avoid repeating completed binary and named runs.
#[derive(Default)]
pub(super) struct ProbeState {
    pub(super) binary_runs: Vec<Option<Verdict>>,
    pub(super) passed_tests: crate::HashSet<(usize, String)>,
}

/// Queued mutant bookkeeping across invocations, including accumulated execution time and evidence.
#[derive(Default)]
pub(super) struct PendingState {
    pub(super) probes: ProbeState,
    pub(super) started_ms: Option<u64>,
    pub(super) execution_ms: u64,
    pub(super) finished: bool,
}

/// Borrowed campaign context for one mutant's current stage invocation; owns no cross-stage evidence.
struct Execution<'a> {
    work: &'a Workspace,
    ordinal: u32,
    reachable: &'a [&'a TestBinary],
    learning: Option<(&'a FileLearning, &'a str)>,
    negative: Option<(&'a NegativeLearning, &'a SiteIdentity, bool)>,
    timeout_multiplier: Option<f64>,
    sweep: Sweep,
    tally: &'a Tally,
}

/// Runs one stage, retaining probe evidence and returning a judgement only when that stage settles it.
#[expect(clippy::too_many_arguments, reason = "execution stages share borrowed campaign and probe state")]
pub(super) fn judge(
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
    phase: Stage,
    state: &mut ProbeState,
) -> Option<Judgement> {
    if phase == Stage::Exact && hint.is_some() {
        tally.exact_candidates.fetch_add(1, Ordering::Relaxed);
    }
    if matches!(phase, Stage::Inferred | Stage::Binaries) {
        tally.inferred_candidates.fetch_add(candidates.len(), Ordering::Relaxed);
        for candidate in candidates {
            let counter = match candidate {
                Candidate::Exact(_) | Candidate::ItemBinary(_) => &tally.item_candidates,
                Candidate::ReachFileBinary(_) => &tally.reach_candidates,
                Candidate::FileBinary(_) => &tally.file_candidates,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }
    let execution = Execution {
        work,
        ordinal,
        reachable,
        learning,
        negative,
        timeout_multiplier,
        sweep,
        tally,
    };
    state.binary_runs.resize_with(reachable.len(), || None);
    let judged = match phase {
        Stage::Exact => execution.exact(hint, state),
        Stage::Inferred => execution.cases(candidates, state),
        Stage::Binaries | Stage::Fallback => execution.binaries(phase, candidates, state),
    };
    if let Some(judged) = &judged {
        tally
            .named_kills
            .lock()
            .expect("a kill-count panic can leave counts and credited mutants inconsistent")
            .record(ordinal, judged);
    }
    judged
}

impl Execution<'_> {
    fn execute(&self, index: usize, only: Only<'_>, kind: Option<ProbeKind>, rank: usize) -> core::result::Result<BinaryRun, Judgement> {
        let binary = self.reachable[index];
        if let Some(kind) = kind {
            kind.attempt(self.tally);
            self.tally.probes.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(only, Only::All) {
            self.tally.whole_selections.fetch_add(1, Ordering::Relaxed);
        }
        record_launch(self.tally);
        let tier = kind.map_or(SelectionTier::Whole, ProbeKind::selection_tier);
        execution_tier(tier, false);
        let started = Instant::now();
        let run = run_binary_observed(
            self.work,
            binary,
            Attempt {
                active: Some(self.ordinal),
                timeout: binary.budget_for(self.timeout_multiplier, self.sweep.timeout_floor),
                stall: self.sweep.stall,
                request: MemoryRequest {
                    calibrating: false,
                    meter: self.sweep.meter,
                    limit: binary.memory,
                },
                only,
            },
            self.sweep.confirm,
        );
        let elapsed = started.elapsed();
        if matches!(run.verdict, Verdict::Failed(_) | Verdict::TestEnumerationFailed(_)) {
            self.tally
                .binary_kills
                .lock()
                .expect("a kill-count panic can leave counts and credited mutants inconsistent")
                .record(self.ordinal, binary);
        }
        let observation = transfer_observation(&run.verdict);
        if negative_reach_is_final(&run, only, self.negative.is_some_and(|(_, _, deterministic)| deterministic))
            && let Some((negative, site, _)) = self.negative
        {
            negative.record(site, binary);
        }
        if observation == Some(true)
            && let Some(kind) = kind
        {
            kind.hit(self.tally);
        }
        trace_selection(
            self.tally,
            self.ordinal,
            binary,
            match only {
                Only::One(test) => Some(test.to_owned()),
                Only::All => None,
            },
            tier,
            rank,
            observation,
            matches!(only, Only::All),
            elapsed,
            self.reachable
                .iter()
                .fold(Duration::ZERO, |total, binary| total.saturating_add(binary.baseline)),
            false,
        );
        if let Some((learning, item)) = self.learning {
            if matches!(only, Only::All) {
                if run.reach == ReachObservation::Reached {
                    learning.reached(item, binary, elapsed);
                }
                if observation == Some(false) {
                    learning.record_canonical_pass(item, binary, run.reach == ReachObservation::Reached);
                }
                if let (Some(kind), Some(hit)) = (kind, observation) {
                    let identity = BinaryIdentity::from_binary(binary);
                    let candidate = match kind {
                        ProbeKind::Item => Candidate::ItemBinary(identity),
                        ProbeKind::Reach => Candidate::ReachFileBinary(identity),
                        ProbeKind::File => Candidate::FileBinary(identity),
                        ProbeKind::Exact => unreachable!("full inferred binaries use a binary inference tier"),
                    };
                    learning.observe(item, &candidate, hit, elapsed);
                }
            } else if let (Only::One(name), Some(hit)) = (only, observation)
                && (!hit || matches!(&run.verdict, Verdict::Failed(Some(failed)) if failed == name))
            {
                learning.observe(item, &Candidate::Exact(killer_for(binary, name)), hit, elapsed);
            }
        }
        if let Verdict::Unmetered(reason) = &run.verdict {
            return Err(Judgement::Abandoned(reason.clone()));
        }
        Ok(run)
    }

    fn exact(&self, hint: Option<&Killer>, state: &mut ProbeState) -> Option<Judgement> {
        let hint = hint?;
        let index = self.reachable.iter().position(|binary| binary.matches_killer(hint))?;
        if negative_excludes(self.negative, self.reachable[index], self.timeout_multiplier, self.sweep) {
            return None;
        }
        if !HarnessFilters::parse(self.work.test_arguments()).admits(&hint.test) {
            return None;
        }
        let run = match self.execute(index, Only::One(&hint.test), Some(ProbeKind::Exact), 1) {
            Ok(run) => run,
            Err(judged) => return Some(judged),
        };
        if matches!(run.verdict, Verdict::Passed) {
            state.passed_tests.insert((index, hint.test.clone()));
        }
        probe_kill(self.reachable[index], run.verdict, Some(&hint.test))
    }

    fn cases(&self, candidates: &[Candidate], state: &mut ProbeState) -> Option<Judgement> {
        let mut selections = Vec::new();
        let mut seen = crate::HashSet::default();
        let filters = HarnessFilters::parse(self.work.test_arguments());
        for candidate in candidates {
            let Candidate::Exact(killer) = candidate else { continue };
            let Some(index) = self.reachable.iter().position(|binary| binary.matches_killer(killer)) else {
                continue;
            };
            if negative_excludes(self.negative, self.reachable[index], self.timeout_multiplier, self.sweep) {
                continue;
            }
            if !filters.admits(&killer.test) || !seen.insert((index, killer.test.as_str())) {
                continue;
            }
            if state.passed_tests.contains(&(index, killer.test.clone())) {
                record_duplicate_avoided(self.tally);
                continue;
            }
            selections.push((index, killer.test.as_str()));
        }
        self.tally
            .named_kills
            .lock()
            .expect("a kill-count panic can leave counts and credited mutants inconsistent")
            .rank(&mut selections, self.reachable);
        // Six is a conservative policy default, not a measured optimum: bound speculative
        // per-case launch costs before advancing to whole-binary inference.
        for (rank, (index, name)) in selections.into_iter().take(6).enumerate() {
            let run = match self.execute(index, Only::One(name), Some(ProbeKind::Item), rank + 1) {
                Ok(run) => run,
                Err(judged) => return Some(judged),
            };
            if matches!(run.verdict, Verdict::Passed) {
                state.passed_tests.insert((index, name.to_owned()));
            }
            if let Some(judged) = probe_kill(self.reachable[index], run.verdict, Some(name)) {
                return Some(judged);
            }
        }
        None
    }

    fn binaries(&self, phase: Stage, candidates: &[Candidate], state: &mut ProbeState) -> Option<Judgement> {
        let mut order = Vec::new();
        for candidate in candidates {
            let identity = match candidate {
                Candidate::Exact(killer) => BinaryIdentity::from_killer(killer),
                Candidate::ItemBinary(identity) | Candidate::ReachFileBinary(identity) | Candidate::FileBinary(identity) => {
                    identity.clone()
                }
            };
            if let Some(index) = self.reachable.iter().position(|binary| identity.names(binary))
                && !order.iter().any(|(known, _)| *known == index)
            {
                order.push((index, Some(candidate.probe_kind())));
            }
        }
        if phase == Stage::Fallback {
            order = self
                .tally
                .binary_kills
                .lock()
                .expect("a kill-count panic can leave counts and credited mutants inconsistent")
                .order(self.ordinal, self.reachable)
                .into_iter()
                .map(|index| (index, None))
                .collect();
        }
        for (rank, (index, kind)) in order.into_iter().enumerate() {
            if state.binary_runs[index].is_some() {
                record_duplicate_avoided(self.tally);
                continue;
            }
            if negative_excludes(self.negative, self.reachable[index], self.timeout_multiplier, self.sweep) {
                if phase == Stage::Fallback {
                    self.tally.saved.fetch_add(1, Ordering::Relaxed);
                    self.tally.reach_saved.fetch_add(1, Ordering::Relaxed);
                    self.tally.uncovered_selections.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            let run = match self.execute(index, Only::All, kind, rank + 1) {
                Ok(run) => run,
                Err(judged) => return Some(judged),
            };
            if matches!(run.verdict, Verdict::Failed(_) | Verdict::TestEnumerationFailed(_)) {
                return terminal_judgement(self.reachable[index], run.verdict);
            }
            state.binary_runs[index] = Some(run.verdict);
        }
        if phase != Stage::Fallback {
            return None;
        }
        let ran = state.binary_runs.iter().any(Option::is_some);
        for (index, run) in state.binary_runs.iter_mut().enumerate() {
            if let Some(verdict) = run.take()
                && let Some(judged) = terminal_judgement(self.reachable[index], verdict)
            {
                return Some(judged);
            }
        }
        Some(Judgement::Reached(
            if ran { Outcome::Survived } else { Outcome::NoCoverage },
            None,
            None,
        ))
    }
}

pub(super) fn killer_for(binary: &TestBinary, test: &str) -> Killer {
    Killer {
        package: binary.package.clone(),
        target: binary.killer_target(),
        test: test.to_owned(),
    }
}

fn probe_kill(binary: &TestBinary, verdict: Verdict, selected: Option<&str>) -> Option<Judgement> {
    match verdict {
        Verdict::Failed(named) => Some(Judgement::Reached(
            Outcome::Killed,
            named.as_deref().or(selected).map(|test| killer_for(binary, test)),
            None,
        )),
        Verdict::TestEnumerationFailed(output) => Some(Judgement::Reached(
            Outcome::Killed,
            None,
            Some(enumeration_note(&binary.path, &output)),
        )),
        _ => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;
