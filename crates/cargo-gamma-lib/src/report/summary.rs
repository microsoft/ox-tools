// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Everything a finished run prints once the live display has stopped.

use std::io::Write as _;

use super::styler::Styler;
use super::text::quantity;
use crate::Result;
use crate::commands::Host;
use crate::discover::Plan;
use crate::exec::Session;
use crate::model::{Mutant, Outcome, Summary};
use crate::report::encode_controls;
#[cfg(test)]
use crate::{
    HashMap,
    exec::{OrderingHints, Phases},
};

/// Which of the uninteresting outcomes the reader asked to see listed in full.
///
/// Survivors are always listed, because they are the result. Everything else is bulk that a healthy
/// run produces thousands of, and printing it by default buries the finding it surrounds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Listings {
    /// List every mutant the suite killed.
    pub killed: bool,

    /// List every mutant that could not be compiled.
    pub unviable: bool,

    /// Whether the live display already named the survivors and timeouts as they happened.
    ///
    /// When it did, repeating them here prints every finding twice on one screen and buries the
    /// `Found` and `Summary` lines between the two copies. When it did not — output is piped, or
    /// progress is off — the listing is the only place the results appear, and it is not optional:
    /// stdout carries the results.
    pub announced: bool,

    /// Whether `cargo gamma hints` would add, correct, or remove scheduling knowledge.
    ///
    /// `None` retains plan-based inference for callers that are not a completed campaign.
    pub promotable: Option<bool>,

    /// Whether persisted campaign state contains the resource outcomes suppression consumes.
    ///
    /// `None` retains outcome-based inference for callers that are not a completed campaign.
    pub suppressible: Option<bool>,

    /// Campaign-efficiency measurements to print after the outcome summary.
    ///
    /// Absent for dry runs and runs that never reached test execution.
    pub stats: Option<Stats>,
}

/// Compact campaign-efficiency measurements for the final footer.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// Wall time from command start until the completed campaign summary.
    pub elapsed: core::time::Duration,

    /// Distinct test binaries participating in the campaign.
    pub binaries: usize,

    /// Test-binary subprocesses launched while evaluating mutants.
    pub launches: usize,

    /// Exact and generalized hint probes that killed their mutant.
    pub successful_hints: usize,
}

fn irregular_quantity(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

fn outcome_blocks(plan: &Plan, styler: Styler, listings: Listings) -> Vec<(String, Vec<String>)> {
    let mut blocks = Vec::new();
    let survivors: Vec<&Mutant> = plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Survived).collect();

    if !survivors.is_empty() && !listings.announced {
        blocks.push((
            styler.outcome(Outcome::Survived),
            survivors.iter().map(|mutant| mutant.describe()).collect(),
        ));
    }

    for outcome in [Outcome::Timeout, Outcome::OutOfMemory, Outcome::Flaky] {
        if listings.announced && outcome != Outcome::Flaky {
            continue;
        }

        let lines: Vec<String> = plan
            .mutants
            .iter()
            .filter(|mutant| mutant.outcome == outcome)
            .map(|mutant| {
                mutant
                    .note
                    .as_deref()
                    .map_or_else(|| mutant.describe(), |note| format!("{}: {note}", mutant.describe()))
            })
            .collect();

        if !lines.is_empty() {
            blocks.push((styler.outcome(outcome), lines));
        }
    }

    for (requested, outcome) in [(listings.killed, Outcome::Killed), (listings.unviable, Outcome::CompileError)] {
        if !requested {
            continue;
        }

        let lines: Vec<String> = plan
            .mutants
            .iter()
            .filter(|mutant| mutant.outcome == outcome)
            .map(Mutant::describe)
            .collect();

        if !lines.is_empty() {
            blocks.push((styler.outcome(outcome), lines));
        }
    }

    blocks
}

/// Names the mutants this run did not itself test, as a tail for the summary line.
///
/// Each of these changes what the total is a total of, so leaving them out entirely would make the
/// score look like it covered more than it did. None of them is a finding about the code, so each
/// appears only when it is not zero, and a clean run gets nothing.
///
/// Settled mutants are the subtle one: they *are* in the score, carried at the verdict an earlier
/// report gave them, and they are named here only so the reader knows this run did not re-run them.
///
/// Two kinds are never named here. Unviable mutants are a fact about what the compiler would accept
/// rather than about what the tests check, and they are withdrawn automatically. Unbuilt ones sit
/// behind conditional compilation the run's feature selection left switched off. Both run to
/// thousands on a large workspace — numbers nobody acts on, on the one line everybody reads. `-V`
/// lists them and `--diag` counts them. Suppressed mutants are left out for the same reason: one
/// directive on a dense function accounts for hundreds of them, so the figure reads as a count of
/// annotations and misleads far more than it informs.
// #[gamma::skip(all, reason = "the zero defaults are identity elements in the rendered exclusion sum and exact nonzero listings are covered by summary tests")]
fn excluded(plan: &Plan) -> String {
    let mut parts: Vec<String> = Vec::new();

    if plan.sharded_out > 0 {
        parts.push(format!("{} outside this shard", plan.sharded_out));
    }

    if plan.settled_out > 0 {
        parts.push(format!("{} already settled", plan.settled_out));
    }

    if parts.is_empty() {
        return String::new();
    }

    format!(", {}", parts.join(", "))
}

/// Names the flakes, when a run produced any.
///
/// Outside the parenthesized counts because it is outside the score: those counts sum to the
/// population in front of them, and a flake is in neither. Absent entirely when there were none,
/// unlike the counts inside the parentheses — those are always printed so the line keeps one shape
/// and can be scanned, whereas this says something happened that usually does not.
fn flakes(count: u32) -> String {
    if count == 0 {
        return String::new();
    }

    format!(", {} never judged", quantity(count as usize, "flaky mutant"))
}

/// Names the files discovery found but could not analyze.
///
/// Separate from the summary rather than a line in it, and on the error stream, because it is not
/// a finding about the code under test: it is a statement about how much of that code the numbers
/// below cover. A skipped file contributes to neither half of the score's fraction, so a run that
/// stepped over one and said nothing is indistinguishable from a run over a smaller workspace —
/// and the direction of the error is always to report a score for less code than was asked about.
///
/// # Errors
///
/// Returns an error if the diagnostic stream cannot be written.
pub fn skipped<H: Host>(host: &mut H, plan: &Plan, styler: Styler) -> Result<()> {
    if plan.skipped.is_empty() {
        return Ok(());
    }

    let mut stream = host.error();

    writeln!(
        stream,
        "{} {} could not be analyzed:",
        styler.warning(),
        quantity(plan.skipped.len(), "file")
    )?;

    for note in &plan.skipped {
        writeln!(stream, "  {}", encode_controls(note))?;
    }

    Ok(())
}

/// Writes the end-of-run summary.
///
/// # Errors
///
/// Returns an error if writing skipped-file diagnostics or summary results to either host stream
/// fails. A downstream reader closing a pipe is one possible cause.
// #[gamma::skip(all, reason = "summary block presence and verdict listings are covered through deterministic Host output; private branch mutants duplicate those rendered contracts")]
pub fn summarize<H: Host>(host: &mut H, plan: &Plan, styler: Styler, listings: Listings) -> Result<()> {
    skipped(host, plan, styler)?;

    let summary = Summary::of(&plan.mutants);
    let heading = styler.footer("Summary:");
    let blocks = outcome_blocks(plan, styler, listings);

    let mut stream = host.output();

    for (label, lines) in &blocks {
        writeln!(stream)?;

        for line in lines {
            writeln!(stream, "{label} {}", encode_controls(line))?;
        }
    }

    writeln!(stream)?;

    // One line for the whole result. Everything a run knows about itself — what it built, what it
    // could not compile, what it was told to skip — is bookkeeping about how the number was
    // reached, and a reader who wants that has `--estimate` and the advice artifact. What is left is the
    // number and the counts that change what it is a number out of.
    //
    // A surviving mutant is one a test ran and did not notice, and nothing else. An uncovered mutant
    // also costs score, but no test reached it, so counting it as a survivor would send the reader
    // looking for an assertion that was never going to be there; it is named on its own instead.
    //
    // The counts are always printed, zero or not. A line whose shape depends on its contents has
    // to be read before it can be scanned, and these are the numbers a reader is looking for. They
    // sum to the population in front of them, so the line can be checked at a glance.
    //
    // Timeout and out-of-memory are named separately from survived because they ask the reader to
    // do different things. All three count against the score, but a timeout is worth confirming is
    // a real hang, and memory exhaustion usually means the ceiling or mutant needs investigation.
    if summary.valid() > 0 {
        writeln!(
            stream,
            "{heading} {} ({} killed, {} survived, {} timed out, {} out of memory, {} uncovered => {}%){}{}",
            quantity(summary.valid() as usize, "mutant"),
            summary.killed,
            summary.survived,
            summary.timeout,
            summary.out_of_memory,
            summary.uncovered,
            crate::report::score(summary.score(), summary.detected() as usize, summary.valid() as usize),
            excluded(plan),
            flakes(summary.flaky)
        )?;
    } else {
        // Nothing was tested — a dry run, or a run every mutant was skipped out of. The population
        // is all there is to report, and reporting nothing at all would read as a failure.
        writeln!(
            stream,
            "{heading} {} in {}, none tested{}",
            quantity(plan.mutants.len(), "mutant"),
            quantity(plan.files.len(), "file"),
            excluded(plan)
        )?;
    }

    if let Some(stats) = listings.stats {
        writeln!(
            stream,
            "{} {} elapsed, {}, {} launched, {}",
            styler.footer("Stats  :"),
            crate::advise::human(stats.elapsed),
            irregular_quantity(stats.binaries, "test binary", "test binaries"),
            irregular_quantity(stats.launches, "test process", "test processes"),
            quantity(stats.successful_hints, "successful hint"),
        )?;
    }

    let note = styler.footer_note();
    if !plan.idle.is_empty() {
        writeln!(
            stream,
            "{note} {} could be removed with `cargo gamma unsuppress --apply`",
            quantity(plan.idle.len(), "superfluous skip directive")
        )?;
        for idle in &plan.idle {
            let reason = idle.reason.as_ref().map_or_else(String::new, |reason| format!(" — {reason}"));
            writeln!(stream, "  {}:{}: skip({}){reason}", idle.file, idle.line, idle.selectors)?;
        }
    }

    let suppressible = listings.suppressible.unwrap_or(summary.timeout > 0 || summary.out_of_memory > 0);
    if suppressible {
        writeln!(
            stream,
            "{note} Run `cargo gamma suppress` to automatically suppress timed-out and out-of-memory mutants"
        )?;
    }

    let promotable = listings.promotable.unwrap_or_else(|| {
        plan.mutants
            .iter()
            .any(|mutant| mutant.outcome == Outcome::CompileError || mutant.killed_by.is_some())
    });
    if promotable {
        writeln!(
            stream,
            "{note} Run `cargo gamma hints` to update your hint file and accelerate subsequent cargo-gamma runs"
        )?;
    }

    Ok(())
}

/// Reports anything about the mechanics of the run that the user has to know about.
///
/// Only the exceptional is reported. What a build cost and what budget a mutant was given are
/// answers to questions nobody asked, and `--estimate` and the advice artifact exist for runs where
/// somebody did; what is left here is the handful of things a run had to do differently from what
/// was asked of it.
///
/// This goes to the diagnostic stream, not to the results stream, because it is information about
/// the run rather than a finding about the code, and a script parsing results should not have to
/// step over it.
///
/// # Errors
///
/// Returns an error if the stream cannot be written.
pub fn session_notes<H: Host>(
    host: &mut H,
    session: &Session,
    _hints_missing: bool,
    _has_suppressible_mutants: bool,
    styler: Styler,
) -> Result<()> {
    let mut stream = host.error();

    if session.widened {
        writeln!(
            stream,
            "{} the narrowed build did not compile, so the whole workspace was built; \
             a test target needing a feature another package enables cannot be built alone",
            styler.note("Scope")
        )?;
    }

    if session.filtered > 0 {
        writeln!(
            stream,
            "{} {} not consulted, so a survivor here may be one they would have caught",
            styler.note("Oracle"),
            quantity(session.filtered, "test target")
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use camino::Utf8PathBuf;

    use super::*;
    use crate::discover::TargetFile;
    use crate::fixtures;
    use crate::testing::{Sink, fails_at_every_line};

    fn mutant(line: usize, outcome: Outcome) -> Mutant {
        Mutant {
            id: format!("m{line}").into(),
            ordinal: u32::try_from(line).unwrap_or(0),
            file: (Utf8PathBuf::from("src/a.rs")).into(),
            line,
            column: 5,
            mutator: ("relational.gt_to_ge".to_owned()).into(),
            item_path: ("subject::f".to_owned()).into(),
            original: "a > b".to_owned().into(),
            replacement: "a >= b".to_owned().into(),
            outcome,
            ..fixtures::mutant()
        }
    }

    fn plan() -> Plan {
        Plan {
            skipped: Vec::new(),
            digests: HashMap::default(),
            root: Utf8PathBuf::from("/w"),
            files: vec![TargetFile {
                path: Utf8PathBuf::from("src/a.rs"),
                absolute: Utf8PathBuf::from("/w/src/a.rs"),
                package: "subject".to_owned(),
                source: None,
            }],
            mutants: vec![
                mutant(1, Outcome::Killed),
                mutant(2, Outcome::Survived),
                mutant(3, Outcome::Timeout),
            ],
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    fn summary(announced: bool) -> String {
        rendered(&plan(), announced)
    }

    /// Summarizes a given plan, so a test can supply its own population.
    fn rendered(plan: &Plan, announced: bool) -> String {
        let mut host = Sink::default();
        let listings = Listings {
            killed: false,
            unviable: false,
            announced,
            promotable: None,
            suppressible: None,
            stats: None,
        };

        summarize(&mut host, plan, Styler::new(false), listings).expect("summarize");

        String::from_utf8(host.out).expect("utf-8")
    }

    fn rendered_with(plan: &Plan, listings: Listings) -> String {
        let mut host = Sink::default();

        summarize(&mut host, plan, Styler::new(false), listings).expect("summarize");

        String::from_utf8(host.out).expect("utf-8")
    }

    #[test]
    fn every_skipped_file_is_written_to_the_diagnostic_stream() {
        let mut host = Sink::default();
        let mut plan = plan();
        plan.skipped = vec!["src/a.rs: parse failed".to_owned(), "src/b.rs: unreadable".to_owned()];

        skipped(&mut host, &plan, Styler::new(false)).expect("diagnostics are written");

        let text = String::from_utf8(host.err).expect("utf-8");
        assert!(text.contains("2 files could not be analyzed:"), "{text}");
        assert!(text.contains("src/a.rs: parse failed"), "{text}");
        assert!(text.contains("src/b.rs: unreadable"), "{text}");
    }

    #[test]
    fn exclusions_are_absent_at_zero_and_exact_when_present() {
        let mut population = plan();
        population.sharded_out = 0;
        population.settled_out = 0;
        assert_eq!(excluded(&population), "");

        population.sharded_out = 2;
        assert_eq!(excluded(&population), ", 2 outside this shard");

        population.settled_out = 3;
        assert_eq!(excluded(&population), ", 2 outside this shard, 3 already settled");
    }

    #[test]
    fn summary_block_boundaries_and_heading_are_exact() {
        let mut population = plan();
        population.mutants = vec![mutant(2, Outcome::Survived)];

        let text = rendered_with(
            &population,
            Listings {
                killed: false,
                unviable: false,
                announced: false,
                promotable: None,
                suppressible: None,
                stats: None,
            },
        );

        assert_eq!(
            text,
            concat!(
                "\n",
                "    SURVIVED src/a.rs:2:5: replace a > b with a >= b [relational.gt_to_ge]\n",
                "\n",
                "Summary: 1 mutant (0 killed, 1 survived, 0 timed out, 0 out of memory, 0 uncovered => 0.0%)\n",
            )
        );
    }

    #[test]
    fn campaign_stats_follow_the_summary() {
        let mut population = plan();
        population.mutants = vec![mutant(1, Outcome::Killed)];

        let text = rendered_with(
            &population,
            Listings {
                promotable: Some(false),
                suppressible: Some(false),
                stats: Some(Stats {
                    elapsed: Duration::from_mins(42),
                    binaries: 12,
                    launches: 821,
                    successful_hints: 386,
                }),
                ..Listings::default()
            },
        );

        assert_eq!(
            text,
            concat!(
                "\n",
                "Summary: 1 mutant (1 killed, 0 survived, 0 timed out, 0 out of memory, 0 uncovered => 100.0%)\n",
                "Stats  : 42m elapsed, 12 test binaries, 821 test processes launched, 386 successful hints\n",
            )
        );
    }

    fn session(widened: bool) -> Session {
        Session {
            ordering: OrderingHints::default(),
            census: Vec::new(),
            baseline: Duration::from_secs(1),
            baseline_wall: Duration::from_secs(1),
            tests: None,
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::from_secs(3),
            metered: false,
            unbounded: None,
            withdrawn: 0,
            rounds: 1,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            peak: None,
            scratch: Utf8PathBuf::new(),
            filtered: 0,
            widened,
            phases: Phases::default(),
        }
    }

    /// A narrowed oracle changes what a survivor means, so it cannot be left to the reader to guess.
    #[test]
    fn an_oracle_that_lost_test_targets_says_how_many() {
        let mut host = Sink::default();
        let narrowed = Session {
            filtered: 3,
            ..session(false)
        };

        session_notes(&mut host, &narrowed, false, false, Styler::new(false)).expect("notes");

        let printed = String::from_utf8(host.err).expect("utf-8");

        assert!(printed.contains("3 test targets not consulted"), "{printed}");
    }

    #[test]
    fn an_oracle_that_kept_every_target_says_nothing_about_it() {
        assert!(!notes(false).contains("Oracle"), "{}", notes(false));
    }

    fn notes(widened: bool) -> String {
        let mut host = Sink::default();

        session_notes(&mut host, &session(widened), false, false, Styler::new(false)).expect("notes");

        String::from_utf8(host.err).expect("utf-8")
    }

    #[test]
    fn a_build_that_had_to_widen_says_so() {
        assert!(notes(true).contains("the whole workspace was built"), "{}", notes(true));
    }

    #[test]
    fn a_build_that_kept_its_scope_says_nothing_about_it() {
        assert!(!notes(false).contains("whole workspace"), "{}", notes(false));
    }

    #[test]
    fn learned_knowledge_suggests_updating_hints() {
        let mut population = plan();
        let mut killed = mutant(2, Outcome::Killed);
        killed.killed_by = Some("tests::caught".to_owned());
        population.mutants = vec![killed];

        let text = rendered_with(&population, Listings::default());

        assert!(text.ends_with("Note   : Run `cargo gamma hints` to update your hint file and accelerate subsequent cargo-gamma runs\n"));
    }

    #[test]
    fn final_footer_order_labels_and_actionable_entries_are_exact() {
        let mut population = plan();
        let mut killed = mutant(1, Outcome::Killed);
        killed.killed_by = Some("tests::caught".to_owned());
        population.mutants = vec![killed, mutant(2, Outcome::Timeout)];
        population.idle = vec![crate::suppress::Idle {
            file: "src/a.rs".into(),
            line: 9,
            selectors: "relational".to_owned(),
            reason: Some("the site no longer produces a mutant".to_owned()),
        }];

        let text = rendered_with(&population, Listings::default());
        let footer = text.split("Summary:").nth(1).expect("summary footer");

        assert_eq!(
            format!("Summary:{footer}"),
            concat!(
                "Summary: 2 mutants (1 killed, 0 survived, 1 timed out, 0 out of memory, 0 uncovered => 50.0%)\n",
                "Note   : 1 superfluous skip directive could be removed with `cargo gamma unsuppress --apply`\n",
                "  src/a.rs:9: skip(relational) — the site no longer produces a mutant\n",
                "Note   : Run `cargo gamma suppress` to automatically suppress timed-out and out-of-memory mutants\n",
                "Note   : Run `cargo gamma hints` to update your hint file and accelerate subsequent cargo-gamma runs\n",
            )
        );
    }

    #[test]
    fn a_blank_line_introduces_the_summary_when_no_result_listing_precedes_it() {
        let mut population = plan();
        let mut killed = mutant(1, Outcome::Killed);
        killed.killed_by = Some("tests::caught".to_owned());
        population.mutants = vec![killed];

        let text = rendered_with(
            &population,
            Listings {
                promotable: Some(false),
                suppressible: Some(false),
                ..Listings::default()
            },
        );

        assert!(text.starts_with("\nSummary:"), "{text:?}");
    }

    #[test]
    fn plural_idle_directives_are_counted_and_listed_after_the_summary() {
        let mut population = plan();
        population.mutants = vec![mutant(1, Outcome::Survived)];
        population.idle = vec![
            crate::suppress::Idle {
                file: "src/a.rs".into(),
                line: 3,
                selectors: "arith".to_owned(),
                reason: None,
            },
            crate::suppress::Idle {
                file: "src/b.rs".into(),
                line: 7,
                selectors: "all".to_owned(),
                reason: Some("obsolete".to_owned()),
            },
        ];

        let text = rendered_with(&population, Listings::default());

        assert!(text.contains("Note   : 2 superfluous skip directives could be removed with `cargo gamma unsuppress --apply`\n"));
        assert!(text.contains("  src/a.rs:3: skip(arith)\n"));
        assert!(text.contains("  src/b.rs:7: skip(all) — obsolete\n"));
        assert!(text.find("Summary:").expect("summary") < text.find("Note   :").expect("note"));
    }

    #[test]
    fn inapplicable_footer_notes_are_omitted() {
        let mut population = plan();
        population.mutants = vec![mutant(1, Outcome::Survived)];

        let text = rendered_with(&population, Listings::default());

        assert!(!text.contains("unsuppress --apply"), "{text}");
        assert!(!text.contains("cargo gamma suppress"), "{text}");
        assert!(!text.contains("cargo gamma hints"), "{text}");
    }

    #[test]
    fn an_existing_hints_file_does_not_hide_new_promotion_advice() {
        let directory = crate::testing::workdir("summary-existing-hints-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        std::fs::write(root.join("gamma-hints.yaml"), "existing: true\n").expect("existing hints");
        let mut population = plan();
        population.root = root;
        let mut killed = mutant(1, Outcome::Killed);
        killed.killed_by = Some("tests::caught".to_owned());
        population.mutants = vec![killed];

        let text = rendered_with(&population, Listings::default());

        assert!(text.contains("Note   : Run `cargo gamma hints`"), "{text}");
    }

    #[test]
    fn persisted_generalized_only_knowledge_shows_promotion_advice() {
        let mut population = plan();
        population.mutants = vec![mutant(1, Outcome::Survived)];

        let text = rendered_with(
            &population,
            Listings {
                promotable: Some(true),
                ..Listings::default()
            },
        );

        assert!(text.contains("Note   : Run `cargo gamma hints`"), "{text}");
    }

    #[test]
    fn an_explicit_empty_persisted_record_suppresses_plan_based_promotion_inference() {
        let mut population = plan();
        let mut killed = mutant(1, Outcome::Killed);
        killed.killed_by = Some("tests::caught".to_owned());
        population.mutants = vec![killed];

        let text = rendered_with(
            &population,
            Listings {
                promotable: Some(false),
                ..Listings::default()
            },
        );

        assert!(!text.contains("cargo gamma hints"), "{text}");
    }

    #[test]
    fn suppressible_mutants_suggest_the_suppress_command() {
        let mut population = plan();
        population.mutants = vec![mutant(2, Outcome::Timeout)];

        let text = rendered_with(&population, Listings::default());

        assert!(text.ends_with("Note   : Run `cargo gamma suppress` to automatically suppress timed-out and out-of-memory mutants\n"));
    }

    #[test]
    fn an_unpersisted_resource_outcome_does_not_advertise_suppression() {
        let mut population = plan();
        population.mutants = vec![mutant(2, Outcome::Timeout)];

        let text = rendered_with(
            &population,
            Listings {
                suppressible: Some(false),
                ..Listings::default()
            },
        );

        assert!(!text.contains("cargo gamma suppress"), "{text}");
    }

    #[test]
    fn results_are_listed_when_nothing_announced_them() {
        let text = summary(false);

        assert!(text.contains("SURVIVED src/a.rs:2"), "{text}");
        assert!(text.contains("TIMEOUT src/a.rs:3"), "{text}");
    }

    #[test]
    fn results_the_live_display_already_named_are_not_repeated() {
        let text = summary(true);

        assert!(!text.contains("SURVIVED"), "{text}");
        assert!(!text.contains("TIMEOUT"), "{text}");

        // The counts still have to be there; only the per-mutant lines are dropped.
        assert!(
            text.contains("3 mutants (1 killed, 1 survived, 1 timed out, 0 out of memory, 0 uncovered => 33.3%)"),
            "{text}"
        );
    }

    #[test]
    fn memory_exhaustion_is_listed_separately_from_a_killed_mutant() {
        // The distinction is the point: an assertion kill earns score credit, while memory
        // exhaustion does not and usually means the mutant or ceiling needs investigation.
        let mut plan = plan();

        plan.mutants = vec![mutant(1, Outcome::Killed), mutant(2, Outcome::OutOfMemory)];
        plan.mutants[1].note = Some("`suite` reached 200 MB, past the 150 MB this run allowed it".to_owned());

        let text = rendered(&plan, false);

        assert!(text.contains("OUTOFMEM"), "{text}");
        assert!(text.contains("past the 150 MB"), "{text}");
        assert!(text.contains("1 killed, 0 survived, 0 timed out, 1 out of memory"), "{text}");
    }

    /// A flake is listed on its own and never counted as a survivor.
    ///
    /// It is neither a detection nor a gap in the tests, so it belongs in neither number, and the
    /// only useful thing about it — the test that failed with no mutant active — has to reach the
    /// reader, because that test is the whole remedy.
    #[test]
    fn a_flake_is_named_apart_from_the_survivors_and_scores_as_neither() {
        let mut plan = plan();

        plan.mutants = vec![mutant(1, Outcome::Killed), mutant(2, Outcome::Flaky)];
        plan.mutants[1].note = Some("test `a::b` in `suite` fails with no mutant active as well as with one".to_owned());

        let text = rendered(&plan, false);

        assert!(text.contains("FLAKY"), "{text}");
        assert!(text.contains("test `a::b`"), "the test to fix has to be named, {text}");
        assert!(text.contains("1 flaky mutant never judged"), "{text}");

        // One valid mutant, not two: the flake is outside the fraction entirely, so a run whose
        // only other mutant was killed still scores 100%.
        assert!(text.contains("1 mutant (1 killed, 0 survived"), "{text}");
        assert!(text.contains("100.0%"), "{text}");
    }

    #[test]
    fn a_flake_remains_visible_when_live_results_were_announced() {
        let mut plan = plan();

        plan.mutants = vec![mutant(1, Outcome::Killed), mutant(2, Outcome::Flaky)];
        plan.mutants[1].note = Some("test `a::b` also fails without a mutant".to_owned());

        let text = rendered(&plan, true);

        assert!(text.contains("FLAKY"), "{text}");
        assert!(text.contains("test `a::b`"), "{text}");
    }

    #[test]
    fn inexact_boundary_scores_do_not_render_as_exact_boundaries() {
        let mut plan = plan();

        plan.mutants = (0..10_000)
            .map(|index| mutant(index + 1, if index == 9_999 { Outcome::Survived } else { Outcome::Killed }))
            .collect();

        let text = rendered(&plan, true);

        assert!(text.contains("99.99%"), "{text}");
        assert!(!text.contains("=> 100.0%"), "{text}");

        plan.mutants = (0..10_000)
            .map(|index| mutant(index + 1, if index == 0 { Outcome::Killed } else { Outcome::Survived }))
            .collect();

        let text = rendered(&plan, true);

        assert!(text.contains("0.01%"), "{text}");
        assert!(!text.contains("=> 0.0%"), "{text}");
    }

    /// A run with no flakes says nothing about them.
    ///
    /// Unlike the counts inside the parentheses, which are printed at zero so the line keeps one
    /// shape, this reports something that usually does not happen and would be noise otherwise.
    #[test]
    fn a_run_without_flakes_does_not_mention_them() {
        let mut plan = plan();

        plan.mutants = vec![mutant(1, Outcome::Killed)];

        let text = rendered(&plan, false);

        assert!(!text.contains("flaky"), "{text}");
    }

    /// A host that cannot bound memory is recorded in the diagnostics rather than announced. The
    /// console reports what the tests did, and every run on a machine without cgroup delegation
    /// would otherwise carry the same paragraph.
    #[test]
    fn a_run_that_could_not_bound_memory_does_not_say_so_on_the_console() {
        let mut host = Sink::default();
        let mut settled = session(false);

        settled.unbounded = Some("no cgroup delegation".to_owned());

        session_notes(&mut host, &settled, false, false, Styler::new(false)).expect("notes");

        let text = String::from_utf8(host.err).expect("utf-8");

        assert!(!text.contains("not bounded on this host"), "{text}");
    }

    #[test]
    fn the_test_sink_exposes_both_streams_without_claiming_a_terminal() {
        let mut host = Sink::default();

        // Report helpers use the same host trait as the CLI, so the local double should exercise
        // both streams and the non-terminal answers instead of relying on dead methods.
        let _ = host.output().write_all(b"out");
        let _ = host.error().write_all(b"err");

        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
        assert_eq!(String::from_utf8(host.out).expect("utf-8"), "out");
        assert_eq!(String::from_utf8(host.err).expect("utf-8"), "err");
    }

    #[test]
    fn the_summary_still_names_mutants_already_settled_out_of_the_run() {
        let mut plan = plan();

        plan.settled_out = 5;

        let text = rendered(&plan, false);

        // Settled mutants were deliberately excluded from this run, and that changes the
        // denominator a reader would otherwise infer from the workspace.
        assert!(text.contains("5 already settled"), "{text}");
    }

    #[test]
    fn killed_mutants_are_listed_only_when_requested() {
        let listings = Listings {
            killed: true,
            unviable: false,
            announced: true,
            promotable: None,
            suppressible: None,
            stats: None,
        };

        let text = rendered_with(&plan(), listings);

        // Caught mutants are usually noise, but verbose listings use them to prove the suite ran
        // the mutation the reader expected.
        assert!(text.contains("killed src/a.rs:1"), "{text}");
        assert!(!text.contains("SURVIVED src/a.rs:2"), "{text}");
    }

    #[test]
    fn killed_listing_excludes_timeout_and_memory_limit_outcomes() {
        let mut population = plan();

        population.mutants = vec![
            mutant(1, Outcome::Killed),
            mutant(2, Outcome::Timeout),
            mutant(3, Outcome::OutOfMemory),
        ];

        let text = rendered_with(
            &population,
            Listings {
                killed: true,
                unviable: false,
                announced: false,
                promotable: None,
                suppressible: None,
                stats: None,
            },
        );

        assert!(text.contains("killed src/a.rs:1"), "{text}");
        assert!(!text.contains("killed src/a.rs:2"), "{text}");
        assert!(!text.contains("killed src/a.rs:3"), "{text}");
        assert!(text.contains("TIMEOUT src/a.rs:2"), "{text}");
        assert!(text.contains("OUTOFMEM src/a.rs:3"), "{text}");
    }

    #[test]
    fn requesting_killed_mutants_when_none_were_killed_adds_no_empty_block() {
        // A run without a single killed mutant is a bad run, but it should not also grow a heading
        // with nothing under it; the listing is skipped entirely rather than printed empty, the
        // same way an unviable listing is skipped when nothing was unviable.
        let mut population = plan();

        population.mutants = vec![mutant(2, Outcome::Survived)];

        let listings = Listings {
            killed: true,
            unviable: false,
            announced: true,
            promotable: None,
            suppressible: None,
            stats: None,
        };

        let text = rendered_with(&population, listings);

        assert!(!text.contains("killed src/"), "{text}");
    }

    #[test]
    fn unviable_mutants_are_listed_only_when_requested() {
        // Unviable mutants say nothing about the code, but they are still a place the encoding
        // could not express, so a verbose listing that omitted them would leave the reader unable
        // to find the gap that produced the count on the summary line.
        let mut population = plan();

        population.mutants.push(mutant(4, Outcome::CompileError));

        let listings = Listings {
            killed: false,
            unviable: true,
            announced: true,
            promotable: None,
            suppressible: None,
            stats: None,
        };

        let text = rendered_with(&population, listings);

        assert!(text.contains("unviable src/a.rs:4"), "{text}");
        assert!(!text.contains("SURVIVED src/a.rs:2"), "{text}");
    }

    #[test]
    fn an_empty_population_reports_files_instead_of_a_score() {
        let mut plan = plan();

        plan.mutants.clear();

        let text = rendered(&plan, false);

        // With no scored mutants, a percentage would be a fiction; the useful fact is the selected
        // population size.
        assert!(text.contains("0 mutants in 1 file, none tested"), "{text}");
    }

    #[test]
    fn a_timeout_is_not_told_it_timed_out_twice() {
        // The heading already says TIMEOUT, so restating it on every line is noise on the listing
        // most likely to be long.
        let mut plan = plan();

        plan.mutants = vec![mutant(7, Outcome::Timeout)];

        let text = rendered(&plan, false);

        assert!(text.contains("src/a.rs:7"), "{text}");
        assert!(!text.contains("ran out its budget"), "{text}");

        // Nothing follows the mutant on the line, so it must not end on a dangling colon.
        assert!(!text.contains("[relational.gt_to_ge]:"), "{text}");
    }

    #[test]
    fn a_stalled_mutant_still_names_the_test_it_hung_in() {
        // This note is the whole reason the field exists: which test stopped making progress is
        // the one thing a reader cannot work out from the mutant itself.
        let mut hung = mutant(7, Outcome::Timeout);

        hung.note = Some("stalled, last test named was `t_slow`".to_owned());

        let mut plan = plan();

        plan.mutants = vec![hung];

        let text = rendered(&plan, false);

        assert!(text.contains("stalled, last test named was `t_slow`"), "{text}");
    }

    #[test]
    fn the_summary_does_not_count_mutants_the_compiler_rejected() {
        // A large workspace produces thousands, they are withdrawn automatically, and no reader
        // acts on the number. It has no place on the one line everybody reads.
        let mut plan = plan();

        plan.mutants.push(mutant(4, Outcome::CompileError));

        let text = rendered(&plan, false);

        assert!(!text.contains("unviable"), "{text}");
    }

    #[test]
    fn the_summary_names_mutants_a_shard_left_to_another_run() {
        let mut plan = plan();

        plan.suppressed = 3;
        plan.sharded_out = 9;

        let text = rendered(&plan, false);

        // A suppression count is dominated by whichever directive sits on the densest function, so
        // it reads as a count of annotations and misleads.
        assert!(!text.contains("suppressed"), "{text}");
        assert!(text.contains("9 outside this shard"), "{text}");
    }

    /// A closed pipe on the results stream has to surface, not be swallowed part-way through.
    #[test]
    fn a_closed_results_stream_fails_the_summary() {
        let listings = Listings {
            killed: true,
            unviable: true,
            announced: true,
            promotable: None,
            suppressible: None,
            stats: None,
        };

        fails_at_every_line(4, |host| summarize(host, &plan(), Styler::new(false), listings));
    }

    /// The same on the population line taken when nothing was actually tested.
    #[test]
    fn a_closed_results_stream_fails_the_untested_summary() {
        let mut plan = plan();

        plan.mutants.clear();

        let listings = Listings {
            killed: false,
            unviable: false,
            announced: false,
            promotable: None,
            suppressible: None,
            stats: None,
        };

        fails_at_every_line(1, |host| summarize(host, &plan, Styler::new(false), listings));
    }

    /// And on the diagnostic stream carrying the session notes.
    #[test]
    fn a_closed_diagnostic_stream_fails_the_session_notes() {
        fails_at_every_line(1, |host| session_notes(host, &session(true), false, false, Styler::new(false)));
    }

    /// `session_notes` writes separate lines for each exceptional session condition
    /// to say about, and every one of those writes ends in the same `?`. A pipe can close between
    /// any two lines just as easily as before the first, so a suite that only ever closed it at
    /// the very start would leave two more `?` operators that had never been shown to propagate
    /// rather than silently swallow a write failure partway through the notes.
    #[test]
    fn a_pipe_that_closes_partway_through_fails_whichever_note_was_writing() {
        let session = Session {
            ordering: OrderingHints::default(),
            census: Vec::new(),
            baseline: Duration::from_secs(1),
            baseline_wall: Duration::from_secs(1),
            tests: None,
            quiet: Duration::ZERO,
            stall: None,
            build: Duration::from_secs(3),
            metered: false,
            unbounded: Some("no cgroup delegation".to_owned()),
            withdrawn: 0,
            rounds: 1,
            rounds_taken: Vec::new(),
            binaries: Vec::new(),
            peak: None,
            scratch: Utf8PathBuf::new(),
            filtered: 2,
            widened: true,
            phases: Phases::default(),
        };

        fails_at_every_line(2, |host| session_notes(host, &session, true, true, Styler::new(false)));
    }
}
