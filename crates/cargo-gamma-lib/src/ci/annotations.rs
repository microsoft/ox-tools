// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How much CI surfacing to emit.

use camino::Utf8Path;
use clap::ValueEnum;

use super::finding::{describe, findings, relative};
use crate::model::Mutant;

/// How much of the CI surfacing to emit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Annotations {
    /// Emit nothing.
    None,

    /// Emit the GitHub renderings when running inside GitHub Actions, and nothing otherwise.
    #[default]
    Auto,

    /// Emit the GitHub renderings regardless of where we are running.
    Github,
}

/// The most findings any one annotation run will print.
///
/// GitHub keeps only the first ten annotations of a level per step and silently discards the rest,
/// so printing more produces a log full of commands that had no effect and a reviewer who believes
/// they have seen everything. The report and the SARIF log carry the full population.
const ANNOTATION_LIMIT: usize = 10;

/// Whether the GitHub renderings should be emitted.
///
/// `Auto` keys off `GITHUB_ACTIONS`, which the runner sets on every step. That means a workflow
/// gets annotations by adding nothing to its command line, which is the only adoption path that
/// reliably happens.
#[must_use]
pub const fn wanted(annotations: Annotations, github_actions: bool) -> bool {
    match annotations {
        Annotations::None => false,
        Annotations::Auto => github_actions,
        Annotations::Github => true,
    }
}

/// Renders the GitHub Actions workflow commands that place survivors on the diff.
///
/// The message is the mutation itself rather than a summary of it. A reviewer looking at the line
/// needs to know what was changed and that nothing complained, and any wording that does not
/// contain the replacement makes them go and look it up.
#[must_use]
pub fn annotations(mutants: &[Mutant], root: &Utf8Path) -> Vec<String> {
    let survivors = findings(mutants);
    let mut lines: Vec<String> = survivors
        .iter()
        .take(ANNOTATION_LIMIT)
        .map(|mutant| {
            let file = relative(&mutant.file, root);
            let title = format!("Surviving mutant ({})", mutant.mutator);
            let message = describe(mutant);

            format!(
                "::warning file={},line={},col={},title={}::{}",
                escape_property(&file),
                mutant.line,
                mutant.column,
                escape_property(&title),
                escape_data(&message)
            )
        })
        .collect();

    if survivors.len() > ANNOTATION_LIMIT {
        lines.push(format!(
            "::notice title=Surviving mutants::{} of {} findings annotated, which is all GitHub keeps per step; \
             the rest are in the report",
            ANNOTATION_LIMIT,
            survivors.len()
        ));
    }

    lines
}

/// Escapes a workflow command property value.
fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}

/// Escapes a workflow command message.
///
/// A newline inside a message would end the command and turn the remainder into log noise, so the
/// escaping is not cosmetic.
fn escape_data(text: &str) -> String {
    text.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Outcome;
    use crate::testing::ci_fixture::{mutant, root};

    #[test]
    fn an_annotation_points_at_a_relative_path() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("::warning file=src/a.rs,line=12,col=5,"), "{}", lines[0]);
    }

    #[test]
    fn an_annotation_says_what_the_mutation_was() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        // A reviewer standing on the line has to be told the replacement, or they go and look it up.
        assert!(lines[0].contains("a >= b"), "{}", lines[0]);
    }

    #[test]
    fn an_uncovered_mutant_says_so() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::NoCoverage)];
        let lines = annotations(&mutants, &root());

        assert!(lines[0].contains("No test reaches this code"), "{}", lines[0]);
    }

    #[test]
    fn too_many_annotations_are_capped_and_the_cap_is_announced() {
        let mutants: Vec<Mutant> = (0..ANNOTATION_LIMIT + 5)
            .map(|line| mutant("/w/src/a.rs", line + 1, "relational.gt_to_ge", Outcome::Survived))
            .collect();

        let lines = annotations(&mutants, &root());

        assert_eq!(lines.len(), ANNOTATION_LIMIT + 1);
        assert!(lines.last().expect("a notice").contains("of 15 findings annotated"));
    }

    #[test]
    fn nothing_survived_means_nothing_to_annotate() {
        let mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed)];

        assert!(annotations(&mutants, &root()).is_empty());
    }

    #[test]
    fn a_newline_cannot_escape_a_message() {
        // A raw newline would end the workflow command and turn the rest into log noise. Mutant
        // text is already flattened before it gets here, so this is the belt to that suspenders.
        assert_eq!(escape_data("a\r\nb"), "a%0D%0Ab");
    }

    #[test]
    fn an_annotation_does_not_repeat_the_location_it_already_carries() {
        let mutants = vec![mutant("/w/src/a.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        assert!(!lines[0].contains("src/a.rs:12"), "{}", lines[0]);
    }

    #[test]
    fn a_comma_cannot_escape_a_property() {
        assert_eq!(escape_property("a,b:c"), "a%2Cb%3Ac");
    }

    #[test]
    fn a_path_cannot_escape_its_file_property() {
        let mutants = vec![mutant("/w/src/a,b:c\r\n.rs", 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        assert!(
            lines[0].starts_with("::warning file=src/a%2Cb%3Ac%0D%0A.rs,line=12,col=5,"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_percent_is_escaped_before_anything_else() {
        // Escaping it last would double-escape the escapes.
        assert_eq!(escape_data("%0A\n"), "%250A%0A");
    }

    #[test]
    fn auto_follows_the_runner() {
        assert!(wanted(Annotations::Auto, true));
        assert!(!wanted(Annotations::Auto, false));
        assert!(wanted(Annotations::Github, false));
        assert!(!wanted(Annotations::None, true));
    }
}
