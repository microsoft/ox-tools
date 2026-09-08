// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checking an edit against what it was meant to suppress.

use std::collections::BTreeSet;

use super::Verification;
use crate::model::Mutant;

/// Compares the suppressed sets before and after an edit.
///
/// `intended` is the set of mutant IDs the edit was written for. Every direction matters: an edit
/// that suppresses nothing is a silent no-op, an edit that suppresses too much is the hazard, and
/// an edit that *stops* suppressing something has undone a decision nobody asked it to revisit.
///
/// The last of those is what removing a directive has to be checked against, and it is checked for
/// both operations because neither is ever meant to do it.
#[must_use]
pub fn verify(before: &[Mutant], after: &[Mutant], intended: &BTreeSet<String>) -> Verification {
    let suppressed = |mutants: &[Mutant]| -> BTreeSet<String> {
        mutants
            .iter()
            .filter(|mutant| mutant.suppression.is_some())
            .map(|mutant| mutant.id.to_string())
            .collect()
    };

    let was = suppressed(before);
    let now = suppressed(after);

    Verification {
        missing: intended.iter().filter(|id| !now.contains(*id)).cloned().collect(),
        collateral: now
            .iter()
            .filter(|id| !was.contains(*id) && !intended.contains(*id))
            .cloned()
            .collect(),
        released: was.iter().filter(|id| !now.contains(*id)).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use core::iter::once;

    use super::*;
    use crate::fixtures::mutant_at as mutant;
    use crate::model::{Channel, Outcome, Suppression};

    /// A directive that no longer suppresses what it did is the whole hazard of removal, and the
    /// verification has to be able to say so.
    #[test]
    fn a_mutant_that_stops_being_suppressed_is_reported_as_released() {
        let mut before = vec![mutant("a", "src/lib.rs", 1, "arith.add_to_sub", Outcome::Killed)];

        before[0].suppression = Some(Suppression {
            channel: Channel::Comment,
            reason: None,
            tag: None,
            line: Some(1),
        });

        let after = vec![mutant("a", "src/lib.rs", 1, "arith.add_to_sub", Outcome::Killed)];
        let result = verify(&before, &after, &BTreeSet::new());

        assert_eq!(result.released, vec!["a".to_owned()]);
        assert!(!result.is_clean());
    }

    #[test]
    fn verification_notices_an_edit_that_suppressed_nothing() {
        let before = vec![mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout)];
        let after = before.clone();
        let intended: BTreeSet<String> = once("aaa".to_owned()).collect();

        let result = verify(&before, &after, &intended);

        assert!(!result.is_clean());
        assert_eq!(result.missing, vec!["aaa".to_owned()]);
    }

    #[test]
    fn verification_notices_collateral_suppression() {
        // The hazard the whole design is arranged around: a directive on a multi-line construct
        // takes out everything inside it, which can include survivors.
        let before = vec![
            mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
            mutant("bbb", "src/lib.rs", 10, "arith.add_to_sub", Outcome::Survived),
        ];
        let mut after = before.clone();

        for entry in &mut after {
            entry.suppression = Some(Suppression {
                channel: Channel::Comment,
                reason: None,
                tag: None,
                line: Some(8),
            });
        }

        let intended: BTreeSet<String> = once("aaa".to_owned()).collect();
        let result = verify(&before, &after, &intended);

        assert!(!result.is_clean());
        assert_eq!(result.collateral, vec!["bbb".to_owned()]);
    }

    #[test]
    fn a_clean_verification_is_both_halves() {
        let before = vec![mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout)];
        let mut after = before.clone();

        after[0].suppression = Some(Suppression {
            channel: Channel::Comment,
            reason: None,
            tag: None,
            line: Some(8),
        });

        let intended: BTreeSet<String> = once("aaa".to_owned()).collect();

        assert!(verify(&before, &after, &intended).is_clean());
    }
}
