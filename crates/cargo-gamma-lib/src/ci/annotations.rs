// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! How much CI surfacing to emit.

use camino::Utf8Path;
use clap::ValueEnum;

use super::finding::{describe, findings, relative};
use crate::model::Mutant;
use crate::report::encode_controls;

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
/// Two layers, because they defend different things. Every control character is first shown rather
/// than obeyed: the log this lands in is a terminal-rendered one, so a path or source fragment
/// carrying the Escape (ESC) sequence `ESC [2K` erases the lines above it and one carrying an
/// Operating System Command (OSC) 8 sequence hangs a hyperlink of its author's choosing on text a
/// reader takes for workflow output. What is left is then escaped for the workflow command syntax
/// itself, where an unescaped `%`, carriage return, or newline ends the command and turns the
/// remainder into log noise. The second layer no longer has a return or a newline to find, and is
/// kept because it is what the syntax requires rather than a consequence of the first.
fn escape_data(text: &str) -> String {
    encode_controls(text).replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
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
        let escaped = escape_data("a\r\nb");

        assert_eq!(escaped, "a\\r\\nb");
        assert!(!escaped.contains('\r') && !escaped.contains('\n'));
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
            lines[0].starts_with("::warning file=src/a%2Cb%3Ac\\r\\n.rs,line=12,col=5,"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn a_percent_is_escaped_before_anything_else() {
        // Escaping it last would double-escape the escapes.
        assert_eq!(escape_data("%0A"), "%250A");
    }

    /// A CI log is rendered by a terminal, so a path that erases lines does it there too.
    #[test]
    fn a_path_cannot_address_the_terminal_the_log_is_read_in() {
        let hostile = "/w/src/\r\u{1b}[2K\u{9b}31mforged\u{1b}]8;;https://evil.test\u{7}link.rs";
        let mutants = vec![mutant(hostile, 12, "relational.gt_to_ge", Outcome::Survived)];
        let lines = annotations(&mutants, &root());

        for line in &lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(!line.contains('\u{9b}'), "{line:?}");
            assert!(!line.contains('\u{7}'), "{line:?}");
            assert!(!line.contains('\r'), "{line:?}");
            assert!(!line.contains('\n'), "{line:?}");
        }

        assert!(lines[0].contains("\\r\\e[2K"), "{}", lines[0]);
    }

    #[test]
    fn auto_follows_the_runner() {
        assert!(wanted(Annotations::Auto, true));
        assert!(!wanted(Annotations::Auto, false));
        assert!(wanted(Annotations::Github, false));
        assert!(!wanted(Annotations::None, true));
    }
}
