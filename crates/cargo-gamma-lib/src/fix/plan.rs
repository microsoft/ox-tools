// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Choosing the directives a completed run should write, and applying them to a file's text.

use core::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use camino::Utf8PathBuf;

use super::{Edit, Eligible};
use crate::model::{Mutant, Outcome};

/// Chooses the directives to write for a completed run.
///
/// Mutants at the same site coalesce into one directive naming each mutator, rather than one comment
/// each: five stacked comments above a line is not something anyone will keep.
///
/// The tag is part of the grouping key, so a site whose mutants were stopped for different reasons
/// gets one directive per reason. Collapsing them would file a mutant under a verdict it never had,
/// and the tag is the whole basis on which a report groups suppressions.
#[must_use]
pub fn plan(mutants: &[Mutant], eligible: &[Eligible]) -> Vec<Edit> {
    let mut grouped: BTreeMap<(Utf8PathBuf, usize, &'static str), Edit> = BTreeMap::new();

    for mutant in mutants {
        // Belt and braces. `Eligible` cannot name a survivor, so this can only fire if someone adds
        // a variant later — which is exactly when a second check is worth having.
        // #[gamma::skip(cond.always_false, loop.delete_continue, reason = "the closed Eligible variants never return Survived; the following eligible lookup independently rejects every survivor")]
        if matches!(mutant.outcome, Outcome::Survived) {
            continue;
        }

        let Some(tag) = eligible
            .iter()
            .find(|entry| entry.outcome() == mutant.outcome)
            .map(|entry| entry.tag())
        else {
            continue;
        };

        let entry = grouped
            .entry((mutant.file.to_path_buf(), mutant.line, tag))
            .or_insert_with(|| Edit {
                file: mutant.file.to_path_buf(),
                line: mutant.line,
                mutators: BTreeSet::new(),
                tag,
            });

        let _ = entry.mutators.insert(mutant.mutator.to_string());
    }

    grouped.into_values().collect()
}

/// Applies edits to one file's text.
///
/// Edits are applied from the last line backwards so that every earlier line number stays valid; the
/// same discipline the instrumenter needs in the other direction, and the same bug if it is wrong.
///
/// A line that already carries a generated directive has its selector list extended instead of
/// gaining a second comment, which is what makes running this twice a no-op.
#[must_use]
pub fn apply(text: &str, edits: &[&Edit], date: &str) -> String {
    let ending = ending(text);
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let mut ordered: Vec<&&Edit> = edits.iter().collect();

    ordered.sort_by_key(|edit| Reverse(edit.line));

    for edit in ordered {
        let Some(index) = edit.line.checked_sub(1).filter(|index| *index < lines.len()) else {
            continue;
        };

        let indent: String = lines[index]
            .chars()
            .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
            .collect();

        if let Some(above) = matching_directive(&lines, index, edit.tag) {
            let mut merged = edit.mutators.clone();

            merged.extend(generated_selectors(&lines[above]).unwrap_or_default());

            let rendered = Edit {
                mutators: merged,
                ..(*edit).clone()
            }
            .render(&indent, date, ending);

            lines[above] = rendered;
            continue;
        }

        lines.insert(index, edit.render(&indent, date, ending));
    }

    lines.concat()
}

/// The line terminator the file already uses.
///
/// Decided by majority rather than by the first line seen, because a file with one stray ending is
/// still a file with a convention, and matching the stray one would spread it.
fn ending(text: &str) -> &'static str {
    let total = text.matches('\n').count();
    let carriage = text.matches("\r\n").count();

    if carriage * 2 > total { "\r\n" } else { "\n" }
}

/// Returns the line holding a generated directive tagged `tag`, among the run of generated
/// directives immediately above `index`.
///
/// The walk stops at the first line that is not a generated directive, so a directive belonging to
/// some construct further up is never mistaken for one governing this line. Walking the whole run
/// rather than checking only the line directly above is what keeps a second run idempotent once a
/// site carries more than one tag: the directive to extend may sit behind its siblings.
fn matching_directive(lines: &[String], index: usize, tag: &str) -> Option<usize> {
    let mut cursor = index;

    while let Some(above) = cursor.checked_sub(1) {
        if generated_tag(&lines[above])? == tag {
            return Some(above);
        }

        // #[gamma::skip(stmt.delete_assign, tag = "timeout", reason = "the suppression scan must advance to the preceding line")]
        cursor = above;
    }

    None
}

/// Returns the tag of a directive this tool generated, if the line holds one.
fn generated_tag(line: &str) -> Option<&str> {
    let inner = generated_body(line)?;
    let after = inner.split_once("tag = \"")?.1;

    after.split_once('"').map(|(tag, _rest)| tag)
}

/// Returns the argument text of a directive this tool generated, if the line holds one.
///
/// Only *generated* directives are extended. A hand-written directive is someone's decision, with
/// their reason attached, and rewriting it would destroy that reason to save one line.
fn generated_body(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();

    if !trimmed.contains("written by cargo gamma suppress") {
        return None;
    }

    trimmed.strip_prefix("// #[gamma::skip(")
}

/// Returns the selectors of a directive this tool generated, if the line holds one.
fn generated_selectors(line: &str) -> Option<Vec<String>> {
    let inner = generated_body(line)?;

    Some(
        inner
            .split(',')
            .map(str::trim)
            .take_while(|part| !part.contains('=') && !part.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// Returns today's UTC date as `YYYY-MM-DD`.
///
/// Hand-rolled rather than pulling in a date library, because this is the only date the tool ever
/// formats and the conversion is a well-known closed form. The date is what makes a generated
/// directive auditable a year later: "why is this here" is answerable from the comment alone.
#[must_use]
pub fn today() -> String {
    today_at(SystemTime::now())
}

fn today_at(now: SystemTime) -> String {
    let seconds = now.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());

    civil_from_days(i64::try_from(seconds / 86_400).unwrap_or(0))
}

/// Converts a count of days since the Unix epoch into `YYYY-MM-DD`.
///
/// Hinnant's algorithm, which shifts the year to start in March so that the leap day lands at the
/// end and the month-length pattern becomes a single linear expression.
fn civil_from_days(days: i64) -> String {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 };
    let year = era * 400 + year_of_era + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::iter::once;

    use super::*;
    use crate::fixtures::mutant_at as mutant;

    #[test]
    fn survivor_generated_dates_change_at_exact_utc_midnight_and_handle_gregorian_century_exceptions() {
        assert_eq!(today_at(UNIX_EPOCH - std::time::Duration::from_secs(1)), "1970-01-01");
        for (seconds, expected) in [
            (0, "1970-01-01"),
            (86_399, "1970-01-01"),
            (86_400, "1970-01-02"),
            (172_799, "1970-01-02"),
            (172_800, "1970-01-03"),
        ] {
            assert_eq!(today_at(UNIX_EPOCH + std::time::Duration::from_secs(seconds)), expected);
        }
        for (days, expected) in [
            (-133_621, "1604-02-28"),
            (-133_620, "1604-02-29"),
            (-98_557, "1700-02-28"),
            (-98_556, "1700-03-01"),
            (-25_509, "1900-02-28"),
            (-25_508, "1900-03-01"),
            (11_015, "2000-02-28"),
            (11_016, "2000-02-29"),
            (11_017, "2000-03-01"),
            (157_112, "2400-02-28"),
            (157_113, "2400-02-29"),
        ] {
            assert_eq!(civil_from_days(days), expected, "independent Gregorian day offset {days}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "exhaustive 801-year calendar sweep; bounded era-boundary coverage remains enabled")]
    fn civil_calendar_matches_independent_month_iteration_across_eight_centuries() {
        let leap = |year: i64| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let mut days = -(1600..1970).map(|year| if leap(year) { 366 } else { 365 }).sum::<i64>();
        for year in 1600..=2400 {
            for month in 1..=12 {
                let length = match month {
                    2 => {
                        if leap(year) {
                            29
                        } else {
                            28
                        }
                    }
                    4 | 6 | 9 | 11 => 30,
                    _ => 31,
                };
                for day in 1..=length {
                    assert_eq!(
                        civil_from_days(days),
                        format!("{year:04}-{month:02}-{day:02}"),
                        "epoch offset {days}"
                    );
                    days += 1;
                }
            }
        }
    }

    #[test]
    fn apply_skips_invalid_lines_without_abandoning_earlier_edits_and_keeps_blank_indents() {
        for (text, ending) in [("first\n\nlast\n", "\n"), ("first\r\n\r\nlast\r\n", "\r\n")] {
            let edit = |line| Edit {
                file: "source.rs".into(),
                line,
                mutators: BTreeSet::from(["stmt.delete".to_owned()]),
                tag: "timeout",
            };
            let invalid = edit(4);
            let blank = edit(2);
            let first = edit(1);
            let zero = edit(0);
            assert_eq!(
                apply(text, &[&invalid, &blank, &first, &zero], "2030-01-02"),
                format!(
                    "{}first{ending}{}{ending}last{ending}",
                    first.render("", "2030-01-02", ending),
                    blank.render("", "2030-01-02", ending)
                )
            );
        }
    }

    #[test]
    fn line_ending_majority_uses_strict_half_and_preserves_dominant_carriage_returns() {
        for (text, expected) in [
            ("", "\n"),
            ("a\r\nb\n", "\n"),
            ("a\r\nb\r\nc\n", "\r\n"),
            ("a\r\nb\nc\n", "\n"),
            ("a\r\n", "\r\n"),
        ] {
            assert_eq!(ending(text), expected);
        }
    }

    #[test]
    fn generated_directive_parsing_rejects_missing_provenance_and_malformed_fields() {
        for text in [
            "// #[gamma::skip(stmt.delete, tag = \"timeout\", reason = \"handwritten\")]",
            "not a directive written by cargo gamma suppress",
            "// #[gamma::skip(stmt.delete, reason = \"written by cargo gamma suppress\")]",
        ] {
            assert_eq!(generated_tag(text), None);
        }
        let malformed = "// #[gamma::skip(stmt.delete,, arith.add_to_sub, reason = \"written by cargo gamma suppress\")]";
        assert_eq!(generated_selectors(malformed), Some(vec!["stmt.delete".to_owned()]));
        assert_eq!(generated_selectors("plain text"), None);
        let generated =
            "  // #[gamma::skip(stmt.delete, arith.add_to_sub, tag = \"timeout\", reason = \"written by cargo gamma suppress\")]";
        assert_eq!(generated_tag(generated), Some("timeout"));
        assert_eq!(
            generated_selectors(generated),
            Some(vec!["stmt.delete".to_owned(), "arith.add_to_sub".to_owned()])
        );
    }

    #[test]
    fn apply_extends_multiple_existing_sites_without_stopping_after_first_merge() {
        let first = Edit {
            file: "source.rs".into(),
            line: 1,
            mutators: BTreeSet::from(["stmt.delete".to_owned()]),
            tag: "timeout",
        };
        let second = Edit { line: 2, ..first.clone() };
        let original = apply("one\ntwo\n", &[&first, &second], "2030-01-02");
        let first = Edit {
            line: 2,
            mutators: BTreeSet::from(["arith.add_to_sub".to_owned()]),
            ..first
        };
        let second = Edit { line: 4, ..first.clone() };
        let merged = apply(&original, &[&first, &second], "2030-02-03");
        let expected = Edit {
            mutators: BTreeSet::from(["arith.add_to_sub".to_owned(), "stmt.delete".to_owned()]),
            ..first
        };
        assert_eq!(
            merged,
            format!(
                "{}one\n{}two\n",
                expected.render("", "2030-02-03", "\n"),
                expected.render("", "2030-02-03", "\n")
            )
        );
    }

    #[test]
    fn civil_date_conversion_handles_the_end_of_a_four_hundred_year_era() {
        for (days, expected) in [
            (-1, "1969-12-31"),
            (0, "1970-01-01"),
            (11_015, "2000-02-28"),
            (11_016, "2000-02-29"),
            (11_017, "2000-03-01"),
        ] {
            assert_eq!(civil_from_days(days), expected);
        }
    }

    #[test]
    fn a_survivor_is_skipped_even_if_it_reaches_the_planner() {
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 4, "relational.lt_to_le", Outcome::Survived),
            mutant("bbb", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
        ];

        let edits = plan(&mutants, &[Eligible::Timeout]);

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].line, 9);
    }

    #[test]
    fn timeouts_are_eligible_by_default_and_unviables_are_opt_in() {
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 4, "fn_value.default", Outcome::CompileError),
            mutant("bbb", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
        ];

        assert_eq!(plan(&mutants, &[Eligible::Timeout]).len(), 1);
        assert_eq!(plan(&mutants, &[Eligible::Timeout, Eligible::Unviable]).len(), 2);
    }

    #[test]
    fn ineligible_outcomes_are_not_planned() {
        let mutants = vec![mutant("aaa", "src/lib.rs", 4, "fn_value.default", Outcome::Killed)];

        assert!(plan(&mutants, &[Eligible::Timeout, Eligible::Unviable]).is_empty());
    }

    #[test]
    fn mutators_at_one_site_coalesce_into_a_single_directive() {
        // Five stacked comments above one line is not something anyone keeps.
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
            mutant("bbb", "src/lib.rs", 9, "arith.add_to_sub", Outcome::Timeout),
        ];

        let edits = plan(&mutants, &[Eligible::Timeout]);

        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].mutators.len(), 2);
    }

    /// Coalescing stops at the tag, because the tag is a claim about why the mutant was stopped.
    ///
    /// One site can hold a mutant that hung and a mutant that ran the machine out of memory, and
    /// both are eligible by default. Folding them into one directive would file one of them under a
    /// verdict it never had, and the tag is the whole basis on which a report groups suppressions.
    #[test]
    fn one_site_stopped_for_two_reasons_gets_one_directive_for_each() {
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 9, "stmt.delete", Outcome::Timeout),
            mutant("bbb", "src/lib.rs", 9, "arith.add_to_sub", Outcome::OutOfMemory),
        ];

        let edits = plan(&mutants, &[Eligible::Timeout, Eligible::OutOfMemory]);

        assert_eq!(edits.len(), 2, "one directive per reason");

        let tags: Vec<&str> = edits.iter().map(|edit| edit.tag).collect();

        assert!(tags.contains(&"timeout"), "{tags:?}");
        assert!(tags.contains(&"outofmem"), "{tags:?}");

        for edit in &edits {
            assert_eq!(edit.line, 9);
            assert_eq!(edit.mutators.len(), 1, "a mutator belongs only to the reason that stopped it");
        }
    }

    /// Both directives survive into the text, and both still govern the line below them.
    #[test]
    fn two_reasons_at_one_site_stack_above_the_line_they_govern() {
        let text = "fn f() {\n    loop {}\n}\n";
        let mutants = vec![
            mutant("aaa", "src/lib.rs", 2, "stmt.delete", Outcome::Timeout),
            mutant("bbb", "src/lib.rs", 2, "arith.add_to_sub", Outcome::OutOfMemory),
        ];

        let edits = plan(&mutants, &[Eligible::Timeout, Eligible::OutOfMemory]);
        let borrowed: Vec<&Edit> = edits.iter().collect();
        let out = apply(text, &borrowed, "2026-08-05");
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines.len(), 5, "two directives above the loop: {out}");
        assert!(lines[1].contains("tag = \"timeout\""), "{out}");
        assert!(lines[2].contains("tag = \"outofmem\""), "{out}");
        assert_eq!(lines[3].trim(), "loop {}", "the directives sit directly above the line: {out}");

        // A second run re-scans the patched file, so its edits name the line the code sits on now —
        // two lower, behind both directives. Extending the matching one rather than stacking a
        // duplicate is only possible because the search walks past the sibling tag to find its own.
        let moved: Vec<Edit> = edits
            .iter()
            .map(|edit| Edit {
                line: edit.line + 2,
                ..edit.clone()
            })
            .collect();
        let borrowed_again: Vec<&Edit> = moved.iter().collect();
        let again = apply(&out, &borrowed_again, "2026-08-05");

        assert_eq!(again, out, "a second application is a no-op");
    }

    /// A tag nothing above the line carries gets its own directive rather than rewriting a sibling.
    ///
    /// The second edit names line 3 because that is where `loop {}` sits once the first directive is
    /// in, which is what a re-scan of the patched file would report. Naming the original line would
    /// stop the search above the directive and pass no matter what the merge rule was.
    #[test]
    fn a_new_reason_at_a_suppressed_site_does_not_retag_the_existing_directive() {
        let text = "fn f() {\n    loop {}\n}\n";
        let first = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };
        let second = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 3,
            mutators: once("arith.add_to_sub".to_owned()).collect(),
            tag: "outofmem",
        };

        let once_applied = apply(text, &[&first], "2026-08-05");
        let twice_applied = apply(&once_applied, &[&second], "2026-08-05");

        assert!(twice_applied.contains("tag = \"timeout\""), "{twice_applied}");
        assert!(twice_applied.contains("tag = \"outofmem\""), "{twice_applied}");
        assert!(
            twice_applied.contains("gamma::skip(stmt.delete,"),
            "the first directive keeps its own selector: {twice_applied}"
        );
        assert!(
            !twice_applied.contains("gamma::skip(arith.add_to_sub, stmt.delete,"),
            "the two reasons must not merge into one directive: {twice_applied}"
        );
    }

    #[test]
    fn a_directive_is_written_above_the_line_at_its_indentation() {
        let text = "fn f() {\n    loop {}\n}\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines[1].starts_with("    // #[gamma::skip("), "{out}");
        assert_eq!(lines[2], "    loop {}");
    }

    #[test]
    fn edits_for_missing_lines_are_ignored() {
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 99,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        assert_eq!(apply("fn f() {}\n", &[&edit], "2026-08-05"), "fn f() {}\n");
    }

    #[test]
    fn edits_are_applied_from_the_end_so_earlier_lines_stay_valid() {
        // Applying forwards shifts every later line by one and puts the second directive one line
        // too high, which is silent: the file still compiles and suppresses the wrong thing.
        let text = "a();\nb();\nc();\n";
        let first = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 1,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };
        let second = Edit { line: 3, ..first.clone() };

        let out = apply(text, &[&first, &second], "2026-08-05");
        let lines: Vec<&str> = out.lines().collect();

        assert!(lines[0].contains("gamma::skip"), "{out}");
        assert_eq!(lines[1], "a();");
        assert_eq!(lines[2], "b();");
        assert!(lines[3].contains("gamma::skip"), "{out}");
        assert_eq!(lines[4], "c();");
    }

    #[test]
    fn running_twice_is_a_no_op() {
        let text = "fn f() {\n    loop {}\n}\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let once = apply(text, &[&edit], "2026-08-05");

        // The second pass sees the directive it wrote, so the line it targets has moved down by one.
        let again = apply(&once, &[&Edit { line: 3, ..edit }], "2026-08-05");

        assert_eq!(once, again);
        assert_eq!(again.matches("gamma::skip").count(), 1, "{again}");
    }

    #[test]
    fn a_second_mutator_extends_the_generated_directive_rather_than_stacking() {
        let text = "fn f() {\n    loop {}\n}\n";
        let first = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let once = apply(text, &[&first], "2026-08-05");
        let second = Edit {
            line: 3,
            mutators: core::iter::once("arith.add_to_sub".to_owned()).collect(),
            ..first
        };
        let twice = apply(&once, &[&second], "2026-08-05");

        assert_eq!(twice.matches("gamma::skip").count(), 1, "{twice}");
        assert!(twice.contains("arith.add_to_sub"), "{twice}");
        assert!(twice.contains("stmt.delete"), "{twice}");
    }

    #[test]
    fn a_hand_written_directive_is_never_rewritten() {
        // Someone's reason is the most valuable thing in the file, and it is not recoverable.
        let text = "fn f() {\n    // #[gamma::skip(stmt.delete, reason = \"driver poll, see RFC-12\")]\n    loop {}\n}\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 3,
            mutators: once("arith.add_to_sub".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");

        assert!(out.contains("RFC-12"), "{out}");
        assert_eq!(out.matches("gamma::skip").count(), 2, "{out}");
    }

    #[test]
    fn the_epoch_converts_to_its_known_date() {
        // Two fixed points, one of them a leap day, because the whole algorithm is about where the
        // leap day lands.
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_417), "2023-03-01");
        assert_eq!(civil_from_days(18_321), "2020-02-29");
    }

    #[test]
    fn today_is_a_plausible_date() {
        let date = today();

        assert_eq!(date.len(), 10, "{date}");
        assert!(date.starts_with("20"), "{date}");
    }

    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        // A lone LF in an otherwise CRLF file is a whitespace change on a line nobody edited, which
        // is exactly the kind of diff that makes a team stop trusting an automated fix.
        let text = "fn f() {\r\n    loop {}\r\n}\r\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");

        assert_eq!(out.matches('\n').count(), out.matches("\r\n").count(), "{out:?}");
        assert!(out.contains("    // #[gamma::skip(stmt.delete,"), "{out:?}");
    }

    #[test]
    fn an_lf_file_keeps_its_line_endings_even_with_one_stray_crlf() {
        // Majority rather than first-seen: a file with one stray ending still has a convention, and
        // matching the stray one would spread it.
        let text = "fn f() {\n    loop {}\r\n}\n\n\n";
        let edit = Edit {
            file: Utf8PathBuf::from("src/lib.rs"),
            line: 2,
            mutators: once("stmt.delete".to_owned()).collect(),
            tag: "timeout",
        };

        let out = apply(text, &[&edit], "2026-08-05");

        assert!(out.contains("suppress 2026-08-05\")]\n    loop"), "{out:?}");
    }
}
