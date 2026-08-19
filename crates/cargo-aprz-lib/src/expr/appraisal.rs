// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::{ExpressionOutcome, Risk};

#[derive(Debug, Clone)]
enum AppraisalState {
    Scored {
        risk: Risk,
        available_points: u32,
        awarded_points: u32,
        score: f64,
    },
    RequiredCheckFailure,
    WeightedEvaluationFailure,
}

/// The outcome of evaluating a crate against policy expressions.
#[derive(Debug, Clone)]
pub struct Appraisal {
    #[cfg(test)]
    #[cfg(not(miri))]
    pub(crate) risk: Risk,
    pub(crate) expression_outcomes: Vec<ExpressionOutcome>,
    #[cfg(test)]
    #[cfg(not(miri))]
    pub(crate) available_points: u32,
    #[cfg(test)]
    #[cfg(not(miri))]
    pub(crate) awarded_points: u32,
    #[cfg(test)]
    #[cfg(not(miri))]
    pub(crate) score: f64,
    state: AppraisalState,
}

impl Appraisal {
    #[must_use]
    pub(crate) const fn new(
        risk: Risk,
        expression_outcomes: Vec<ExpressionOutcome>,
        available_points: u32,
        awarded_points: u32,
        score: f64,
    ) -> Self {
        Self {
            #[cfg(test)]
            #[cfg(not(miri))]
            risk,
            expression_outcomes,
            #[cfg(test)]
            #[cfg(not(miri))]
            available_points,
            #[cfg(test)]
            #[cfg(not(miri))]
            awarded_points,
            #[cfg(test)]
            #[cfg(not(miri))]
            score,
            state: AppraisalState::Scored {
                risk,
                available_points,
                awarded_points,
                score,
            },
        }
    }

    #[must_use]
    pub(crate) fn required_check_failure(expression_outcomes: Vec<ExpressionOutcome>) -> Self {
        debug_assert!(
            expression_outcomes
                .iter()
                .any(|outcome| !matches!(outcome.disposition, super::ExpressionDisposition::True)),
            "required-check appraisals must contain a failed or inconclusive outcome"
        );
        Self {
            #[cfg(test)]
            #[cfg(not(miri))]
            risk: Risk::High,
            expression_outcomes,
            #[cfg(test)]
            #[cfg(not(miri))]
            available_points: 0,
            #[cfg(test)]
            #[cfg(not(miri))]
            awarded_points: 0,
            #[cfg(test)]
            #[cfg(not(miri))]
            score: 0.0,
            state: AppraisalState::RequiredCheckFailure,
        }
    }

    #[must_use]
    pub(crate) fn weighted_evaluation_failure(expression_outcomes: Vec<ExpressionOutcome>) -> Self {
        debug_assert!(
            expression_outcomes
                .iter()
                .any(|outcome| { matches!(outcome.disposition, super::ExpressionDisposition::Failed(_)) }),
            "weighted-evaluation failures must contain an inconclusive outcome"
        );
        Self {
            #[cfg(test)]
            #[cfg(not(miri))]
            risk: Risk::High,
            expression_outcomes,
            #[cfg(test)]
            #[cfg(not(miri))]
            available_points: 0,
            #[cfg(test)]
            #[cfg(not(miri))]
            awarded_points: 0,
            #[cfg(test)]
            #[cfg(not(miri))]
            score: 0.0,
            state: AppraisalState::WeightedEvaluationFailure,
        }
    }

    #[must_use]
    pub(crate) const fn risk(&self) -> Risk {
        match self.state {
            AppraisalState::Scored { risk, .. } => risk,
            AppraisalState::RequiredCheckFailure | AppraisalState::WeightedEvaluationFailure => Risk::High,
        }
    }

    #[must_use]
    pub(crate) const fn is_required_check_failure(&self) -> bool {
        matches!(self.state, AppraisalState::RequiredCheckFailure)
    }

    #[must_use]
    pub(crate) const fn is_weighted_evaluation_failure(&self) -> bool {
        matches!(self.state, AppraisalState::WeightedEvaluationFailure)
    }

    /// Returns the weighted score, or `None` when evaluation did not produce one.
    #[must_use]
    pub(crate) const fn weighted_score(&self) -> Option<f64> {
        match self.state {
            AppraisalState::Scored { score, .. } => Some(score),
            AppraisalState::RequiredCheckFailure | AppraisalState::WeightedEvaluationFailure => None,
        }
    }

    #[must_use]
    pub(crate) const fn point_totals(&self) -> Option<(u32, u32)> {
        match self.state {
            AppraisalState::Scored {
                available_points,
                awarded_points,
                ..
            } => Some((awarded_points, available_points)),
            AppraisalState::RequiredCheckFailure | AppraisalState::WeightedEvaluationFailure => None,
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::expr::ExpressionDisposition;

    #[test]
    fn test_required_check_failure_records_explicit_state() {
        let appraisal = Appraisal::required_check_failure(vec![ExpressionOutcome::new(
            "Required".into(),
            "Required policy".into(),
            ExpressionDisposition::False,
        )]);

        assert!(appraisal.is_required_check_failure());
        assert_eq!(appraisal.weighted_score(), None);
        assert_eq!(appraisal.point_totals(), None);
        assert_eq!(appraisal.risk(), Risk::High);
    }

    #[test]
    fn test_zero_point_appraisal_is_not_a_required_check_failure() {
        let appraisal = Appraisal::new(
            Risk::High,
            vec![ExpressionOutcome::new(
                "Weighted".into(),
                "Weighted policy".into(),
                ExpressionDisposition::False,
            )],
            0,
            0,
            0.0,
        );

        assert!(!appraisal.is_required_check_failure());
        assert_eq!(appraisal.weighted_score(), Some(0.0));
        assert_eq!(appraisal.point_totals(), Some((0, 0)));
    }

    #[test]
    fn test_weighted_evaluation_failure_records_distinct_skipped_score_state() {
        let appraisal = Appraisal::weighted_evaluation_failure(vec![ExpressionOutcome::new(
            "Weighted".into(),
            "Weighted policy".into(),
            ExpressionDisposition::Failed("unavailable".into()),
        )]);

        assert!(!appraisal.is_required_check_failure());
        assert!(appraisal.is_weighted_evaluation_failure());
        assert_eq!(appraisal.weighted_score(), None);
        assert_eq!(appraisal.point_totals(), None);
        assert_eq!(appraisal.risk(), Risk::High);
    }
}
