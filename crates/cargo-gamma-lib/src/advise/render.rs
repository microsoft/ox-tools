// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rendering findings and yields as plain text or as Markdown.

use core::fmt::Write as _;

use super::text::{human, sentence, share, slug};
use super::{Finding, Timing, Yield};
use crate::model::{Outcome, Summary};

/// The heading of the section reporting what the run cost and decided.
const RUN_HEADING: &str = "This run";

/// The heading of the section holding the diagnoses.
const FINDINGS_HEADING: &str = "Findings";

/// The heading of the per-family cost and value table.
const YIELD_HEADING: &str = "Yield by mutator family";

/// The heading of the definitions.
const GLOSSARY_HEADING: &str = "What the verdicts mean";

/// Where the rendered Markdown is going to be read.
///
/// The two destinations want genuinely different documents, not the same one at two sizes. A file
/// is opened on purpose by someone who wants the whole picture and needs to navigate it; a job
/// summary panel is scrolled past by someone who did not ask for it, sits under a heading the CI
/// renderer already owns, and has just been told the score and the verdict counts by the panel
/// above it. Repeating that there would be noise, and a level-one title nested under it would be
/// malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// A standalone file: title, table of contents, and what the run cost.
    #[default]
    Document,

    /// A fragment appended to something that already has a heading and a score.
    Embedded,
}

impl Layout {
    /// The heading prefix for a top-level section.
    const fn section(self) -> &'static str {
        match self {
            Self::Document => "##",
            Self::Embedded => "###",
        }
    }

    /// The heading prefix for one finding.
    const fn subsection(self) -> &'static str {
        match self {
            Self::Document => "###",
            Self::Embedded => "####",
        }
    }
}

/// Renders the diagnosis and the family table as a structured Markdown document.
///
/// The same analysis as the console rendering, in the one format that travels: a job summary panel,
/// a pull request comment, an issue, a file checked into a repository. Prose is left unwrapped
/// because a Markdown renderer reflows to the reader's width, and hard-wrapping to ours would
/// fight it.
///
/// The document is built to be navigated rather than read start to finish. A run that produced
/// eight findings is exactly the run whose reader wants to jump to one of them, so every section
/// is linkable, findings are numbered, and the table of contents names them.
#[must_use]
pub fn render_markdown(findings: &[Finding], rows: &[Yield], summary: Summary, timing: &Timing, layout: Layout) -> String {
    let mut out = String::new();

    if layout == Layout::Document {
        out.push_str("# Mutation testing advice\n\n");

        out.push_str(
            "What this run cost, what it found, and what could be changed — with the signal cost \
             of every change stated alongside it.\n\n",
        );

        write_contents(&mut out, findings, rows);
        write_outcome(&mut out, summary, timing);
    }

    write_findings(&mut out, findings, layout);
    write_yields(&mut out, rows, layout);
    write_glossary(&mut out, layout);

    out
}

/// Writes the table of contents.
fn write_contents(out: &mut String, findings: &[Finding], rows: &[Yield]) {
    out.push_str("## Contents\n\n");
    let _ = writeln!(out, "- [{RUN_HEADING}](#{})", slug(RUN_HEADING));
    let _ = writeln!(out, "- [{FINDINGS_HEADING}](#{})", slug(FINDINGS_HEADING));

    for (index, finding) in findings.iter().enumerate() {
        let heading = finding_heading(index, finding);

        let _ = writeln!(out, "  - [{heading}](#{})", slug(&heading));
    }

    if !rows.is_empty() {
        let _ = writeln!(out, "- [{YIELD_HEADING}](#{})", slug(YIELD_HEADING));
    }

    let _ = writeln!(out, "- [{GLOSSARY_HEADING}](#{})\n", slug(GLOSSARY_HEADING));
}

/// Writes what the run cost and what it decided, as two tables.
///
/// The verdicts and the cost are separate tables because they answer separate questions, and a
/// single table mixing counts with durations invites reading down a column that means two things.
///
/// The verdict rows are enumerated from [`Outcome::ALL`] and scored through [`Outcome::scoring`]
/// rather than listed here, because this table is read against the score on its last row. A
/// hand-written list is a list that can fall behind the enum, and it did: with the three counters
/// it omitted, a run stopped entirely by its memory ceiling rendered a header, a separator, and a
/// perfect score over nothing at all.
fn write_outcome(out: &mut String, summary: Summary, timing: &Timing) {
    let _ = writeln!(out, "## {RUN_HEADING}\n");
    let _ = writeln!(out, "| Verdict | Mutants | Share of score |\n|---|---:|---:|");

    // #[gamma::skip(literal.int_decrement, reason = "a scored row implies valid is at least one, while valid zero emits no scored rows, so max(0) and max(1) are observationally identical")]
    let valid = f64::from(summary.valid().max(1));

    for outcome in Outcome::ALL {
        let count = summary.count(outcome);

        if count == 0 {
            continue;
        }

        let share = if outcome.is_valid() {
            format!("{:.1}%", f64::from(count) * 100.0 / valid)
        } else {
            "not scored".to_owned()
        };

        let _ = writeln!(out, "| {} | {count} | {share} |", verdict_label(outcome));
    }

    let _ = writeln!(out, "| **Score** | | **{:.1}%** |\n", summary.score());

    let executed = timing.wall.saturating_sub(timing.build + timing.baseline);

    let _ = writeln!(out, "| Cost | Time | Share of run |\n|---|---:|---:|");

    for (label, spent) in [
        ("Build", timing.build),
        ("Baseline", timing.baseline),
        ("Testing mutants", executed),
    ] {
        let _ = writeln!(out, "| {label} | {} | {} |", human(spent), share(spent, timing.wall));
    }

    let _ = writeln!(out, "| **Total** | **{}** | at {} jobs |\n", human(timing.wall), timing.jobs);
}

/// How one verdict is named in the run table.
///
/// A `match` so that a new outcome has to be given a name here before it can be rendered, and
/// spelled out in this document's own words rather than in the short forms the console prints: the
/// audience for this file is whoever it was forwarded to.
const fn verdict_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Killed => "Killed",
        Outcome::Timeout => "Timed out",
        Outcome::OutOfMemory => "Stopped by the memory limit",
        Outcome::Survived => "Survived",
        Outcome::NoCoverage => "Uncovered",
        Outcome::Flaky => "Flaky",
        Outcome::CompileError => "Unviable",
        Outcome::Ignored => "Ignored",
        Outcome::NotBuilt => "Not built",
        Outcome::Pending => "Not run",
    }
}

/// Writes the findings, numbered so they can be referred to by position.
fn write_findings(out: &mut String, findings: &[Finding], layout: Layout) {
    let _ = writeln!(out, "{} {FINDINGS_HEADING}\n", layout.section());

    if findings.is_empty() {
        out.push_str(
            "Nothing crossed its threshold. Every check this tool makes looks for a cost that is \
             large enough to be worth trading signal for, and none of them fired.\n\n",
        );

        return;
    }

    out.push_str(
        "Findings follow a fixed diagnostic order: run-wide costs first, then costly verdicts, \
         population concentration, mutator yield, and uncovered code.\n\n",
    );

    for (index, finding) in findings.iter().enumerate() {
        let _ = writeln!(out, "{} {}\n", layout.subsection(), finding_heading(index, finding));
        let _ = writeln!(out, "Finding code: `{}`\n", finding.code);

        if !finding.detail.is_empty() {
            out.push_str("What was measured:\n\n");

            for line in &finding.detail {
                let _ = writeln!(out, "- {line}");
            }

            out.push('\n');
        }

        let _ = writeln!(out, "> **Remedy.** {}\n>", sentence(&finding.remedy));

        // The cost is never dropped, even here. A remedy quoted without what it gives up is how a
        // team ends up raising a score by measuring less.
        let _ = writeln!(out, "> **Costs.** {}\n", sentence(&finding.cost));
    }
}

/// Writes the per-family cost and value table.
fn write_yields(out: &mut String, rows: &[Yield], layout: Layout) {
    if rows.is_empty() {
        return;
    }

    let _ = writeln!(out, "{} {YIELD_HEADING}\n", layout.section());

    out.push_str(
        "Survivors per CPU-hour is what makes families comparable: it is the rate at which a \
         family bought the only thing a mutation run produces. A family near the bottom of this \
         table is the cheapest thing to turn off, and the last column says what turning it off \
         would have cost this run.\n\n",
    );

    out.push_str("| Family | Mutants | CPU | Survivors | Survivors/CPU-h |\n|---|---:|---:|---:|---:|\n");

    for row in rows {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {:.1} |",
            row.family,
            row.mutants,
            human(row.cpu),
            row.survivors,
            row.per_cpu_hour()
        );
    }

    out.push('\n');
}

/// Writes the definitions the rest of the document leans on.
///
/// Included because this file is written to be shared, and the person it gets forwarded to is
/// usually not the person who ran the tool.
fn write_glossary(out: &mut String, layout: Layout) {
    let _ = writeln!(out, "{} {GLOSSARY_HEADING}\n", layout.section());

    for (term, meaning) in [
        (
            "Killed",
            "a test failed while the mutant was active, which is the outcome you want.",
        ),
        (
            "Survived",
            "every test still passed with the mutant active, so nothing asserted on the behaviour it changed.",
        ),
        (
            "Timed out",
            "the suite never finished with the mutant active. Counted as undetected because no assertion rejected the change.",
        ),
        (
            "Out of memory",
            "the suite crossed its memory ceiling with the mutant active. Counted as undetected because no assertion rejected the change.",
        ),
        (
            "Uncovered",
            "no test reaches the code at all. Counted against the score exactly as a survivor is.",
        ),
        (
            "Unviable",
            "the mutant did not compile, so it says nothing about the tests and is left out of the score.",
        ),
        (
            "Baseline",
            "how long the suite takes with no mutant active. Every mutant pays this, so it multiplies by the population.",
        ),
    ] {
        let _ = writeln!(out, "- **{term}** — {meaning}");
    }

    out.push('\n');
}

/// The heading for one finding, numbered by position.
fn finding_heading(index: usize, finding: &Finding) -> String {
    format!("{}. {}", index + 1, sentence(&finding.headline))
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::advise::analysis::{analyze, yields};
    use crate::testing::advise_fixture::{mutant, timing};

    /// The verdict rows of the run table, as `(label, count, share)`.
    ///
    /// Taken from between the table's separator and the score line, so that neither the cost table
    /// below — whose rows are the same shape — nor the score itself can stand in for a verdict row
    /// a test is asserting the absence of.
    fn run_table_rows(document: &str) -> Vec<(String, u32, String)> {
        document
            .lines()
            .skip_while(|line| !line.starts_with("| Verdict |"))
            .skip(2)
            .take_while(|line| !line.starts_with("| **Score**"))
            .map(|line| {
                let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();

                assert_eq!(cells.len(), 3, "a verdict row has three cells: {line}");

                (
                    cells[0].to_owned(),
                    cells[1].parse().expect("a verdict row counts mutants"),
                    cells[2].to_owned(),
                )
            })
            .collect()
    }

    /// Every finding this crate constructs carries at least one measurement, but the field itself
    /// is a plain `Vec` a caller could hand over empty; the section it introduces has to disappear
    /// along with it; showing an empty "What was measured" header would read as though the
    /// renderer forgot to fill it in rather than as though there was nothing to say.
    #[test]
    fn a_finding_with_no_detail_lines_omits_the_measured_section() {
        let finding = Finding {
            code: "bare",
            headline: "a headline with nothing to back it up".to_owned(),
            detail: Vec::new(),
            remedy: "nothing to do".to_owned(),
            cost: "none".to_owned(),
        };

        let document = render_markdown(&[finding], &[], Summary::default(), &timing(1, 1, 100), Layout::Document);

        assert!(!document.contains("What was measured"), "{document}");
    }

    #[test]
    fn the_findings_introduction_describes_the_diagnostic_order() {
        let finding = Finding {
            code: "example",
            headline: "example".to_owned(),
            detail: Vec::new(),
            remedy: "act".to_owned(),
            cost: "none".to_owned(),
        };
        let document = render_markdown(&[finding], &[], Summary::default(), &timing(1, 1, 100), Layout::Document);

        assert!(document.contains(
            "Findings follow a fixed diagnostic order: run-wide costs first, then costly verdicts, \
             population concentration, mutator yield, and uncovered code.\n\n"
        ));
        assert!(!document.contains("remedy costs no signal"), "{document}");
    }

    #[test]
    fn every_contents_entry_points_at_a_heading_that_exists() {
        let mutants = vec![
            mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100),
            mutant("a.rs", "arith.add_to_sub", Outcome::Survived, 100),
        ];

        let timing = timing(50, 5, 100);
        let findings = analyze(&mutants, &timing);
        let summary = Summary::of(&mutants);
        let document = render_markdown(&findings, &yields(&mutants), summary, &timing, Layout::Document);

        assert!(!findings.is_empty(), "the fixture must produce something to link to");

        // A table of contents whose links do not resolve is worse than none, because it is only
        // discovered to be broken by someone who already had to scroll.
        let anchors: Vec<String> = document
            .lines()
            .filter_map(|line| {
                line.trim_start()
                    .strip_prefix("- [")
                    .or_else(|| line.trim_start().strip_prefix("  - ["))
            })
            .filter_map(|entry| entry.split("](#").nth(1).map(|tail| tail.trim_end_matches(')').to_owned()))
            .collect();

        let headings: Vec<String> = document
            .lines()
            .filter_map(|line| line.strip_prefix("## ").or_else(|| line.strip_prefix("### ")))
            .map(slug)
            .collect();

        assert!(anchors.len() >= 4, "{document}");

        for anchor in anchors {
            assert!(headings.contains(&anchor), "`{anchor}` is not a heading in:\n{document}");
        }
    }

    #[test]
    fn a_run_with_nothing_to_report_still_says_what_it_cost() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let timing = timing(1, 1, 100);
        let document = render_markdown(&[], &yields(&mutants), Summary::of(&mutants), &timing, Layout::Document);

        assert!(document.contains("## This run"), "{document}");
        assert!(document.contains("Nothing crossed its threshold"), "{document}");
        assert!(document.contains("| Killed | 1 | 100.0% |"), "{document}");
        assert!(document.contains("| **Score** | | **100.0%** |"), "{document}");
    }

    /// Every mutant the run produced appears in the table, and only the scored ones take a share.
    ///
    /// The population is deliberately lopsided, and deliberately includes the three counters most
    /// easily left without a row at all — memory exhaustion, a flake, and a mutant the build never
    /// compiled. With those at zero the table cannot be caught omitting them: a run whose mutants
    /// were all stopped by a memory ceiling would render a header and separator with nothing
    /// between them, and an all-zero fixture reproduces none of that. Summing the rows
    /// back up is what pins it, because a row that is missing, duplicated or reading the wrong
    /// counter all fail the same assertion.
    #[test]
    fn every_verdict_in_the_population_is_accounted_for_in_the_run_table() {
        let summary = Summary {
            killed: 3,
            survived: 1,
            timeout: 2,
            out_of_memory: 4,
            flaky: 5,
            unviable: 6,
            ignored: 7,
            uncovered: 8,
            not_built: 9,
            pending: 10,
        };

        let document = render_markdown(&[], &[], summary, &timing(1, 1, 10), Layout::Document);
        let rows = run_table_rows(&document);
        let counted: u32 = rows.iter().map(|(_, count, _)| count).sum();
        let scored: u32 = rows
            .iter()
            .filter(|(_, _, share)| share != "not scored")
            .map(|(_, count, _)| count)
            .sum();

        assert_eq!(counted, 55, "every mutant in the population must have a row: {document}");
        assert_eq!(scored, summary.valid(), "the scoring rows must sum to the denominator: {document}");

        // The score's numerator is on the page too: only assertion-driven kills.
        let detected: u32 = rows
            .iter()
            .filter(|(label, _, _)| label == "Killed")
            .map(|(_, count, _)| count)
            .sum();

        assert_eq!(
            detected,
            summary.detected(),
            "the detected rows must sum to the numerator: {document}"
        );
    }

    /// A run whose every mutant exhausted its memory ceiling scores 0%, and the table under that
    /// score has to identify the undetected outcome rather than being empty.
    #[test]
    fn a_run_of_nothing_but_memory_exhaustion_still_has_a_breakdown() {
        let summary = Summary {
            out_of_memory: 3,
            ..Summary::default()
        };

        let document = render_markdown(&[], &[], summary, &timing(1, 1, 10), Layout::Document);

        assert_eq!(
            run_table_rows(&document),
            vec![("Stopped by the memory limit".to_owned(), 3, "100.0%".to_owned())],
            "{document}"
        );
        assert!(document.contains("| **Score** | | **0.0%** |"), "{document}");
    }

    #[test]
    fn unscored_outcomes_are_named_as_not_scored_in_the_run_table() {
        let summary = Summary {
            flaky: 1,
            killed: 1,
            survived: 0,
            timeout: 0,
            out_of_memory: 1,
            unviable: 1,
            ignored: 1,
            uncovered: 0,
            not_built: 1,
            pending: 1,
        };
        let timing = timing(1, 1, 10);
        let document = render_markdown(&[], &[], summary, &timing, Layout::Document);

        // Flakes, unviable, ignored, unbuilt and pending mutants are not in the denominator, so the
        // table must not present them as a share of the mutation score. Memory exhaustion is in the
        // denominator and therefore still has a scored share despite earning no detection credit.
        assert_eq!(document.matches("not scored").count(), 5, "{document}");
        assert!(document.contains("| Stopped by the memory limit | 1 | 50.0% |"), "{document}");
    }

    #[test]
    fn the_document_renders_every_table_row_and_explanatory_section() {
        let finding = Finding {
            code: "example",
            headline: "example finding".to_owned(),
            detail: vec!["first measurement".to_owned(), "second measurement".to_owned()],
            remedy: "fix it".to_owned(),
            cost: "some signal".to_owned(),
        };
        let row = Yield {
            family: "relational".to_owned(),
            mutants: 2,
            cpu: Duration::from_mins(30),
            survivors: 3,
        };
        let summary = Summary {
            killed: 1,
            timeout: 1,
            survived: 1,
            uncovered: 1,
            unviable: 1,
            ignored: 1,
            pending: 1,
            ..Summary::default()
        };
        let timing = Timing {
            build: Duration::from_secs(10),
            baseline: Duration::from_secs(20),
            wall: Duration::from_secs(100),
            jobs: 4,
        };
        let document = render_markdown(&[finding], &[row], summary, &timing, Layout::Document);

        assert!(document.starts_with("# Mutation testing advice\n\nWhat this run cost, what it found, and what could be changed — with the signal cost of every change stated alongside it.\n\n"), "{document}");
        assert!(document.contains("## Contents\n\n- [This run](#this-run)\n- [Findings](#findings)\n  - [1. Example finding](#1-example-finding)\n- [Yield by mutator family](#yield-by-mutator-family)\n- [What the verdicts mean](#what-the-verdicts-mean)\n"), "{document}");
        for row in [
            "| Killed | 1 | 25.0% |",
            "| Timed out | 1 | 25.0% |",
            "| Survived | 1 | 25.0% |",
            "| Uncovered | 1 | 25.0% |",
            "| Unviable | 1 | not scored |",
            "| Ignored | 1 | not scored |",
            "| Not run | 1 | not scored |",
            "| Build | 10.0s | 10% |",
            "| Baseline | 20.0s | 20% |",
            "| Testing mutants | 70.0s | 70% |",
            "| **Total** | **2m** | at 4 jobs |",
        ] {
            assert!(document.contains(row), "missing `{row}` in:\n{document}");
        }
        assert!(document.contains("Findings follow a fixed diagnostic order: run-wide costs first, then costly verdicts, population concentration, mutator yield, and uncovered code.\n\n"), "{document}");
        assert!(
            document.contains(
                "What was measured:\n\n- first measurement\n- second measurement\n\n> **Remedy.** Fix it\n>\n> **Costs.** Some signal\n"
            ),
            "{document}"
        );
        assert!(document.contains("Survivors per CPU-hour is what makes families comparable: it is the rate at which a family bought the only thing a mutation run produces."), "{document}");
        assert!(document.contains("| Family | Mutants | CPU | Survivors | Survivors/CPU-h |\n|---|---:|---:|---:|---:|\n| `relational` | 2 | 30m | 3 | 6.0 |\n\n"), "{document}");
        for definition in [
            "- **Killed** — a test failed while the mutant was active, which is the outcome you want.",
            "- **Survived** — every test still passed with the mutant active, so nothing asserted on the behaviour it changed.",
            "- **Timed out** — the suite never finished with the mutant active. Counted as undetected because no assertion rejected the change.",
            "- **Out of memory** — the suite crossed its memory ceiling with the mutant active. Counted as undetected because no assertion rejected the change.",
            "- **Uncovered** — no test reaches the code at all. Counted against the score exactly as a survivor is.",
            "- **Unviable** — the mutant did not compile, so it says nothing about the tests and is left out of the score.",
            "- **Baseline** — how long the suite takes with no mutant active. Every mutant pays this, so it multiplies by the population.",
        ] {
            assert!(document.contains(definition), "missing `{definition}` in:\n{document}");
        }
        assert!(document.ends_with("\n\n"), "the glossary must end as a Markdown block:\n{document}");
    }

    #[test]
    fn empty_rows_are_absent_from_both_contents_and_body() {
        let document = render_markdown(&[], &[], Summary::default(), &timing(1, 1, 10), Layout::Document);
        assert!(!document.contains("- [Yield by mutator family]"), "{document}");
        assert!(!document.contains("## Yield by mutator family"), "{document}");
        assert!(!document.contains("| Killed | 0 |"), "{document}");
    }

    #[test]
    fn a_run_with_no_wall_time_reports_no_share_rather_than_a_division_by_zero() {
        assert_eq!(share(Duration::from_secs(1), Duration::ZERO), "—");
    }

    #[test]
    fn the_embedded_layout_nests_under_the_heading_its_host_already_wrote() {
        let mutants = vec![mutant("a.rs", "relational.lt_to_le", Outcome::Killed, 100)];
        let timing = timing(50, 5, 100);
        let findings = analyze(&mutants, &timing);
        let summary = Summary::of(&mutants);
        let panel = render_markdown(&findings, &yields(&mutants), summary, &timing, Layout::Embedded);

        assert!(!panel.contains("# Mutation testing advice"), "{panel}");
        assert!(!panel.contains("## Contents"), "{panel}");

        // The job summary panel states the score and the verdict counts itself, directly above.
        assert!(!panel.contains(RUN_HEADING), "{panel}");
        assert!(panel.starts_with("### Findings"), "{panel}");
    }

    #[test]
    fn finding_headings_are_numbered_and_capitalized() {
        let finding = Finding {
            code: "x",
            headline: "lowercase headline".to_owned(),
            detail: Vec::new(),
            remedy: String::new(),
            cost: String::new(),
        };
        assert_eq!(finding_heading(1, &finding), "2. Lowercase headline");
    }
}
