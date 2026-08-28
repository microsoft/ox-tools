// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a merge concluded: the merged report plus the numbers that make it trustworthy.

use std::collections::BTreeSet;

use crate::elements::Report;

/// The largest rotation a merge will account for.
///
/// A shard count arrives from a report, which is a file a merge did not necessarily write, and the
/// only thing it is used for is counting up to. Nothing anyone runs is within two orders of
/// magnitude of this — a shard is a CI job, and the job matrices that schedule them cap in the low
/// hundreds — so the bound constrains no plausible rotation. It exists because the alternative to
/// bounding it is trusting it: at `u32::MAX` the enumeration below would ask for sixteen gigabytes
/// before anything could report why.
///
/// [`read`](super::read) refuses a report that claims more than this, which is what makes the
/// figure a diagnostic rather than a silent truncation, and is why the cap in
/// [`Merged::missing_shards`] is unreachable through the command.
pub(crate) const MAX_SHARDS: u32 = 65_536;

/// What a merge concluded.
#[derive(Debug, Default)]
pub struct Merged {
    /// The merged report, ready to render.
    pub report: Option<Report>,

    /// Mutants with a verdict inside the freshness window.
    pub fresh: usize,

    /// Mutants whose most recent verdict predates the window.
    pub stale: usize,

    /// Whether freshness could not be classified because this machine's clock was unreadable.
    pub freshness_unavailable: bool,

    /// Mutants seen in a report but never actually run.
    pub never_tested: usize,

    /// Distinct shard indices seen.
    pub shards_seen: BTreeSet<u32>,

    /// The shard count the inputs agreed on, when they agreed.
    pub shard_count: Option<u32>,

    /// Inputs whose shard count disagreed with the others.
    ///
    /// Worth reporting rather than resolving: mixing a run at count 30 with one at count 40 means
    /// the two partitioned the population differently, so "shards seen" no longer means coverage.
    pub inconsistent: Vec<String>,

    /// Mutants detected by a failing test assertion.
    pub detected: usize,

    /// Valid mutants, the denominator of the score.
    pub valid: usize,

    /// Verdicts dropped because the code they were formed against no longer exists.
    ///
    /// A mutant's identity is content-addressed, so editing the code it was generated from gives
    /// the replacement a different id. Without this, the old id stays in the denominator forever:
    /// a survivor that has since been fixed keeps depressing the score, and a caught mutant keeps
    /// crediting code that has changed. Reported rather than silently applied, because a large
    /// number here means the inputs span commits that are further apart than the reader thinks.
    pub withdrawn: usize,

    /// Files whose withdrawals could not be checked because no input supplied a complete population.
    ///
    /// Sharded and previously merged reports can both describe incomplete populations, so their
    /// silence about an id says nothing about whether that code still exists.
    pub unchecked: usize,

    /// Verdicts omitted because their mutation presentation does not fit the selected source.
    pub incompatible: usize,
}

impl Merged {
    /// The merged mutation score, as a percentage.
    ///
    /// Answers 100% for an empty population rather than dividing by zero, which is the only sensible
    /// thing to print — a merge that caught everything it tested did catch everything it tested —
    /// and matches what [`Summary::score`](crate::model::Summary::score) prints for the same case,
    /// so the `run` and `merge` sides never disagree on what nothing scores. The caller is expected
    /// to report the count beside it so a perfect score over nothing is visibly nothing, and to gate
    /// through [`Self::scored`] rather than this value.
    #[must_use]
    pub fn score(&self) -> f64 {
        if self.valid == 0 {
            return 100.0;
        }

        #[expect(clippy::cast_precision_loss, reason = "mutant counts are far below the f64 integer limit")]
        let ratio = self.detected as f64 / self.valid as f64;

        ratio * 100.0
    }

    /// The merged mutation score, or `None` when nothing was scored.
    ///
    /// [`Self::score`] answers 100% for an empty population, which is the right thing to print and a
    /// catastrophic thing to hand a threshold: `--min-score 100` against a merge that scored nothing
    /// is not a gate that passed but a gate that never ran, and the two have to be distinguishable at
    /// the one place the difference decides an exit code. This mirrors
    /// [`Summary::scored`](crate::model::Summary::scored) so the merge gate refuses an empty
    /// population structurally rather than by relying on the printed score being unflattering.
    #[must_use]
    pub fn scored(&self) -> Option<f64> {
        (self.valid > 0).then(|| self.score())
    }

    /// How much of the rotation the inputs covered, as a percentage.
    #[must_use]
    pub fn coverage(&self) -> f64 {
        let Some(count) = self.shard_count.filter(|count| *count > 0) else {
            return 100.0;
        };

        #[expect(clippy::cast_precision_loss, reason = "shard counts are small")]
        let ratio = self.shards_seen.len() as f64 / f64::from(count);

        ratio * 100.0
    }

    /// The shard indices the rotation has not covered.
    ///
    /// At most [`MAX_SHARDS`] of them, and the enumeration stops as soon as it has that many rather
    /// than walking the whole count first. A `Merged` built by the read path can never reach the
    /// cap, because a report claiming a larger rotation is refused where it is read; one built by
    /// hand from a count it did not check would otherwise turn that count into an allocation of the
    /// same size.
    #[must_use]
    pub fn missing_shards(&self) -> Vec<u32> {
        self.shard_count
            .map(|count| {
                (0..count)
                    .filter(|index| !self.shards_seen.contains(index))
                    .take(MAX_SHARDS as usize)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::fixtures::{mutant_result_at as mutant, report_with as report};

    #[test]
    fn rotation_coverage_reports_the_shards_that_were_missed() {
        let merged = super::super::merge(
            &[
                (
                    "a".to_owned(),
                    fixtures::report_with(Some((0, 4)), 100, vec![fixtures::mutant_result_at("aaa", 1, "Killed")]),
                ),
                (
                    "b".to_owned(),
                    fixtures::report_with(Some((2, 4)), 200, vec![fixtures::mutant_result_at("bbb", 2, "Killed")]),
                ),
            ],
            300,
            None,
        );

        assert_eq!(merged.shards_seen.len(), 2);
        assert_eq!(merged.missing_shards(), vec![1, 3]);
        assert!((merged.coverage() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unsharded_reports_merge_and_report_full_coverage() {
        // The parallel-CI case degenerates to this when nobody passed a shard flag.
        let merged = super::super::merge(&[("a".to_owned(), report(None, 100, vec![mutant("aaa", 1, "Killed")]))], 200, None);

        assert!(merged.shards_seen.is_empty());
        assert!((merged.coverage() - 100.0).abs() < f64::EPSILON);
        assert!(merged.missing_shards().is_empty());
    }

    /// A count no read path would have accepted still costs a bounded amount to report on.
    ///
    /// The read path refuses this document, so the only way here is a `Merged` assembled by hand —
    /// and the enumeration has to stay cheap on that path too, because "cheap" is the whole
    /// property: at `u32::MAX` the unbounded version allocates sixteen gigabytes, and a test that
    /// only checked the length would have to survive the allocation first.
    #[test]
    fn an_unchecked_shard_count_is_reported_up_to_the_bound_and_no_further() {
        let merged = Merged {
            shard_count: Some(u32::MAX),
            shards_seen: BTreeSet::from([0, 2]),
            ..Merged::default()
        };

        let missing = merged.missing_shards();

        assert_eq!(missing.len(), MAX_SHARDS as usize);

        // The ones actually seen are still excluded, so the cap truncates the list rather than
        // replacing it with the first N indices.
        assert_eq!(missing[0], 1);
        assert_eq!(missing[1], 3);
    }

    /// The bound is not a limit on how anyone shards: every index of the largest supported rotation
    /// is still enumerated.
    #[test]
    fn a_rotation_at_the_bound_still_reports_every_missing_shard() {
        let merged = Merged {
            shard_count: Some(MAX_SHARDS),
            shards_seen: BTreeSet::from([0]),
            ..Merged::default()
        };

        assert_eq!(merged.missing_shards().len(), MAX_SHARDS as usize - 1);
    }
}
