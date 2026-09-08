// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Marking the mutants a file's directives govern.

use super::{Directive, Intent};
use crate::model::{Expectation, Mutant, Outcome, Suppression};

/// Marks every mutant a `skip` directive governs, and returns how many were marked.
///
/// The mutants keep their identity and stay in the population so that reports can show what was
/// suppressed and why. Silently dropping them would make a directive indistinguishable from a
/// mutator that never fired.
pub fn suppress(mutants: &mut [Mutant], found: &[Directive]) -> usize {
    let mut count = 0;

    for mutant in mutants.iter_mut() {
        let Some(directive) = found
            .iter()
            .filter(|directive| directive.intent == Some(Intent::Skip))
            .find(|directive| directive.governs(mutant))
        else {
            continue;
        };

        mutant.outcome = Outcome::Ignored;
        mutant.suppression = Some(Suppression {
            channel: directive.channel,
            reason: directive.reason.clone(),
            tag: directive.tag.clone(),
            line: Some(directive.line),
        });

        count += 1;
    }

    for mutant in mutants.iter_mut() {
        let expecting = found
            .iter()
            .filter(|directive| matches!(directive.intent, Some(Intent::ExpectKilled | Intent::ExpectSurvived)))
            .find(|directive| directive.governs(mutant));

        if let Some(directive) = expecting {
            mutant.expectation = Some(Expectation {
                killed: directive.intent == Some(Intent::ExpectKilled),
                line: directive.line,
                reason: directive.reason.clone(),
            });
        }
    }

    for mutant in mutants.iter_mut() {
        let timing = found
            .iter()
            .filter(|directive| directive.test_timeout_multiplier.is_some())
            .find(|directive| directive.governs(mutant));

        if let Some(directive) = timing {
            mutant.test_timeout_multiplier = directive.test_timeout_multiplier;
        }
    }

    count
}
