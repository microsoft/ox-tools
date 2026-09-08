// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::sync::Mutex;
use std::time::Instant;

use super::progress::Progress;

/// How long a binary may go silent before it is presumed hung.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Stall {
    /// The budget, or `None` to wait out the full timeout.
    pub(super) budget: Option<Duration>,
}

impl Stall {
    /// No stall detection: every mutant waits out its whole budget.
    pub(super) const NONE: Self = Self { budget: None };

    /// Builds a budget from the longest silence the baseline legitimately produced.
    ///
    /// Calibrating from the measured quiet period is the point: a suite whose slowest test takes
    /// half a minute goes quiet that long when healthy, and a fixed budget would either call that
    /// a hang or be too loose to help a suite of millisecond tests.
    #[must_use]
    pub(super) fn calibrated(quiet: Duration, factor: f64, floor: Duration) -> Self {
        let budget = quiet.mul_f64(factor).max(floor);

        Self { budget: Some(budget) }
    }

    /// Whether the binary has been silent for longer than the budget allows.
    pub(super) fn exceeded(self, progress: &Mutex<Progress>) -> bool {
        self.exceeded_at(progress, Instant::now())
    }

    /// The same question, asked as of `now`.
    ///
    /// Splitting the clock out of the decision is what makes the boundary testable. Asserting it
    /// through the real clock means sleeping either side of the budget, which is slow and, on a
    /// loaded machine, wrong: a thread that oversleeps its 5 ms by 30 turns "under the budget" into
    /// a stall and produces a failure nobody can reproduce. The boundary itself has nothing to do
    /// with the passage of time — it is one comparison — so the time is passed in.
    fn exceeded_at(self, progress: &Mutex<Progress>, now: Instant) -> bool {
        let Some(budget) = self.budget else {
            return false;
        };

        #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
        let progress = progress.lock().unwrap();

        now.saturating_duration_since(progress.heard) > budget
    }

    /// How long the binary may still stay silent before the budget is exceeded.
    ///
    /// A stall is the one thing the waiting thread cannot be woken for, because it *is* the absence
    /// of anything to wake on — so it has to be waited out. This says how long that wait may be:
    /// sleeping longer would let a hung binary go unnoticed past the budget the run promised.
    ///
    /// `Duration::MAX` when there is no budget, which leaves the caller's other bounds in charge.
    pub(super) fn slack(self, progress: &Mutex<Progress>) -> Duration {
        self.slack_at(progress, Instant::now())
    }

    /// The same figure, as of `now`; see [`Self::exceeded_at`] for why the clock is a parameter.
    fn slack_at(self, progress: &Mutex<Progress>, now: Instant) -> Duration {
        let Some(budget) = self.budget else {
            return Duration::MAX;
        };

        #[expect(clippy::unwrap_used, reason = "the reader only panics if the whole process is unwinding")]
        let heard = progress.lock().unwrap().heard;

        // Saturating to zero rather than going negative: the budget is already spent, and the
        // caller's next pass through the loop is what notices.
        budget.saturating_sub(now.saturating_duration_since(heard))
    }

    /// The same budget, multiplied.
    ///
    /// Used to re-ask a question rather than to ask a new one: a suspected stall is retried under a
    /// budget this much looser, which scheduling noise cannot survive but a real hang still will.
    pub(super) fn scaled(self, factor: u32) -> Self {
        Self {
            budget: self.budget.map(|budget| budget.saturating_mul(factor)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::progress::Watch;

    #[test]
    fn a_stall_budget_scales_with_the_silence_the_baseline_produced() {
        let quiet = Duration::from_secs(30);
        let stall = Stall::calibrated(quiet, 2.0, Duration::from_secs(1));

        assert_eq!(stall.budget, Some(Duration::from_mins(1)));
    }

    #[test]
    fn a_suite_that_never_goes_quiet_still_gets_a_usable_budget() {
        // Otherwise a suite of millisecond tests would produce a budget of nothing, and scheduler
        // noise on a loaded machine would read as a hang.
        let stall = Stall::calibrated(Duration::ZERO, 10.0, Duration::from_secs(5));

        assert_eq!(stall.budget, Some(Duration::from_secs(5)));
    }

    /// A silence of exactly `elapsed`, without any of it having actually passed.
    fn silent_for(elapsed: Duration) -> Mutex<Progress> {
        let mut progress = Progress::new(Watch::Off);

        // The field is the only clock input `exceeded` has, so backdating it is the whole fake.
        progress.heard = Instant::now()
            .checked_sub(elapsed)
            .expect("the process has not been running since the epoch");

        Mutex::new(progress)
    }

    #[test]
    fn without_a_budget_nothing_is_ever_declared_stalled() {
        // No elapsed silence is needed: without a budget there is no comparison to make.
        assert!(!Stall::NONE.exceeded(&silent_for(Duration::ZERO)));
        assert_eq!(Stall::NONE.slack(&silent_for(Duration::ZERO)), Duration::MAX);
    }

    /// The stall boundary is asserted at it and either side of it, and no test sleeps.
    ///
    /// Both parts matter. Sleeping to cross a millisecond threshold is slow and unreliable — a
    /// thread that oversleeps turns the under-budget case into a stall — so the sub-budget case in
    /// particular could never be asserted honestly against the real clock, and the boundary itself
    /// was simply never pinned. The comparison is `>`, so equality is *not* a stall: a binary that
    /// has been silent for exactly its budget has used all of it and none more.
    #[test]
    fn the_stall_boundary_is_exclusive_and_is_pinned_on_both_sides_of_itself() {
        let budget = Duration::from_secs(30);
        let stall = Stall { budget: Some(budget) };
        let now = Instant::now();

        for (elapsed, stalled) in [
            (Duration::ZERO, false),
            (budget.saturating_sub(Duration::from_nanos(1)), false),
            (budget, false),
            (budget + Duration::from_nanos(1), true),
            (budget * 2, true),
        ] {
            let progress = silent_for(Duration::ZERO);

            progress.lock().expect("a test holds the only reference").heard =
                now.checked_sub(elapsed).expect("the process has not been running since the epoch");

            assert_eq!(
                stall.exceeded_at(&progress, now),
                stalled,
                "{elapsed:?} of silence against a {budget:?} budget"
            );
        }
    }

    /// The slack is what is left of the budget, and it stops at zero rather than going backwards.
    ///
    /// This is what bounds the wait loop's sleep, so a figure that ran past zero would be a wait
    /// that overshot the budget the run promised, and one that did not fall as time passed would
    /// be a hung binary noticed late.
    #[test]
    fn the_slack_falls_with_the_silence_and_stops_at_zero() {
        let budget = Duration::from_secs(30);
        let stall = Stall { budget: Some(budget) };
        let now = Instant::now();

        for (elapsed, left) in [
            (Duration::ZERO, budget),
            (Duration::from_secs(10), Duration::from_secs(20)),
            (budget, Duration::ZERO),
            (budget * 2, Duration::ZERO),
        ] {
            let progress = silent_for(Duration::ZERO);

            progress.lock().expect("a test holds the only reference").heard =
                now.checked_sub(elapsed).expect("the process has not been running since the epoch");

            assert_eq!(stall.slack_at(&progress, now), left, "{elapsed:?} of silence");
        }
    }

    #[test]
    fn a_line_of_output_clears_the_silence() {
        let progress = silent_for(Duration::from_millis(51));
        let stall = Stall {
            budget: Some(Duration::from_millis(50)),
        };

        assert!(stall.exceeded(&progress), "an hour of silence is a stall to begin with");

        progress
            .lock()
            .expect("a test holds the only reference")
            .heard("test tests::a ... ok\n");

        assert!(!stall.exceeded(&progress));
    }
}
