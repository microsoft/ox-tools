// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Costing mutation work for scheduling.

use core::time::Duration;

/// Weight retained by inferred-test scheduling while persisted observations accumulate.
const PRIOR_STRENGTH: f64 = 8.0;

/// The share of a whole binary a killed mutant is initially assumed to reach.
///
/// The harness stops at the first failure, so half the measured binary is a neutral scheduling
/// prior.
const KILLED_SHARE: f64 = 0.50;

/// Initial settled-cost share for work expected to begin with one narrow probe.
///
/// One quarter keeps exact and learned probes ahead of whole-suite work without treating an
/// unconfirmed hint as free. Raising it delays hinted work; lowering it makes hints dominate the
/// schedule before enough observations exist to justify that confidence.
const NARROW_PROBE_SHARE: f64 = 0.25;

/// A durable exact hint is checked knowledge and almost always convicts again.
const EXACT_HIT_SHARE: f64 = 0.95;

/// Inferred tests and incomplete-census selections are useful but deliberately less trusted.
const INFERRED_HIT_SHARE: f64 = 0.65;

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

/// Predicted work for one live mutant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MutationWork {
    /// How the tests are expected to be selected.
    pub(crate) kind: WorkKind,

    /// Expected cost when ordinary test execution judges the mutant.
    settled: Duration,
}

impl MutationWork {
    /// Builds scheduling metadata from measured test work.
    #[must_use]
    pub(crate) fn new(kind: WorkKind, suite: Duration) -> Self {
        Self {
            kind,
            settled: match kind {
                WorkKind::Exact | WorkKind::Hinted => scale_duration(suite, NARROW_PROBE_SHARE),
                WorkKind::Selected | WorkKind::Whole => scale_duration(suite, KILLED_SHARE),
                WorkKind::Uncovered => Duration::ZERO,
            },
        }
    }

    /// Builds a probe-first prediction with a probabilistic whole-work fallback.
    #[must_use]
    pub(crate) fn hinted(kind: WorkKind, probe: Duration, fallback: Duration) -> Self {
        let hit_share = match kind {
            WorkKind::Exact => EXACT_HIT_SHARE,
            WorkKind::Hinted => INFERRED_HIT_SHARE,
            _ => 0.0,
        };
        Self::hinted_with_hit_share(kind, probe, fallback, hit_share)
    }

    /// Builds an inferred-test prediction using its persisted transfer evidence.
    #[must_use]
    pub(crate) fn hinted_with_observations(probe: Duration, fallback: Duration, hits: u32, misses: u32) -> Self {
        let samples = hits.saturating_add(misses);
        let denominator = PRIOR_STRENGTH + f64::from(samples);
        let hit_share = PRIOR_STRENGTH.mul_add(INFERRED_HIT_SHARE, f64::from(hits)) / denominator;

        Self::hinted_with_hit_share(WorkKind::Hinted, probe, fallback, hit_share)
    }

    fn hinted_with_hit_share(kind: WorkKind, probe: Duration, fallback: Duration, hit_share: f64) -> Self {
        let miss_share = 1.0 - hit_share;
        let killed_fallback = scale_duration(fallback, KILLED_SHARE);

        Self::costed(kind, probe.saturating_add(scale_duration(killed_fallback, miss_share)))
    }

    /// Builds scheduling metadata with a precomputed expected cost.
    #[must_use]
    pub(crate) fn costed(kind: WorkKind, killed: Duration) -> Self {
        Self { kind, settled: killed }
    }

    pub(crate) const fn scheduling_cost(self) -> Duration {
        self.settled
    }
}

fn scale_duration(duration: Duration, factor: f64) -> Duration {
    Duration::try_from_secs_f64(duration.as_secs_f64() * factor).unwrap_or(Duration::MAX)
}

pub(crate) fn inferred_fallback_cost(fallback: Duration) -> Duration {
    scale_duration(scale_duration(fallback, KILLED_SHARE), 1.0 - INFERRED_HIT_SHARE)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn survivor_observed_hint_cost_matrix_updates_both_hit_and_miss_evidence() {
        for (hits, misses, expected_ms) in [(0, 0, 2_400), (8, 0, 1_700), (0, 8, 3_700), (8, 8, 2_800)] {
            let work = MutationWork::hinted_with_observations(Duration::from_secs(1), Duration::from_secs(8), hits, misses);
            assert_eq!(work.kind, WorkKind::Hinted);
            assert_eq!(work.scheduling_cost().as_millis(), expected_ms, "{hits} hits, {misses} misses");
        }
        assert_eq!(
            MutationWork::hinted(WorkKind::Hinted, Duration::from_secs(1), Duration::from_secs(8)).scheduling_cost(),
            Duration::from_millis(2_400)
        );
    }

    #[test]
    fn whole_binary_scheduling_still_starts_at_half_the_measured_baseline() {
        let work = MutationWork::new(WorkKind::Whole, Duration::from_secs(10));

        assert_eq!(work.scheduling_cost(), Duration::from_secs(5));
    }

    #[test]
    fn exact_hints_and_inferred_tests_have_distinct_scheduling_costs() {
        let exact = MutationWork::hinted(WorkKind::Exact, Duration::from_secs(2), Duration::from_mins(1));
        let inferred = MutationWork::hinted(WorkKind::Hinted, Duration::from_secs(2), Duration::from_mins(1));

        assert!(exact.scheduling_cost() < inferred.scheduling_cost());
    }

    #[test]
    fn every_work_kind_uses_its_documented_initial_cost() {
        let suite = Duration::from_secs(8);

        assert_eq!(MutationWork::new(WorkKind::Exact, suite).scheduling_cost(), Duration::from_secs(2));
        assert_eq!(MutationWork::new(WorkKind::Hinted, suite).scheduling_cost(), Duration::from_secs(2));
        assert_eq!(
            MutationWork::new(WorkKind::Selected, suite).scheduling_cost(),
            Duration::from_secs(4)
        );
        assert_eq!(MutationWork::new(WorkKind::Uncovered, suite).scheduling_cost(), Duration::ZERO);
        assert_eq!(
            MutationWork::hinted(WorkKind::Selected, Duration::from_secs(2), suite).scheduling_cost(),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn scaling_an_extreme_duration_saturates_instead_of_panicking() {
        assert_eq!(scale_duration(Duration::MAX, 10_000.0), Duration::MAX);
    }
}
