// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What counts as a finding, and where it points.

use camino::Utf8Path;

use crate::model::{Mutant, Outcome};

/// The undetected mutants, in report order.
///
/// Each asks for different work, but all lower the score because no assertion killed them.
pub(super) fn findings(mutants: &[Mutant]) -> Vec<&Mutant> {
    mutants
        .iter()
        .filter(|mutant| {
            matches!(
                mutant.outcome,
                Outcome::Survived | Outcome::Timeout | Outcome::OutOfMemory | Outcome::NoCoverage
            )
        })
        .collect()
}

/// A path relative to the workspace root, with forward slashes.
///
/// Every consumer here resolves against the repository checkout, so an absolute path from the
/// machine that ran the job points at nothing. Forward slashes because SARIF and the workflow
/// commands both specify them regardless of the host.
///
/// The separator rewrite is conditional on the host, because on Unix a backslash is an ordinary
/// character in a file name: rewriting it there maps `src/a\b.rs` and `src/a/b.rs` — two distinct
/// files — onto one string, and this result is what the summary keys its rows by and what SARIF and
/// the annotations emit as a location. A finding attributed to a file that did not earn it is
/// exactly what the escaping downstream of here was written to prevent, and no escaping can undo a
/// collision introduced on the key. Nothing is lost by the condition: [`Mutant::file`] already
/// carries forward slashes, so on Unix the rewrite never had anything to do.
pub(super) fn relative(path: &Utf8Path, root: &Utf8Path) -> String {
    let path = path.strip_prefix(root).unwrap_or(path).as_str();

    #[cfg(windows)]
    let relative = path.replace('\\', "/");

    #[cfg(not(windows))]
    let relative = path.to_owned();

    relative
}

/// What a survivor is, in one sentence.
///
/// The location is carried in fields of its own by both consumers, so repeating it here would only
/// take space from the part a reader cannot get anywhere else.
pub(super) fn describe(mutant: &Mutant) -> String {
    match mutant.outcome {
        Outcome::NoCoverage => format!("No test reaches this code: {}.", mutant.summary()),
        Outcome::Timeout => format!("{} and the test run timed out before an assertion rejected it.", mutant.summary()),
        Outcome::OutOfMemory => {
            format!(
                "{} and the test run exceeded its memory limit before an assertion rejected it.",
                mutant.summary()
            )
        }
        _other => format!("{} and no test failed.", mutant.summary()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ci_fixture::{mutant, root};

    #[test]
    fn every_undetected_outcome_is_a_finding() {
        let mutants = vec![
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed),
            mutant("/w/src/a.rs", 2, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 3, "relational.gt_to_ge", Outcome::Timeout),
            mutant("/w/src/a.rs", 4, "relational.gt_to_ge", Outcome::NoCoverage),
            mutant("/w/src/a.rs", 5, "relational.gt_to_ge", Outcome::CompileError),
            mutant("/w/src/a.rs", 6, "relational.gt_to_ge", Outcome::OutOfMemory),
        ];

        let found = findings(&mutants);

        assert_eq!(found.len(), 4);
        assert_eq!(found[0].line, 2);
        assert_eq!(found[1].line, 3);
        assert_eq!(found[2].line, 4);
        assert_eq!(found[3].line, 6);
    }

    #[test]
    fn a_path_outside_the_root_is_left_alone() {
        // Better an absolute path a consumer cannot resolve than a relative one pointing at the
        // wrong file inside the checkout.
        assert_eq!(relative(Utf8Path::new("/elsewhere/a.rs"), &root()), "/elsewhere/a.rs");
    }

    /// Two files whose names differ only by a backslash stay two files.
    ///
    /// On Unix a backslash is an ordinary character in a file name, so rewriting it into a slash
    /// merges `src/a\b.rs` into `src/a/b.rs` — and the result is what the summary keys its rows by
    /// and what SARIF emits as a location, so one file's survivors are printed under the other's
    /// name and an alert lands on whichever of the two exists. The escaping downstream is
    /// one-to-one and cannot see this, because the collision happens on the key.
    #[cfg(not(windows))]
    #[test]
    fn two_paths_differing_only_by_a_backslash_do_not_collide() {
        use crate::ci::{Level, sarif};

        let mutants = vec![
            mutant(r"/w/src/a\b.rs", 1, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a/b.rs", 1, "relational.gt_to_ge", Outcome::Survived),
        ];

        assert_ne!(
            relative(&mutants[0].file, &root()),
            relative(&mutants[1].file, &root()),
            "two files must not share one name"
        );

        let (log, _truncation) = sarif(&mutants, &root(), Level::Note).expect("a sarif log");
        let document: serde_json::Value = serde_json::from_str(&log).expect("valid json");
        let uris: Vec<String> = document["runs"][0]["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|result| result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"].to_string())
            .collect();

        assert_eq!(uris.len(), 2, "{document}");
        assert_ne!(uris[0], uris[1], "{document}");

        // The job summary keys its rows on the same string, so a collision there sums one file's
        // survivors into the other's row rather than merely renaming it.
        let table = crate::ci::summary(&mutants, &root());
        let rows = table.lines().filter(|line| line.contains("b.rs")).count();

        assert_eq!(rows, 2, "{table}");
    }
}
