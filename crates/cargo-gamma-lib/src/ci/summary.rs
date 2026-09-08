// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Markdown written to `$GITHUB_STEP_SUMMARY`.

use core::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io;

use camino::Utf8Path;

use super::finding::{findings, relative};
use crate::HashMap;
use crate::model::{Mutant, Outcome, Summary};

/// How many under-tested files the job summary lists.
const SUMMARY_FILES: usize = 10;

/// Appends one complete panel to a GitHub step summary.
///
/// `O_APPEND` only makes each individual write append-safe. `write_all` can retry after a short
/// write, so the file lock spans the whole panel and prevents another cargo-gamma process from
/// placing its panel between those retries.
pub(crate) fn append(path: &Utf8Path, panel: &str) -> io::Result<()> {
    let lock = OpenOptions::new().append(true).create(true).read(true).open(path)?;
    let mut writer = lock.try_clone()?;

    append_locked(&lock, &mut writer, panel)
}

fn append_locked(lock: &File, writer: &mut impl io::Write, panel: &str) -> io::Result<()> {
    lock.lock()?;

    match writer.write_all(panel.as_bytes()) {
        Ok(()) => lock.unlock(),
        Err(cause) => {
            let _ = lock.unlock();

            Err(cause)
        }
    }
}

/// Renders the Markdown written to `$GITHUB_STEP_SUMMARY`.
///
/// This is the artifact a team actually reads every morning, so it leads with the number that
/// decides whether anyone reads further, and then spends its space on where the gaps are rather
/// than on restating the run's configuration.
#[must_use]
pub fn summary(mutants: &[Mutant], root: &Utf8Path) -> String {
    let totals = Summary::of(mutants);
    let mut text = String::from("## Mutation testing\n\n");

    // The headline is the score's own arithmetic, not a second opinion about it: the mutants
    // counted as detected are exactly `Summary::detected`: mutants rejected by a failing
    // assertion. The total is `Summary::valid`, its denominator. Timeouts and memory exhaustion
    // remain in that denominator because no assertion rejected them.
    //
    // The complement is labelled for what it counts, which is not survivors: a mutant no test
    // reaches is undetected too, and the table below lists the two separately because they ask the
    // reader to do different things — "no test links this code" and "the tests that run it did not
    // notice" are different problems. Calling the sum "survived" put a figure above a table that
    // contradicted it, with nothing on the page to explain the difference.
    let detected = totals.detected();
    let valid = totals.valid();

    let _ = writeln!(
        text,
        "**Score {:.1}%** — {detected} detected, {} not detected of {valid} mutants.\n",
        totals.score(),
        valid - detected
    );

    text.push_str("| Outcome | Count |\n|---|---:|\n");

    // Every outcome has a row, enumerated from `Outcome::ALL` rather than listed here, so the table
    // sums to the population. The ones that score nothing are here because they explain a
    // population smaller than the run: a mutant that would not compile, one that was suppressed,
    // one the build never produced and one that never ran are all absent from the number above, and
    // a reader who cannot see them reads the difference as mutants going missing — which is exactly
    // what a hand-written list did to the three outcomes it had fallen behind the enum on.
    for outcome in Outcome::ALL {
        let count = totals.count(outcome);

        if count > 0 {
            let _ = writeln!(text, "| {} | {count} |", label(outcome));
        }
    }

    let hot = under_tested(mutants, root);

    if !hot.is_empty() {
        text.push_str("\n### Where the gaps are\n\n| File | Not detected |\n|---|---:|\n");

        for (file, count) in hot {
            let _ = writeln!(text, "| {} | {count} |", cell(&file));
        }
    }

    text
}

/// How one outcome is named in the summary table.
///
/// A `match`, so a new outcome cannot reach a report without a name.
const fn label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Killed => "Killed",
        Outcome::Timeout => "Timed out",
        Outcome::OutOfMemory => "Out of memory",
        Outcome::Survived => "Survived",
        Outcome::NoCoverage => "Uncovered",
        Outcome::Flaky => "Flaky",
        Outcome::CompileError => "Unviable",
        Outcome::Ignored => "Suppressed",
        Outcome::NotBuilt => "Not built",
        Outcome::Pending => "Not run",
    }
}

/// Renders one path as a single Markdown table cell.
///
/// A file name is the one thing in this document that neither this tool nor the reader chooses, and
/// on Unix it may hold anything but a null byte and a slash — a pipe, which ends a table cell; a
/// backtick, which ends a code span; a newline, which ends the row and moves the count onto a line
/// of its own, where the table silently attributes it to some other file. A count printed beside
/// the wrong path is worse than an ugly one, so the path is escaped rather than trusted.
///
/// The escape is one-to-one, which is what "unambiguous" means here: two different paths cannot
/// render as the same cell, so a reader is never shown one file's name over another's count. The
/// backslash goes first for exactly that reason, since it is what every other escape is built out
/// of, and the delimiter that closes the code span is chosen longer than any run of backticks
/// inside it — the rule Markdown itself provides for this, and the only one that works for every
/// input.
fn cell(path: &str) -> String {
    let escaped = escape(path);
    let fence = "`".repeat(longest_run(&escaped) + 1);

    // A code span drops one space from each end when it has one at both, and cannot be empty at
    // all, so anything that starts or ends with a space or a backtick is given a pair of its own to
    // lose. Without it `` ` a ` `` and `` `a` `` are the same cell for two different files.
    let padding = if escaped.is_empty() || escaped.starts_with(['`', ' ']) || escaped.ends_with(['`', ' ']) {
        " "
    } else {
        ""
    };

    format!("{fence}{padding}{escaped}{padding}{fence}")
}

/// Rewrites the characters that cannot survive a table row, so that no two paths collide.
///
/// A pipe is escaped the way GitHub's tables specify, which is understood before the row is split
/// into cells and so works inside a code span as well as outside one. The rest are control
/// characters, which have no Markdown spelling at all: they are written as the escapes a Rust or C
/// programmer already reads, and the backslash that introduces them is escaped first so that the
/// mapping can be undone — a path holding the two characters `\` and `n` has to stay distinct from
/// one holding a newline.
fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());

    for character in path.chars() {
        match character {
            '\\' => out.push_str(r"\\"),
            '|' => out.push_str(r"\|"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            other if other.is_control() => {
                let _ = write!(out, r"\u{{{:04x}}}", u32::from(other));
            }
            other => out.push(other),
        }
    }

    out
}

/// The longest run of backticks in a string, which is what a code span around it has to beat.
fn longest_run(text: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;

    for character in text.chars() {
        if character == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }

    longest
}

/// The files with the most survivors, worst first.
fn under_tested(mutants: &[Mutant], root: &Utf8Path) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::default();

    for mutant in findings(mutants) {
        *counts.entry(relative(&mutant.file, root)).or_default() += 1;
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();

    // Ties broken by path so a summary is reproducible run to run; an unstable order turns every
    // morning's table into a diff nobody can read.
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.truncate(SUMMARY_FILES);
    ranked
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use core::time::Duration;
    use std::process::Command;
    use std::{env, fs, thread};

    use camino::Utf8PathBuf;

    use super::*;
    use crate::testing::ci_fixture::{mutant, root};

    const SHORT_WRITER_PATH: &str = "CARGO_GAMMA_SHORT_WRITER_PATH";
    const SHORT_WRITER_GATE: &str = "CARGO_GAMMA_SHORT_WRITER_GATE";
    const SHORT_WRITER_PANEL: &str = "CARGO_GAMMA_SHORT_WRITER_PANEL";

    struct ShortWriter {
        file: File,
    }

    impl io::Write for ShortWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let written = self.file.write(&buf[..buf.len().min(1)])?;

            thread::sleep(Duration::from_millis(1));

            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    #[test]
    fn concurrent_short_writes_keep_each_summary_panel_contiguous() {
        if let (Some(path), Some(gate), Some(panel)) = (
            env::var_os(SHORT_WRITER_PATH),
            env::var_os(SHORT_WRITER_GATE),
            env::var_os(SHORT_WRITER_PANEL),
        ) {
            let path = Utf8PathBuf::from(path.into_string().expect("the test path is UTF-8"));
            let gate = Utf8PathBuf::from(gate.into_string().expect("the test gate is UTF-8"));
            let panel = panel.into_string().expect("the test panel is UTF-8");

            while !gate.exists() {
                thread::sleep(Duration::from_millis(1));
            }

            let lock = OpenOptions::new()
                .append(true)
                .create(true)
                .read(true)
                .open(&path)
                .expect("open step summary");
            let writer = lock.try_clone().expect("clone summary handle");
            let mut writer = ShortWriter { file: writer };

            append_locked(&lock, &mut writer, &panel).expect("append one panel");

            return;
        }

        let directory = crate::testing::workdir("ci-summary-lock-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the test directory is UTF-8");
        let path = root.join("summary.md");
        let gate = root.join("go");
        let first = format!("<!-- first -->\n{}\n<!-- /first -->\n", "a".repeat(64));
        let second = format!("<!-- second -->\n{}\n<!-- /second -->\n", "b".repeat(64));
        let test = "ci::summary::tests::concurrent_short_writes_keep_each_summary_panel_contiguous";
        let executable = env::current_exe().expect("the test executable is known");

        let mut children = Vec::new();

        for panel in [&first, &second] {
            children.push(
                Command::new(&executable)
                    .args(["--exact", test, "--nocapture"])
                    .env(SHORT_WRITER_PATH, path.as_str())
                    .env(SHORT_WRITER_GATE, gate.as_str())
                    .env(SHORT_WRITER_PANEL, panel)
                    .spawn()
                    .expect("start concurrent writer"),
            );
        }

        thread::sleep(Duration::from_millis(20));
        fs::write(&gate, "").expect("release writers");

        for mut child in children {
            assert!(child.wait().expect("wait for writer").success(), "the writer process failed");
        }

        let written = fs::read_to_string(&path).expect("read step summary");

        assert!(
            written == format!("{first}{second}") || written == format!("{second}{first}"),
            "short writes interleaved panels: {written:?}"
        );
    }

    /// How many cells a Markdown reader finds in a table row.
    ///
    /// Cells are split on pipes before anything else is parsed, so a code span does not protect one
    /// and only a backslash does — and a backslash escapes whatever follows it, including another
    /// backslash, which is why an escape that left those alone could still hide an active pipe.
    fn cells(row: &str) -> usize {
        let mut count = 1;
        let mut characters = row.chars();

        while let Some(character) = characters.next() {
            match character {
                '\\' => {
                    let _ = characters.next();
                }
                '|' => count += 1,
                _other => {}
            }
        }

        count
    }

    /// The rows of the file table, which is the only place a path is rendered.
    ///
    /// Taken from below the table's separator so that the outcome table above, whose rows are the
    /// same shape, cannot stand in for the row under test.
    fn file_rows(text: &str) -> Vec<String> {
        let (_, gaps) = text.split_once("### Where the gaps are").expect("a file table");

        gaps.lines()
            .skip_while(|line| !line.starts_with("|---"))
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_summary_leads_with_the_score() {
        let mutants = vec![
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed),
            mutant("/w/src/a.rs", 2, "relational.gt_to_ge", Outcome::Survived),
        ];

        let text = summary(&mutants, &root());

        assert!(text.contains("**Score 50.0%**"), "{text}");
        assert!(text.contains("| Killed | 1 |"), "{text}");
        assert!(text.contains("| Survived | 1 |"), "{text}");
    }

    #[test]
    fn an_empty_outcome_is_left_out_of_the_summary_table() {
        let mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Killed)];
        let text = summary(&mutants, &root());

        assert!(!text.contains("Unviable"), "{text}");
        assert!(!text.contains("Where the gaps are"), "{text}");
    }

    /// Resource exhaustion remains in the denominator and out of the numerator.
    ///
    /// The population is deliberately lopsided — every scoring outcome a different size — so that
    /// dropping any one from the denominator changes a number this test names. Detected is the 3
    /// assertion kills; valid also includes 2 timeouts, 4 out-of-memory verdicts, 1 survivor and 5
    /// uncovered mutants, for a 20.0% score.
    #[test]
    fn the_headline_and_the_table_account_for_a_memory_kill() {
        let mut mutants = Vec::new();

        for (outcome, many) in [
            (Outcome::Killed, 3),
            (Outcome::Timeout, 2),
            (Outcome::OutOfMemory, 4),
            (Outcome::Survived, 1),
            (Outcome::NoCoverage, 5),
            (Outcome::CompileError, 6),
            (Outcome::Ignored, 7),
        ] {
            for line in 0..many {
                mutants.push(mutant("/w/src/a.rs", line + 1, "relational.gt_to_ge", outcome));
            }
        }

        let totals = Summary::of(&mutants);
        let text = summary(&mutants, &root());

        // The headline is the score's own arithmetic, so it reconciles with the score printed in
        // front of it and with what the console reports for the same run. Its second figure is the
        // score's complement, which includes resource exhaustion, the survivor and uncovered
        // mutants, so it is labelled for that union rather than any one outcome.
        assert!(
            text.contains("**Score 20.0%** — 3 detected, 12 not detected of 15 mutants."),
            "{text}"
        );
        assert_eq!(totals.detected(), 3);
        assert_eq!(totals.valid(), 15);
        assert_eq!(totals.survived, 1, "the headline's 6 must not be readable as survivors");

        assert!(text.contains("| Killed | 3 |"), "{text}");
        assert!(text.contains("| Timed out | 2 |"), "{text}");
        assert!(text.contains("| Out of memory | 4 |"), "{text}");
        assert!(text.contains("| Survived | 1 |"), "{text}");
        assert!(text.contains("| Uncovered | 5 |"), "{text}");

        // Every row that scores adds up to the headline, and so to the score above it.
        let scoring = [("Killed", 3)];
        let detected: u32 = scoring
            .iter()
            .filter(|(label, _)| text.contains(&format!("| {label} | ")))
            .map(|(_, count)| count)
            .sum();

        assert_eq!(detected, totals.detected(), "the killed rows must sum to the headline: {text}");
    }

    /// A run with nothing but memory exhaustion scores zero because no assertion killed it.
    #[test]
    fn a_memory_kill_is_the_whole_breakdown_when_it_is_the_whole_run() {
        let mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::OutOfMemory)];
        let text = summary(&mutants, &root());

        assert!(text.contains("**Score 0.0%** — 0 detected, 1 not detected of 1 mutants."), "{text}");
        assert!(text.contains("| Out of memory | 1 |"), "{text}");
    }

    /// The headline's complement counts uncovered mutants as well as survivors, so nothing on the
    /// page may call it survivors.
    ///
    /// The two are separate rows in the table directly beneath, and separate on purpose: "no test
    /// links this code" and "the tests that run it did not notice" send a reader to do different
    /// things. A figure labelled `survived` that disagrees with the `Survived` row two lines below
    /// leaves the reader with no way to tell which number is wrong.
    #[test]
    fn no_figure_in_the_summary_labels_uncovered_mutants_as_survivors() {
        let mut mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived)];

        for line in 0..5 {
            mutants.push(mutant("/w/src/b.rs", line + 2, "relational.gt_to_ge", Outcome::NoCoverage));
        }

        let totals = Summary::of(&mutants);
        let text = summary(&mutants, &root());

        assert_eq!(totals.survived, 1);
        assert_eq!(totals.valid() - totals.detected(), 6);

        // Every count the document states against the word "survived", wherever it appears, has to
        // be the number of survivors.
        let figures: Vec<&str> = text.split(" survived").collect();

        for figure in figures.iter().take(figures.len().saturating_sub(1)) {
            let count: u32 = figure
                .rsplit(|character: char| !character.is_ascii_digit())
                .next()
                .expect("a split always yields one part")
                .parse()
                .expect("every figure printed beside `survived` is a count");

            assert_eq!(count, totals.survived, "`{count} survived` is not the survivor count: {text}");
        }

        assert!(text.contains("6 not detected"), "{text}");
        assert!(text.contains("| Survived | 1 |"), "{text}");
        assert!(text.contains("| Uncovered | 5 |"), "{text}");
    }

    /// Every mutant in the run is in the table, whatever became of it.
    ///
    /// The table carries no score, so an omission does not move a number — it makes the rows stop
    /// adding up to the run, and a reader cannot tell a category nobody listed from mutants that
    /// went missing. One of each outcome, with distinct multiplicities so a row reading the wrong
    /// counter fails as loudly as a row that is absent.
    #[test]
    fn the_summary_table_accounts_for_every_mutant_in_the_run() {
        let mut mutants = Vec::new();

        for (index, outcome) in Outcome::ALL.into_iter().enumerate() {
            for line in 0..=index {
                mutants.push(mutant("/w/src/a.rs", line + 1, "relational.gt_to_ge", outcome));
            }
        }

        let totals = Summary::of(&mutants);
        let text = summary(&mutants, &root());

        let counted: usize = text
            .lines()
            .skip_while(|line| !line.starts_with("| Outcome |"))
            .skip(2)
            .take_while(|line| line.starts_with('|'))
            .map(|line| {
                line.trim_matches('|')
                    .split('|')
                    .nth(1)
                    .expect("an outcome row states a count")
                    .trim()
                    .parse::<usize>()
                    .expect("an outcome row's count is a number")
            })
            .sum();

        assert_eq!(counted, mutants.len(), "the table must account for every mutant: {text}");

        for outcome in Outcome::ALL {
            let count = totals.count(outcome);

            assert!(
                text.contains(&format!("| {} | {count} |", label(outcome))),
                "{outcome} is missing from the table: {text}"
            );
        }
    }

    #[test]
    fn the_summary_ranks_files_by_survivors() {
        let mut mutants = vec![mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived)];

        for line in 0..3 {
            mutants.push(mutant("/w/src/b.rs", line + 1, "relational.gt_to_ge", Outcome::Survived));
        }

        let ranked = under_tested(&mutants, &root());

        assert_eq!(ranked, vec![("src/b.rs".to_owned(), 3), ("src/a.rs".to_owned(), 1)]);
    }

    #[test]
    fn files_with_the_same_count_are_ordered_by_path() {
        let mutants = vec![
            mutant("/w/src/z.rs", 1, "relational.gt_to_ge", Outcome::Survived),
            mutant("/w/src/a.rs", 1, "relational.gt_to_ge", Outcome::Survived),
        ];

        let ranked = under_tested(&mutants, &root());

        assert_eq!(ranked[0].0, "src/a.rs");
    }

    /// An ordinary path is rendered as it always was: a plain code span, with nothing escaped.
    #[test]
    fn an_ordinary_path_is_left_alone() {
        assert_eq!(cell("src/lib.rs"), "`src/lib.rs`");
    }

    /// Each of these characters is legal in a Unix file name and each ends something in Markdown.
    /// The row stays two cells wide and one line long whichever one turns up.
    #[test]
    fn a_path_that_ends_a_cell_a_span_or_a_line_stays_in_one_cell() {
        for (what, name) in [
            ("a pipe", "a|b.rs"),
            ("a backtick", "a`b.rs"),
            ("a run of backticks", "a```b.rs"),
            ("a leading backtick", "`ab.rs"),
            ("a trailing backtick", "ab.rs`"),
            ("a newline", "a\nb.rs"),
            ("a carriage return", "a\rb.rs"),
            ("a tab", "a\tb.rs"),
            ("a backslash", "a\\b.rs"),
            ("a bell", "a\u{7}b.rs"),
            ("everything at once", "a|`\n`|b.rs"),
        ] {
            let mutants = vec![mutant(&format!("/w/src/{name}"), 1, "relational.gt_to_ge", Outcome::Survived)];
            let text = summary(&mutants, &root());
            let rows = file_rows(&text);

            assert_eq!(rows.len(), 1, "{what} must leave the file on one row: {text}");
            assert_eq!(cells(&rows[0]), 4, "{what} must leave one row of two cells: {}", rows[0]);
            assert!(rows[0].ends_with("| 1 |"), "{what} must keep the count in the row: {}", rows[0]);
        }
    }

    /// Two files whose names differ have to render as cells that differ, or the survivor count in
    /// the next column is attributed to a file that did not earn it.
    #[test]
    fn no_two_paths_render_as_the_same_cell() {
        let names = [
            "a|b.rs",
            "a\\|b.rs",
            "a\\b.rs",
            "ab.rs",
            "a\nb.rs",
            "a\\nb.rs",
            "a`b.rs",
            "a``b.rs",
            " a.rs",
            "a.rs ",
            "a.rs",
            "`a.rs`",
            "a\u{7}b.rs",
            "a\\u{0007}b.rs",
            "",
        ];

        for (index, left) in names.iter().enumerate() {
            for right in &names[index + 1..] {
                assert_ne!(cell(left), cell(right), "{left:?} and {right:?} render alike");
            }
        }
    }

    /// The delimiter has to be longer than any run of backticks inside the cell, and the content
    /// must not begin or end with one, or the span closes early and the rest of the path leaks into
    /// the table as Markdown.
    #[test]
    fn a_code_span_is_fenced_longer_than_the_backticks_it_holds() {
        assert_eq!(cell("a`b"), "``a`b``");
        assert_eq!(cell("a``b"), "```a``b```");
        assert_eq!(cell("`a`"), "`` `a` ``");

        // A code span cannot be empty, so the empty path is a pair of spaces rather than nothing:
        // "consists entirely of spaces" is the one case the code-span rule leaves alone, and a cell
        // that renders as nothing at all would read as a table this tool failed to fill in.
        assert_eq!(cell(""), "`  `");
    }
}
