// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Projecting what a run will cost, before paying for it.
//!
//! The failure mode this exists to prevent is discovering a four-hour job four hours in. Everything
//! here is derived from measurements a run has already taken by the time the first mutant would
//! start: the build really built, the baseline really ran, and unviable mutants were really
//! withdrawn. Only one quantity is genuinely unknown before mutants execute — how much of the suite
//! a killed mutant gets through before something fails — and the projection says which assumption
//! it made about it rather than folding it silently into a single confident number.

use core::time::Duration;

use crate::HashMap;
use crate::advise::human;
use crate::model::{Mutant, Outcome};
use crate::report::quantity;

/// Enough completed mutants to distinguish throughput from one unusually quick launch.
const MIN_LIVE_SAMPLES: usize = 8;

/// Enough observations to trust the campaign even when the completed work was unusually cheap.
const REPRESENTATIVE_SAMPLE_FLOOR: usize = 32;

/// Share of predicted work that makes a smaller sample representative.
const MIN_OBSERVED_WORK_SHARE: f64 = 0.02;

/// Weight retained by the pre-sweep model while live observations accumulate.
const PRIOR_STRENGTH: f64 = 8.0;

const PRIOR_SAMPLES: usize = 8;

/// How quickly calibration follows the recent part of a campaign.
const CALIBRATION_ALPHA: f64 = 0.25;

/// The share of the suite a mutant that fails or survives is assumed to reach before it is judged.
///
/// A killed mutant almost never runs the whole suite: something fails, and with a fail-fast binary
/// the rest is never reached. Assuming the full baseline for every mutant would overestimate badly
/// on a healthy codebase, which is the failure mode that makes an estimate useless — nobody plans
/// against a number they have learned is always too big. A survivor does run the whole suite, so
/// this is a blend across both rather than a claim about either.
const KILLED_SHARE: f64 = 0.60;

/// The share of mutants assumed to hang, for the middle of the range.
///
/// This is the one quantity that decides what a run costs, and the one nothing measured before the
/// mutants execute can supply. It matters far more than its size suggests: a mutant that hangs is
/// stopped by a budget with a floor under it, then re-run to confirm, so on a suite that finishes
/// in a moment one hang can cost as much as several thousand mutants that do not. Turning a loop
/// counter into an infinite loop is an ordinary mutation, not an exotic one, so assuming none is
/// not the safe choice — it is the choice that produced an estimate off by two orders of magnitude.
const STALL_SHARE: f64 = 0.05;

/// The share of mutants assumed to hang, for the top of the range, as a percentage.
///
/// Held as a percentage because it is printed as one. Deriving the fraction from the number that is
/// displayed keeps the projection and its explanation from ever disagreeing.
const STALL_PERCENT_HIGH: u32 = 15;

/// The share of mutants assumed to hang, for the top of the range.
fn stall_share_high() -> f64 {
    f64::from(STALL_PERCENT_HIGH) / 100.0
}

/// The execution shape used to predict one mutant's test cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkKind {
    /// A durable exact killer is expected to decide the mutant with one probe.
    Exact,

    /// Census evidence selected measured individual tests.
    Selected,

    /// A learned candidate is tried before its whole-binary fallback.
    Hinted,

    /// Reachable test binaries run without a narrower prediction.
    Whole,

    /// Complete census evidence found no test that reaches the mutant.
    Uncovered,
}

impl WorkKind {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            Self::Exact => 0,
            Self::Selected => 1,
            Self::Hinted => 2,
            Self::Whole => 3,
            Self::Uncovered => 4,
        }
    }
}

/// Predicted work for one live mutant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationWork {
    /// The run-local selector used to join scheduling events to this prediction.
    pub(crate) ordinal: u32,

    /// How the tests are expected to be selected.
    pub(crate) kind: WorkKind,

    /// Expected cost when ordinary test execution judges the mutant.
    settled: Duration,

    /// Cost when every selected test runs.
    suite: Duration,

    /// Expected cost when one test binary consumes its timeout and confirmation allowance.
    stalled: Duration,
}

impl MutationWork {
    /// Builds one prediction from measured test work and one representative timeout budget.
    #[must_use]
    pub(crate) fn new(ordinal: u32, kind: WorkKind, suite: Duration, single_budget: Duration, confirmation_runs: u32) -> Self {
        Self {
            ordinal,
            kind,
            settled: scale_duration(suite, KILLED_SHARE),
            suite,
            stalled: suite.saturating_add(single_budget.saturating_mul(confirmation_runs.saturating_add(1))),
        }
    }

    pub(crate) const fn scheduling_cost(self) -> Duration {
        self.suite
    }

    fn predicted(self, outcome: OutcomeKind) -> Duration {
        match outcome {
            OutcomeKind::Killed => self.settled,
            OutcomeKind::Full => self.suite,
            OutcomeKind::Resource => self.stalled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    Killed,
    Full,
    Resource,
}

impl OutcomeKind {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::Killed => 0,
            Self::Full => 1,
            Self::Resource => 2,
        }
    }

    const fn of(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Timeout | Outcome::OutOfMemory => Self::Resource,
            Outcome::Survived | Outcome::Flaky => Self::Full,
            _ => Self::Killed,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Confidence {
    Low,
    Middle,
    High,
}

impl Confidence {
    const fn prior(self) -> [f64; OutcomeKind::COUNT] {
        match self {
            Self::Low => [0.85, 0.15, 0.0],
            Self::Middle => [0.80, 0.15, STALL_SHARE],
            Self::High => [0.70, 0.15, 0.15],
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Calibration {
    recent: f64,
    samples: usize,
}

impl Calibration {
    fn observe(&mut self, actual: Duration, predicted: Duration) {
        let denominator = predicted.as_secs_f64().max(0.001);
        let ratio = (actual.as_secs_f64() / denominator).clamp(0.01, 10_000.0);

        self.recent = if self.samples == 0 {
            ratio
        } else {
            self.recent.mul_add(1.0 - CALIBRATION_ALPHA, ratio * CALIBRATION_ALPHA)
        };
        self.samples = self.samples.saturating_add(1);
    }

    fn factor(self) -> f64 {
        if self.samples == 0 {
            return 1.0;
        }

        let evidence = count_as_f64(self.samples.min(PRIOR_SAMPLES));
        self.recent.mul_add(evidence, PRIOR_STRENGTH) / (PRIOR_STRENGTH + evidence)
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkState {
    Queued,
    Active(Duration),
    Done,
}

#[derive(Debug, Clone, Copy)]
struct LiveWork {
    prediction: MutationWork,
    state: WorkState,
}

/// A low and high estimate of wall time still required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Remaining {
    pub(crate) low: Duration,
    pub(crate) high: Duration,
}

/// Adaptive live projection for a mutation sweep.
///
/// The pre-sweep workload remains the prior. Completed work calibrates each execution-shape and
/// outcome bucket with a bounded-memory EWMA, while active work contributes only its predicted
/// residual. Remaining work is placed onto worker lanes to project the tail rather than dividing a
/// serial total by a nominal job count.
#[derive(Debug)]
pub(crate) struct LiveEstimate {
    work: Vec<LiveWork>,
    by_ordinal: HashMap<u32, usize>,
    next_unfinished: usize,
    jobs: usize,
    done: usize,
    completed_middle: Duration,
    total_middle: Duration,
    calibration: [[Calibration; OutcomeKind::COUNT]; WorkKind::COUNT],
}

impl LiveEstimate {
    #[must_use]
    pub(crate) fn new(work: &[MutationWork], jobs: usize) -> Self {
        let total_middle = work.iter().fold(Duration::ZERO, |total, item| {
            total.saturating_add(prior_cost(*item, Confidence::Middle))
        });
        let by_ordinal = work
            .iter()
            .enumerate()
            .map(|(index, prediction)| (prediction.ordinal, index))
            .collect();

        Self {
            work: work
                .iter()
                .copied()
                .map(|prediction| LiveWork {
                    prediction,
                    state: WorkState::Queued,
                })
                .collect(),
            by_ordinal,
            next_unfinished: 0,
            jobs: jobs.max(1),
            done: 0,
            completed_middle: Duration::ZERO,
            total_middle,
            calibration: [[Calibration::default(); OutcomeKind::COUNT]; WorkKind::COUNT],
        }
    }

    pub(crate) fn start(&mut self, ordinal: u32, elapsed: Duration) {
        let Some(index) = self.by_ordinal.get(&ordinal).copied() else {
            return;
        };
        let work = self
            .work
            .get_mut(index)
            .expect("ordinal indices are built directly from the same work population");
        if matches!(work.state, WorkState::Queued) {
            work.state = WorkState::Active(elapsed);
        }
    }

    pub(crate) fn finish(&mut self, ordinal: u32, outcome: Outcome, actual: Duration) {
        let Some(index) = self.by_ordinal.get(&ordinal).copied() else {
            return;
        };
        let work = self
            .work
            .get_mut(index)
            .expect("ordinal indices are built directly from the same work population");
        if matches!(work.state, WorkState::Done) {
            return;
        }

        work.state = WorkState::Done;
        self.done = self.done.saturating_add(1);
        self.completed_middle = self
            .completed_middle
            .saturating_add(prior_cost(work.prediction, Confidence::Middle));

        let outcome = OutcomeKind::of(outcome);
        let predicted = work.prediction.predicted(outcome);
        self.calibration[work.prediction.kind.index()][outcome.index()].observe(actual, predicted);
    }

    /// Finishes the first unfinished item, used by callers that have no run-local identity.
    pub(crate) fn finish_next(&mut self, outcome: Outcome, actual: Duration) {
        while self
            .work
            .get(self.next_unfinished)
            .is_some_and(|work| matches!(work.state, WorkState::Done))
        {
            self.next_unfinished = self.next_unfinished.saturating_add(1);
        }
        if let Some(ordinal) = self.work.get(self.next_unfinished).map(|work| work.prediction.ordinal) {
            self.finish(ordinal, outcome, actual);
        }
    }

    #[must_use]
    pub(crate) fn remaining(&self, elapsed: Duration) -> Option<Remaining> {
        if self.done < MIN_LIVE_SAMPLES || self.done == self.work.len() || !self.representative() {
            return None;
        }

        let low = self.makespan(Confidence::Low, elapsed);
        let high = self.makespan(Confidence::High, elapsed).max(low);

        Some(Remaining { low, high })
    }

    fn representative(&self) -> bool {
        if self.done >= REPRESENTATIVE_SAMPLE_FLOOR {
            return true;
        }

        let total = self.total_middle.as_secs_f64();

        total == 0.0 || self.completed_middle.as_secs_f64() / total >= MIN_OBSERVED_WORK_SHARE
    }

    fn makespan(&self, confidence: Confidence, elapsed: Duration) -> Duration {
        let mut lanes = vec![Duration::ZERO; self.jobs];
        let mut queued = Vec::new();

        for work in &self.work {
            let predicted = self.calibrated(work.prediction, confidence);

            match work.state {
                WorkState::Queued => queued.push(predicted),
                WorkState::Active(started) => {
                    let spent = elapsed.saturating_sub(started);
                    let residual = active_residual(work.prediction, confidence, predicted, spent);
                    add_to_shortest_lane(&mut lanes, residual);
                }
                WorkState::Done => {}
            }
        }

        queued.sort_unstable_by(|left, right| right.cmp(left));
        for cost in queued {
            add_to_shortest_lane(&mut lanes, cost);
        }

        lanes.into_iter().max().unwrap_or_default()
    }

    fn calibrated(&self, work: MutationWork, confidence: Confidence) -> Duration {
        let mut probabilities = confidence.prior();
        let kind = &self.calibration[work.kind.index()];
        let class_sample_count = kind.iter().map(|bucket| bucket.samples).sum::<usize>();

        if class_sample_count > 0 {
            let denominator = PRIOR_STRENGTH + count_as_f64(class_sample_count);
            for (index, probability) in probabilities.iter_mut().enumerate() {
                *probability = PRIOR_STRENGTH.mul_add(*probability, count_as_f64(kind[index].samples)) / denominator;
            }
        }

        let mut cost = Duration::ZERO;

        for outcome in [OutcomeKind::Killed, OutcomeKind::Full, OutcomeKind::Resource] {
            let index = outcome.index();
            let observed = kind[index];
            let factor = observed.factor();
            cost = cost.saturating_add(scale_duration(work.predicted(outcome), probabilities[index] * factor));
        }

        cost
    }
}

fn prior_cost(work: MutationWork, confidence: Confidence) -> Duration {
    let priors = confidence.prior();

    [OutcomeKind::Killed, OutcomeKind::Full, OutcomeKind::Resource]
        .into_iter()
        .fold(Duration::ZERO, |total, outcome| {
            total.saturating_add(scale_duration(work.predicted(outcome), priors[outcome.index()]))
        })
}

fn active_residual(work: MutationWork, confidence: Confidence, predicted: Duration, spent: Duration) -> Duration {
    let total = if spent < predicted {
        predicted
    } else {
        match confidence {
            Confidence::Low => predicted,
            Confidence::Middle => predicted.max(predicted.saturating_add(work.stalled) / 2),
            Confidence::High => predicted.max(work.stalled),
        }
    };

    total.saturating_sub(spent)
}

fn add_to_shortest_lane(lanes: &mut [Duration], cost: Duration) {
    let lane = lanes
        .iter_mut()
        .min()
        .expect("LiveEstimate always creates at least one worker lane");
    *lane = lane.saturating_add(cost);
}

fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

fn scale_duration(duration: Duration, factor: f64) -> Duration {
    Duration::try_from_secs_f64(duration.as_secs_f64() * factor).unwrap_or(Duration::MAX)
}

/// What testing every live mutant once would cost, summed over the mutants.
///
/// Every field is a serial total over the live mutants, counting for each one only the test
/// binaries that can actually reach its package. Projecting from the whole baseline instead — as
/// though every mutant ran the entire suite — overestimates a loosely coupled workspace by roughly
/// the number of crates in it, which is exactly the shape of estimate nobody plans against because
/// they have learned it is always too big.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Workload {
    /// What testing them all once would cost.
    pub suite: Duration,

    /// What it would cost if every one of them ran every binary out of time.
    pub budget: Duration,

    /// What it would cost if every one of them ran a single average binary out of time.
    pub single: Duration,
}

/// A projection of what a run will cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    /// Mutants that would actually be tested.
    pub live: usize,

    /// Mutants withdrawn during the build because they could not compile.
    pub withdrawn: usize,

    /// Measured: the instrumented build.
    pub build: Duration,

    /// Measured: wall time for the concurrently run suite with no mutant active.
    pub baseline: Duration,

    /// Projected: testing every live mutant, at the configured parallelism, assuming the usual
    /// share of them hang.
    pub mutants: Duration,

    /// Projected: the same, assuming none of them hang.
    pub settled: Duration,

    /// Projected: the same, assuming an unusually large share of them hang.
    pub stalling: Duration,

    /// How many mutants are tested at once.
    pub jobs: usize,

    /// Projected: every live mutant running out the budget of every binary that can reach it.
    pub worst: Duration,
}

impl Estimate {
    /// The lower end of the range: nothing hangs.
    ///
    /// The width of this range is not a confidence interval dressed up as one. It is the single
    /// thing that decides what the run costs and cannot be measured before it starts, so a run
    /// whose ends are far apart is telling the reader something true: the cost is going to be
    /// decided by how many mutants hang, not by how many there are.
    #[must_use]
    pub fn low(&self) -> Duration {
        self.build + self.baseline + self.settled
    }

    /// The upper end of the range.
    ///
    /// Capped at the projected ceiling, which is the point where every mutant has already been
    /// given every second of test time it will ever get: a range whose top is above that is
    /// describing time that cannot be spent on tests.
    #[must_use]
    pub fn high(&self) -> Duration {
        (self.build + self.baseline + self.stalling).min(self.worst_case())
    }

    /// The ceiling on test time: every mutant hits its timeout and is then confirmed.
    ///
    /// A ceiling on the time spent *running tests*, and not on the time a run takes. It counts the
    /// confirmation run, which is what makes it a ceiling on that time rather than a guess at it: a
    /// mutant that exhausts its budget is not believed on the first try, so the path that costs the
    /// most costs several times its timeout rather than one of them.
    ///
    /// What it does not count is the fixed cost of putting each mutant through the machine —
    /// launching its processes, scheduling it onto a lane, and writing down what happened. Nothing
    /// measured before the first mutant runs prices that, so it is left out and said to be left
    /// out, rather than approximated into a number that would look like a bound and not be one. On
    /// a suite that finishes in a moment the fixed cost is the larger share, so a run can and does
    /// overshoot this; [`render`] says so on the line it is printed on.
    #[must_use]
    pub fn worst_case(&self) -> Duration {
        self.build + self.baseline + self.worst
    }
}

/// Projects a run from what the build and baseline already measured.
#[must_use]
pub fn project(mutants: &[Mutant], work: Workload, baseline: Duration, build: Duration, jobs: usize, confirm: bool) -> Estimate {
    let live = mutants
        .iter()
        // #[gamma::skip(relational.gt_to_ge, reason = "ordinal zero is the invariant marker for a mutant already removed from the live population, and therefore cannot simultaneously have Pending outcome")]
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .count();

    let withdrawn = mutants.iter().filter(|mutant| mutant.outcome == Outcome::CompileError).count();
    let lanes = u32::try_from(jobs.max(1)).unwrap_or(1);
    let confirmation_runs = if confirm { crate::exec::CONFIRM_FACTOR } else { 0 };

    Estimate {
        live,
        withdrawn,
        build,
        baseline,
        mutants: spend(work, lanes, STALL_SHARE, confirmation_runs),
        settled: spend(work, lanes, 0.0, confirmation_runs),
        stalling: spend(work, lanes, stall_share_high(), confirmation_runs),
        jobs,
        worst: work.budget.saturating_mul(confirmation_runs.saturating_add(1)) / lanes,
    }
}

/// What testing every live mutant comes to, if the given share of them hang.
///
/// The two outcomes are priced separately because they differ by orders of magnitude rather than by
/// a little. A mutant that is judged pays for the tests it reached. A mutant that hangs pays for
/// everything it got through, then for a whole budget it never finishes, and then for the
/// confirmation run that budget is not believed without — and that budget has a floor under it, so
/// on a quick suite it is not a multiple of the tests but a constant far larger than all of them.
fn spend(work: Workload, lanes: u32, stalling: f64, confirmation_runs: u32) -> Duration {
    let judged = work.suite.mul_f64(KILLED_SHARE * (1.0 - stalling));
    let hung = (work.suite + work.single.saturating_mul(confirmation_runs.saturating_add(1))).mul_f64(stalling);

    (judged + hung) / lanes
}

/// Renders a projection as the single line printed once the fixed cost is paid.
///
/// One line, because it is printed in the middle of a run whose build and baseline timings are
/// already on the screen directly above it; repeating them would be padding. What is left is the
/// only thing the reader cannot already see: how long the remaining wait is, and how bad it could
/// get.
///
/// The ends of the range are labelled with the assumption that produces each, rather than left to
/// look like a margin of error. A reader who sees a wide range and is told what widens it can do
/// something about it — lower the timeout floor, or find the mutants that hang — where a reader
/// shown a bare interval can only distrust it.
///
/// The ceiling is labelled with what it leaves out for the same reason. It bounds the time spent
/// running tests and nothing else: the fixed cost of launching, scheduling and recording each
/// mutant is not measured before the first one runs, so it is not in the number. On a suite that
/// finishes in a moment that cost is the larger share and a run will exceed the figure, which is
/// exactly the sort of surprise a CI budget is planned around.
#[must_use]
pub fn render(estimate: &Estimate) -> String {
    format!(
        "{} if none hang, {} if {}% do, for {} at {}; {} worst case for test time, before per-mutant overhead",
        human(estimate.low()),
        human(estimate.high()),
        STALL_PERCENT_HIGH,
        quantity(estimate.live, "mutant"),
        quantity(estimate.jobs, "job"),
        human(estimate.worst_case())
    )
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;

    use super::*;
    use crate::fixtures;

    fn mutant(ordinal: u32, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("m{ordinal}{outcome}").into(),
            ordinal,
            package: ("p".to_owned()).into(),
            file: (Utf8PathBuf::from("a.rs")).into(),
            outcome,
            ..fixtures::mutant()
        }
    }

    fn population() -> Vec<Mutant> {
        let mut mutants: Vec<Mutant> = (1..=100).map(|index| mutant(index, Outcome::Pending)).collect();

        mutants.push(mutant(0, Outcome::Ignored));
        mutants.push(mutant(101, Outcome::CompileError));
        mutants
    }

    /// A serial workload of `secs` seconds, and a worst case ten times as bad.
    fn work(secs: u64) -> Workload {
        Workload {
            suite: Duration::from_secs(secs),
            budget: Duration::from_secs(secs * 10),
            single: Duration::from_secs(secs * 5),
        }
    }

    /// A workload whose only cost is the suite, so a projection reduces to the settled case.
    fn suite_only(secs: u64) -> Workload {
        Workload {
            suite: Duration::from_secs(secs),
            ..Workload::default()
        }
    }

    #[test]
    fn only_mutants_that_would_run_are_counted() {
        let estimate = project(&population(), work(100), Duration::ZERO, Duration::from_secs(5), 1, true);

        assert_eq!(estimate.live, 100);
        assert_eq!(estimate.withdrawn, 1);
    }

    #[test]
    fn parallelism_divides_the_projection() {
        let one = project(&population(), work(1000), Duration::ZERO, Duration::from_secs(50), 1, true);
        let eight = project(&population(), work(1000), Duration::ZERO, Duration::from_secs(50), 8, true);

        assert_eq!(one.mutants / 8, eight.mutants);
        assert_eq!(one.worst / 8, eight.worst);
    }

    #[test]
    fn projection_uses_the_exact_lane_and_confirmation_arithmetic() {
        let load = Workload {
            suite: Duration::from_secs(1_000),
            budget: Duration::from_secs(800),
            single: Duration::from_secs(400),
        };
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 4, true);

        assert_eq!(estimate.live, 100);
        assert_eq!(estimate.withdrawn, 1);
        assert_eq!(estimate.settled, Duration::from_secs(150));
        assert_eq!(estimate.mutants, Duration::from_secs(175));
        assert_eq!(estimate.stalling, Duration::from_secs(225));
        assert_eq!(estimate.worst, load.budget.saturating_mul(1 + crate::exec::CONFIRM_FACTOR) / 4);
    }

    #[test]
    fn zero_jobs_does_not_divide_by_zero() {
        let estimate = project(&population(), work(1000), Duration::ZERO, Duration::from_secs(50), 0, true);

        assert!(estimate.mutants > Duration::ZERO);
    }

    #[test]
    fn only_the_binaries_a_mutant_reaches_are_charged_for_it() {
        // The caller sums the reachable suites; a mutant that can only be seen by a tenth of the
        // workspace must cost a tenth of what one visible to all of it costs.
        let narrow = project(&population(), suite_only(100), Duration::ZERO, Duration::ZERO, 1, true);
        let wide = project(&population(), suite_only(1000), Duration::ZERO, Duration::ZERO, 1, true);

        assert_eq!(narrow.mutants * 10, wide.mutants);
    }

    #[test]
    fn the_error_bar_brackets_the_estimate() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4, true);

        assert!(estimate.low() < estimate.high());
        assert!(estimate.build + estimate.baseline <= estimate.low());
    }

    #[test]
    fn the_error_bar_never_widens_the_part_that_was_measured() {
        // The build really happened; the projection has no business being uncertain about it.
        let estimate = project(&[], Workload::default(), Duration::ZERO, Duration::from_secs(30), 4, true);

        assert_eq!(estimate.low(), Duration::from_secs(30));
        assert_eq!(estimate.high(), Duration::from_secs(30));
    }

    #[test]
    fn the_worst_case_exceeds_the_estimate() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4, true);

        assert!(estimate.worst_case() > estimate.high());
    }

    #[test]
    fn the_worst_case_pays_for_confirming_every_timeout() {
        // A mutant that runs out its budget is made to prove it, so a ceiling that counts one
        // timeout apiece is one a real run can walk straight past.
        let load = work(1000);
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 1, true);

        assert_eq!(estimate.worst_case(), load.budget.saturating_mul(1 + crate::exec::CONFIRM_FACTOR));
    }

    #[test]
    fn disabling_confirmation_removes_its_cost_from_the_projection() {
        let load = work(1000);
        let confirmed = project(&population(), load, Duration::ZERO, Duration::ZERO, 1, true);
        let unconfirmed = project(&population(), load, Duration::ZERO, Duration::ZERO, 1, false);

        assert_eq!(unconfirmed.worst_case(), load.budget);
        assert!(unconfirmed.mutants < confirmed.mutants);
        assert!(unconfirmed.stalling < confirmed.stalling);
    }

    #[test]
    fn the_projected_range_never_reaches_past_the_ceiling() {
        // Above the worst case there is no time left to spend: every mutant has already been given
        // every second it will ever get.
        let load = Workload {
            suite: Duration::from_secs(1000),
            ..work(1)
        };
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 1, true);

        assert_eq!(estimate.high(), estimate.worst_case());
    }

    #[test]
    fn the_rendering_is_one_line_carrying_the_range_the_population_and_the_worst_case() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4, true);
        let rendered = render(&estimate);

        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.contains("100 mutants"), "{rendered}");
        assert!(rendered.contains("4 jobs"), "{rendered}");
        assert!(rendered.contains("worst case"), "{rendered}");
    }

    /// Each end of the range is labelled with the assumption that produces it, because a reader
    /// shown a bare interval learns only that the tool is unsure, where a reader told that the
    /// width is hanging mutants can go and do something about them.
    #[test]
    fn the_rendering_says_what_widens_the_range() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4, true);
        let rendered = render(&estimate);

        assert!(rendered.contains("if none hang"), "{rendered}");
        assert!(rendered.contains("if 15% do"), "{rendered}");
    }

    /// The ceiling counts test time and nothing else. Saying so is the whole fix: a figure that
    /// omits the per-mutant launch, scheduling and reporting cost is routinely walked past by a
    /// real run at the timeout floor, and a reader planning a CI budget against a bare "worst
    /// case" has no way to know that.
    #[test]
    fn the_rendering_says_what_the_ceiling_leaves_out() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4, true);
        let rendered = render(&estimate);

        assert!(rendered.contains("worst case for test time"), "{rendered}");
        assert!(rendered.contains("before per-mutant overhead"), "{rendered}");
    }

    /// The whole point of the rework: a population that hangs must be projected as costing more
    /// than the same population that does not. The old model had no term for it at all, and was
    /// measured two orders of magnitude optimistic on a suite whose mutants hung.
    #[test]
    fn hanging_mutants_cost_more_than_mutants_that_are_judged() {
        let load = Workload {
            suite: Duration::from_secs(10),
            budget: Duration::from_secs(4000),
            single: Duration::from_secs(2000),
        };
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 1, true);

        assert!(estimate.stalling > estimate.settled.saturating_mul(100), "{estimate:?}");
        assert!(estimate.mutants > estimate.settled, "{estimate:?}");
    }

    /// A timeout budget is spent in the one binary that hangs, not in every binary that could have
    /// reached the mutant, so a workload measuring only the total across binaries would price a
    /// hang at several times what one costs.
    #[test]
    fn a_hang_is_charged_for_one_binary_rather_than_all_of_them() {
        let all = Workload {
            suite: Duration::ZERO,
            budget: Duration::from_secs(400),
            single: Duration::from_secs(400),
        };
        let one = Workload {
            single: Duration::from_secs(100),
            ..all
        };

        let costly = project(&population(), all, Duration::ZERO, Duration::ZERO, 1, true);
        let cheaper = project(&population(), one, Duration::ZERO, Duration::ZERO, 1, true);

        assert_eq!(cheaper.stalling * 4, costly.stalling);
    }

    fn live_population(costs: impl IntoIterator<Item = u64>) -> Vec<MutationWork> {
        costs
            .into_iter()
            .enumerate()
            .map(|(index, seconds)| {
                MutationWork::new(
                    u32::try_from(index + 1).expect("test populations fit in u32"),
                    WorkKind::Whole,
                    Duration::from_secs(seconds),
                    Duration::from_secs(seconds.saturating_mul(10)),
                    crate::exec::CONFIRM_FACTOR,
                )
            })
            .collect()
    }

    fn finish_killed(estimate: &mut LiveEstimate, ordinals: impl IntoIterator<Item = u32>, elapsed: Duration) {
        for ordinal in ordinals {
            estimate.finish(ordinal, Outcome::Killed, elapsed);
        }
    }

    #[test]
    fn large_live_populations_index_late_ordinals_directly() {
        let work = live_population((0..100_000).map(|_| 1));
        let mut estimate = LiveEstimate::new(&work, 17);

        assert_eq!(estimate.by_ordinal.get(&100_000), Some(&99_999));

        estimate.start(100_000, Duration::from_secs(3));
        assert!(matches!(estimate.work[99_999].state, WorkState::Active(_)));

        estimate.finish(100_000, Outcome::Killed, Duration::from_secs(1));
        assert!(matches!(estimate.work[99_999].state, WorkState::Done));
        assert_eq!(estimate.done, 1);
    }

    #[test]
    fn cheap_early_completions_do_not_pretend_expensive_work_is_represented() {
        let work = live_population([1; 8].into_iter().chain([100; 32]));
        let mut estimate = LiveEstimate::new(&work, 4);

        finish_killed(&mut estimate, 1..=8, Duration::from_millis(600));

        assert!(
            estimate.remaining(Duration::from_secs(2)).is_none(),
            "eight cheap completions are not representative of the expensive queue"
        );

        finish_killed(&mut estimate, 9..=32, Duration::from_mins(1));
        assert!(estimate.remaining(Duration::from_secs(20)).is_some());
    }

    #[test]
    fn worker_lanes_reduce_the_projected_tail() {
        let work = live_population([10; 16]);
        let mut serial = LiveEstimate::new(&work, 1);
        let mut parallel = LiveEstimate::new(&work, 4);

        finish_killed(&mut serial, 1..=8, Duration::from_secs(6));
        finish_killed(&mut parallel, 1..=8, Duration::from_secs(6));

        let serial = serial.remaining(Duration::from_secs(12)).expect("representative sample");
        let parallel = parallel.remaining(Duration::from_secs(12)).expect("representative sample");

        assert!(parallel.low < serial.low, "{parallel:?} versus {serial:?}");
        assert!(parallel.high < serial.high, "{parallel:?} versus {serial:?}");
    }

    #[test]
    fn time_already_spent_in_flight_is_not_counted_again() {
        let work = live_population([10; 9]);
        let mut estimate = LiveEstimate::new(&work, 2);

        finish_killed(&mut estimate, 1..=8, Duration::from_secs(6));
        estimate.start(9, Duration::ZERO);

        let before = estimate.remaining(Duration::ZERO).expect("representative sample");
        let after = estimate.remaining(Duration::from_secs(3)).expect("representative sample");

        assert!(after.low < before.low, "{after:?} versus {before:?}");
        assert!(after.high < before.high, "{after:?} versus {before:?}");
    }

    #[test]
    fn live_calibration_distinguishes_fast_and_slow_campaigns() {
        let work = live_population([10; 40]);
        let mut fast = LiveEstimate::new(&work, 4);
        let mut slow = LiveEstimate::new(&work, 4);

        finish_killed(&mut fast, 1..=8, Duration::from_secs(1));
        finish_killed(&mut slow, 1..=8, Duration::from_secs(18));

        let fast = fast.remaining(Duration::from_secs(20)).expect("representative sample");
        let slow = slow.remaining(Duration::from_secs(20)).expect("representative sample");

        assert!(fast.low < slow.low, "{fast:?} versus {slow:?}");
        assert!(fast.high < slow.high, "{fast:?} versus {slow:?}");
    }

    #[test]
    fn survivor_and_timeout_heavy_samples_price_the_remaining_queue_higher() {
        let work = live_population([10; 40]);
        let mut killed = LiveEstimate::new(&work, 4);
        let mut survived = LiveEstimate::new(&work, 4);
        let mut timed_out = LiveEstimate::new(&work, 4);

        finish_killed(&mut killed, 1..=8, Duration::from_secs(6));
        for ordinal in 1..=8 {
            survived.finish(ordinal, Outcome::Survived, Duration::from_secs(10));
            timed_out.finish(ordinal, Outcome::Timeout, Duration::from_secs(410));
        }

        let killed = killed.remaining(Duration::from_secs(20)).expect("representative sample");
        let survived = survived.remaining(Duration::from_secs(20)).expect("representative sample");
        let timed_out = timed_out.remaining(Duration::from_secs(20)).expect("representative sample");

        assert!(survived.low > killed.low, "{survived:?} versus {killed:?}");
        assert!(timed_out.low > survived.low, "{timed_out:?} versus {survived:?}");
        assert!(timed_out.high > survived.high, "{timed_out:?} versus {survived:?}");
    }

    #[test]
    fn recent_results_move_the_projection_when_campaign_cost_drifts() {
        let work = live_population([10; 64]);
        let mut estimate = LiveEstimate::new(&work, 4);

        finish_killed(&mut estimate, 1..=8, Duration::from_secs(18));
        let slow = estimate.remaining(Duration::from_secs(20)).expect("representative sample");

        finish_killed(&mut estimate, 9..=16, Duration::from_secs(1));
        let recovered = estimate.remaining(Duration::from_secs(30)).expect("representative sample");

        let slow_per_mutant = slow.high.as_secs_f64() / 56.0;
        let recovered_per_mutant = recovered.high.as_secs_f64() / 48.0;
        assert!(recovered_per_mutant < slow_per_mutant, "{recovered:?} versus {slow:?}");
    }

    #[test]
    fn observed_outcomes_narrow_the_prior_range() {
        let work = live_population([10; 80]);
        let mut estimate = LiveEstimate::new(&work, 4);

        finish_killed(&mut estimate, 1..=8, Duration::from_secs(6));
        let early = estimate.remaining(Duration::from_secs(10)).expect("representative sample");
        let early_ratio = early.high.as_secs_f64() / early.low.as_secs_f64();

        finish_killed(&mut estimate, 9..=32, Duration::from_secs(6));
        let mature = estimate.remaining(Duration::from_secs(40)).expect("representative sample");
        let mature_ratio = mature.high.as_secs_f64() / mature.low.as_secs_f64();

        assert!(mature_ratio < early_ratio, "{mature:?} versus {early:?}");
    }

    #[test]
    fn outcome_and_execution_shape_calibrations_do_not_contaminate_each_other() {
        let mut work = live_population([10; 24]);
        for item in work.iter_mut().take(8) {
            item.kind = WorkKind::Exact;
        }
        let mut estimate = LiveEstimate::new(&work, 4);

        finish_killed(&mut estimate, 1..=8, Duration::from_millis(100));

        let exact = estimate.calibrated(work[0], Confidence::Middle);
        let whole = estimate.calibrated(work[8], Confidence::Middle);

        assert!(exact < whole, "exact={exact:?}, whole={whole:?}");

        estimate.finish(9, Outcome::Timeout, Duration::from_secs(400));
        let resource = estimate.calibration[WorkKind::Whole.index()][OutcomeKind::Resource.index()];
        let killed = estimate.calibration[WorkKind::Whole.index()][OutcomeKind::Killed.index()];

        assert_eq!(resource.samples, 1);
        assert_eq!(killed.samples, 0);
    }

    #[test]
    fn an_overdue_in_flight_mutant_retains_a_resource_tail_in_the_high_estimate() {
        let work = live_population([1; 16]);
        let mut estimate = LiveEstimate::new(&work, 2);

        finish_killed(&mut estimate, 1..=8, Duration::from_millis(600));
        estimate.start(9, Duration::ZERO);

        let range = estimate.remaining(Duration::from_secs(3)).expect("representative sample");

        assert!(range.high > range.low, "{range:?}");
    }

    #[test]
    fn confirmation_policy_is_part_of_each_resource_prediction() {
        let without_confirmation = MutationWork::new(1, WorkKind::Whole, Duration::from_secs(10), Duration::from_secs(100), 0);
        let with_confirmation = MutationWork::new(
            1,
            WorkKind::Whole,
            Duration::from_secs(10),
            Duration::from_secs(100),
            crate::exec::CONFIRM_FACTOR,
        );

        assert_eq!(without_confirmation.stalled, Duration::from_secs(110));
        assert_eq!(with_confirmation.stalled, Duration::from_secs(410));
    }

    #[test]
    fn zero_work_and_duplicate_completion_events_remain_well_defined() {
        let work = live_population([0; 16]);
        let mut estimate = LiveEstimate::new(&work, 4);

        finish_killed(&mut estimate, 1..=8, Duration::ZERO);
        estimate.finish(1, Outcome::Timeout, Duration::from_secs(100));
        estimate.finish(u32::MAX, Outcome::Survived, Duration::from_secs(100));

        assert_eq!(estimate.done, 8);
        assert_eq!(
            estimate.remaining(Duration::from_secs(1)),
            Some(Remaining {
                low: Duration::ZERO,
                high: Duration::ZERO,
            })
        );
    }

    #[test]
    fn scaling_an_extreme_duration_saturates_instead_of_panicking() {
        assert_eq!(scale_duration(Duration::MAX, 10_000.0), Duration::MAX);
    }
}
