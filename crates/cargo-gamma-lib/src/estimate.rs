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

use crate::advise::human;
use crate::model::{Mutant, Outcome};
use crate::report::quantity;

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
pub fn project(mutants: &[Mutant], work: Workload, baseline: Duration, build: Duration, jobs: usize) -> Estimate {
    let live = mutants
        .iter()
        .filter(|mutant| mutant.ordinal > 0 && mutant.outcome == Outcome::Pending)
        .count();

    let withdrawn = mutants.iter().filter(|mutant| mutant.outcome == Outcome::CompileError).count();
    let lanes = u32::try_from(jobs.max(1)).unwrap_or(1);

    Estimate {
        live,
        withdrawn,
        build,
        baseline,
        mutants: spend(work, lanes, STALL_SHARE),
        settled: spend(work, lanes, 0.0),
        stalling: spend(work, lanes, stall_share_high()),
        jobs,
        worst: work.budget.saturating_mul(1 + crate::exec::CONFIRM_FACTOR) / lanes,
    }
}

/// What testing every live mutant comes to, if the given share of them hang.
///
/// The two outcomes are priced separately because they differ by orders of magnitude rather than by
/// a little. A mutant that is judged pays for the tests it reached. A mutant that hangs pays for
/// everything it got through, then for a whole budget it never finishes, and then for the
/// confirmation run that budget is not believed without — and that budget has a floor under it, so
/// on a quick suite it is not a multiple of the tests but a constant far larger than all of them.
fn spend(work: Workload, lanes: u32, stalling: f64) -> Duration {
    let judged = work.suite.mul_f64(KILLED_SHARE * (1.0 - stalling));
    let hung = (work.suite + work.single.saturating_mul(1 + crate::exec::CONFIRM_FACTOR)).mul_f64(stalling);

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
        let estimate = project(&population(), work(100), Duration::ZERO, Duration::from_secs(5), 1);

        assert_eq!(estimate.live, 100);
        assert_eq!(estimate.withdrawn, 1);
    }

    #[test]
    fn parallelism_divides_the_projection() {
        let one = project(&population(), work(1000), Duration::ZERO, Duration::from_secs(50), 1);
        let eight = project(&population(), work(1000), Duration::ZERO, Duration::from_secs(50), 8);

        assert_eq!(one.mutants / 8, eight.mutants);
        assert_eq!(one.worst / 8, eight.worst);
    }

    #[test]
    fn zero_jobs_does_not_divide_by_zero() {
        let estimate = project(&population(), work(1000), Duration::ZERO, Duration::from_secs(50), 0);

        assert!(estimate.mutants > Duration::ZERO);
    }

    #[test]
    fn only_the_binaries_a_mutant_reaches_are_charged_for_it() {
        // The caller sums the reachable suites; a mutant that can only be seen by a tenth of the
        // workspace must cost a tenth of what one visible to all of it costs.
        let narrow = project(&population(), suite_only(100), Duration::ZERO, Duration::ZERO, 1);
        let wide = project(&population(), suite_only(1000), Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(narrow.mutants * 10, wide.mutants);
    }

    #[test]
    fn the_error_bar_brackets_the_estimate() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4);

        assert!(estimate.low() < estimate.high());
        assert!(estimate.build + estimate.baseline <= estimate.low());
    }

    #[test]
    fn the_error_bar_never_widens_the_part_that_was_measured() {
        // The build really happened; the projection has no business being uncertain about it.
        let estimate = project(&[], Workload::default(), Duration::ZERO, Duration::from_secs(30), 4);

        assert_eq!(estimate.low(), Duration::from_secs(30));
        assert_eq!(estimate.high(), Duration::from_secs(30));
    }

    #[test]
    fn the_worst_case_exceeds_the_estimate() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4);

        assert!(estimate.worst_case() > estimate.high());
    }

    #[test]
    fn the_worst_case_pays_for_confirming_every_timeout() {
        // A mutant that runs out its budget is made to prove it, so a ceiling that counts one
        // timeout apiece is one a real run can walk straight past.
        let load = work(1000);
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(estimate.worst_case(), load.budget.saturating_mul(1 + crate::exec::CONFIRM_FACTOR));
    }

    #[test]
    fn the_projected_range_never_reaches_past_the_ceiling() {
        // Above the worst case there is no time left to spend: every mutant has already been given
        // every second it will ever get.
        let load = Workload {
            suite: Duration::from_secs(1000),
            ..work(1)
        };
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(estimate.high(), estimate.worst_case());
    }

    #[test]
    fn the_rendering_is_one_line_carrying_the_range_the_population_and_the_worst_case() {
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4);
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
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4);
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
        let estimate = project(&population(), work(1000), Duration::from_secs(3), Duration::from_secs(50), 4);
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
        let estimate = project(&population(), load, Duration::ZERO, Duration::ZERO, 1);

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

        let costly = project(&population(), all, Duration::ZERO, Duration::ZERO, 1);
        let cheaper = project(&population(), one, Duration::ZERO, Duration::ZERO, 1);

        assert_eq!(cheaper.stalling * 4, costly.stalling);
    }
}
