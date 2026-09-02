// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Unit tests for internal helpers reachable through the `internals` facade.
//!
//! These live here rather than beside their implementations because they touch nothing the
//! facade does not already expose, so they compile against the crate as a consumer does.

mod identity {
    use camino::Utf8Path;
    use cargo_gamma_lib::internals::model::*;
    use serde_json::{from_str, to_string};

    fn assert_unwind_safe<T: core::panic::UnwindSafe + core::panic::RefUnwindSafe>() {}

    #[test]
    fn public_test_and_iteration_types_are_unwind_safe() {
        assert_unwind_safe::<cargo_gamma_lib::internals::discover::RecordEntries<'static>>();
        assert_unwind_safe::<cargo_gamma_lib::testing::CommandPause>();
    }

    #[test]
    fn a_site_key_separates_every_field_it_is_given() {
        let base = site_key("foo", "arith.add_to_sub", "a + b");

        assert_ne!(base, site_key("bar", "arith.add_to_sub", "a + b"));
        assert_ne!(base, site_key("foo", "arith.add_to_mul", "a + b"));
        assert_ne!(base, site_key("foo", "arith.add_to_sub", "a + c"));
        assert_eq!(base, site_key("foo", "arith.add_to_sub", "a + b"));

        // Without length prefixes these two would hash the same bytes and share a counter, which
        // would give two distinct sites the same occurrence index and so the same identity.
        assert_ne!(site_key("ab", "c", "d"), site_key("a", "bc", "d"));
    }

    /// Identifiers are a stable contract, not an implementation detail, and are pinned as one.
    ///
    /// A `gamma.toml` suppression names a mutant by id, and incremental execution decides what an earlier
    /// report already settled by id. Both outlive the run that produced them, so the hash is a
    /// format other people's files depend on: change how a nibble is indexed, how many bytes are
    /// taken, or which fields are fed in, and every suppression anyone has written stops matching
    /// the mutant it was written for — silently, because a suppression that matches nothing looks
    /// exactly like a suppression that is no longer needed.
    ///
    /// Pinning the literal output is the only assertion that can notice. The values here have no
    /// meaning beyond being what this code produced when the contract was fixed; if a deliberate
    /// change makes them wrong, the format has changed and old reports and suppressions have to be
    /// migrated rather than the expectations quietly updated.
    #[test]
    fn identifiers_are_stable_across_versions() {
        let path = Utf8Path::new("src/lib.rs");

        assert_eq!(
            mutant_id(path, "foo", "arith.add_to_sub", "a + b", SiteIndex::new(0, 0)),
            "97ac41aad8e4"
        );
        assert_eq!(
            mutant_id(path, "foo", "arith.add_to_sub", "a + b", SiteIndex::new(1, 0)),
            "b06d54ae21d3"
        );
        assert_eq!(
            mutant_id(path, "foo", "arith.add_to_sub", "a + b", SiteIndex::new(0, 1)),
            "4788ec1a4cbe"
        );
    }

    /// The identity of a site pins the whole normalization-plus-hash pipeline, not just the hash.
    ///
    /// `identifiers_are_stable_across_versions` pins ids over already-normalized text, so it proves
    /// nothing about `normalize_site_text` — the function whose entire job is to make an id survive
    /// a `cargo fmt`. If someone taught normalization to lowercase identifiers, to keep comments,
    /// or to collapse whitespace differently, `"a + b"` would still hash to the same bytes and that
    /// test would stay green while every real-world id silently moved. This one closes that gap: it
    /// feeds *raw*, un-normalized site text — one case per normalization branch — through the same
    /// path a real site takes, and pins the id at the far end.
    ///
    /// These literals are the same hard external contract as the ids above, and carry the same
    /// weight: a `gamma.toml` suppression and the incremental cache both name mutants by exactly
    /// these bytes. If one of these assertions fails, the pipeline that turns source into identity
    /// has changed, which means every stored verdict and every hand-written suppression across all
    /// users now points at nothing. That is a breaking change. Do NOT re-bless the literal to make
    /// the test green: either revert the normalization/hash change, or make it deliberately, bump
    /// whatever version gates the id format, and provide a migration for existing reports and
    /// suppressions. The value moving is the whole signal.
    ///
    /// The raw texts are chosen to reach every branch of `normalize_site_text` and span several
    /// structurally distinct mutator families (`arith`, `relational`, `lit`, `fn_value`) so that a
    /// change to how the mutator name feeds the hash is also caught:
    ///
    /// - nested block comment (`/* .. /* .. */ .. */`): outer-open, nested-open, nested-close and
    ///   outer-close branches, plus whitespace collapse around the stripped comment.
    /// - trailing line comment (`// ..\n`): the line-comment branch.
    /// - mixed-case identifiers (`Some(Value)`): the verbatim-character branch; case is preserved.
    /// - a multi-line call with leading indentation: the leading-whitespace-while-empty branch and
    ///   the whitespace-run-collapses-to-one-space branch across newlines.
    /// - a string literal holding comment-like text: pins identity-format version 2's
    ///   literal-aware normalization.
    /// - a division `a / b`: the `/` that is neither `//` nor `/*` and so is kept verbatim.
    #[test]
    fn identifiers_are_stable_across_normalization_of_raw_site_text() {
        let path = Utf8Path::new("src/lib.rs");

        // (item_path, mutator, RAW site text, occurrence, replacement_index, expected id)
        let golden = [
            (
                "foo",
                "arith.add_to_sub",
                "a /* outer /* inner */ still */ + b",
                0_u32,
                0_u32,
                "97ac41aad8e4",
            ),
            ("foo", "relational.gt_to_ge", "x + y // add two things\n", 0, 0, "daa754ee79ca"),
            ("foo", "lit.true_to_false", "Some(Value)", 0, 0, "9f1b32f7f171"),
            (
                "foo",
                "fn_value.some_default",
                "  foo(\n    Bar,\n    Baz,\n)",
                0,
                0,
                "82fac56cc040",
            ),
            ("foo", "fn_value.err_with", "let s = \"http://x /* y */\";", 0, 0, "db1ed8ff8671"),
            ("foo", "relational.lt_to_le", "a / b < c", 0, 0, "96c290d75e77"),
        ];

        for (item_path, mutator, raw_site_text, occurrence, replacement_index, expected) in golden {
            let normalized = normalize_site_text(raw_site_text);
            let id = mutant_id(path, item_path, mutator, &normalized, SiteIndex::new(occurrence, replacement_index));

            assert_eq!(
                id, expected,
                "id for mutator {mutator} over raw site text {raw_site_text:?} (normalized to {normalized:?})"
            );
        }
    }

    /// The identity newtype behaves as the bare string stored records, reports, and suppressions
    /// use, both on the wire and in comparisons.
    #[test]
    fn an_identity_is_a_bare_string_on_the_wire() {
        let id = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "arith.add_to_sub",
            "a + b",
            SiteIndex::new(0, 0),
        );
        let json = to_string(&id).expect("an identity serializes");

        assert_eq!(json, "\"97ac41aad8e4\"");
        assert_eq!(from_str::<MutantId>(&json).expect("an identity deserializes"), id);
        assert_eq!(id, "97ac41aad8e4");
        assert_eq!("97ac41aad8e4", id);
        assert_eq!(id.as_str(), "97ac41aad8e4");
        assert_eq!(id.to_string(), "97ac41aad8e4");
        assert_eq!(MutantId::new("97ac41aad8e4"), id);
        assert_eq!(MutantId::from("97ac41aad8e4".to_owned()), id);
    }

    /// `SiteIndex` groups the two identity coordinates and gives each a named accessor.
    ///
    /// Its constructor takes adjacent `u32` values in documented order; reversing them must change
    /// the identity.
    #[test]
    fn the_site_counts_are_grouped_and_named() {
        let path = Utf8Path::new("src/lib.rs");

        assert_ne!(
            mutant_id(path, "foo", "arith.add_to_sub", "a + b", SiteIndex::new(1, 0)),
            mutant_id(path, "foo", "arith.add_to_sub", "a + b", SiteIndex::new(0, 1))
        );

        let site = SiteIndex::new(4, 2);

        assert_eq!(site.occurrence(), 4);
        assert_eq!(site.replacement_index(), 2);
    }

    #[test]
    fn ids_are_twelve_hex_characters() {
        let id = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "arith.add_to_sub",
            "a + b",
            SiteIndex::new(0, 0),
        );

        assert_eq!(id.len(), MUTANT_ID_HEX_LEN);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(!id.is_heap_allocated());
    }

    #[test]
    fn literal_contents_remain_part_of_identity() {
        let first = normalize_site_text("let u = \"http://a  b\";");
        let second = normalize_site_text("let u = \"http://c b\";");

        assert_eq!(first, "let u = \"http://a  b\";");
        assert_eq!(second, "let u = \"http://c b\";");
        assert_ne!(first, second);
        assert_ne!(
            site_key("f", "fn_value.err_with", &first),
            site_key("f", "fn_value.err_with", &second)
        );
        assert_ne!(
            mutant_id(Utf8Path::new("src/lib.rs"), "f", "fn_value.err_with", &first, SiteIndex::new(0, 0)),
            mutant_id(Utf8Path::new("src/lib.rs"), "f", "fn_value.err_with", &second, SiteIndex::new(0, 0))
        );
    }

    #[test]
    fn ids_are_stable_for_identical_input() {
        let first = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "arith.add_to_sub",
            "a + b",
            SiteIndex::new(0, 0),
        );
        let second = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "arith.add_to_sub",
            "a + b",
            SiteIndex::new(0, 0),
        );

        assert_eq!(first, second);

        // Determinism has to hold over text that actually goes through normalization too, not just
        // over an already-clean `"a + b"`, so re-run the whole pipeline on comment- and
        // whitespace-bearing input and confirm two runs agree.
        let raw = "a /* c */  +\n\tb";
        let normalized = normalize_site_text(raw);
        let third = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "relational.gt_to_ge",
            &normalized,
            SiteIndex::new(0, 0),
        );
        let fourth = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "relational.gt_to_ge",
            &normalize_site_text(raw),
            SiteIndex::new(0, 0),
        );

        assert_eq!(third, fourth);
    }

    #[test]
    fn every_field_participates_in_identity() {
        let base = mutant_id(
            Utf8Path::new("src/lib.rs"),
            "foo",
            "arith.add_to_sub",
            "a + b",
            SiteIndex::new(0, 0),
        );

        let variants = [
            mutant_id(
                Utf8Path::new("src/other.rs"),
                "foo",
                "arith.add_to_sub",
                "a + b",
                SiteIndex::new(0, 0),
            ),
            mutant_id(
                Utf8Path::new("src/lib.rs"),
                "bar",
                "arith.add_to_sub",
                "a + b",
                SiteIndex::new(0, 0),
            ),
            mutant_id(
                Utf8Path::new("src/lib.rs"),
                "foo",
                "arith.add_to_mul",
                "a + b",
                SiteIndex::new(0, 0),
            ),
            mutant_id(
                Utf8Path::new("src/lib.rs"),
                "foo",
                "arith.add_to_sub",
                "a + c",
                SiteIndex::new(0, 0),
            ),
            mutant_id(
                Utf8Path::new("src/lib.rs"),
                "foo",
                "arith.add_to_sub",
                "a + b",
                SiteIndex::new(1, 0),
            ),
            mutant_id(
                Utf8Path::new("src/lib.rs"),
                "foo",
                "arith.add_to_sub",
                "a + b",
                SiteIndex::new(0, 1),
            ),
        ];

        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn field_boundaries_cannot_be_confused() {
        // Without length prefixing these two would hash the same bytes in the same order.
        let first = mutant_id(Utf8Path::new("ab"), "c", "d", "e", SiteIndex::new(0, 0));
        let second = mutant_id(Utf8Path::new("a"), "bc", "d", "e", SiteIndex::new(0, 0));

        assert_ne!(first, second);
    }

    #[test]
    fn normalization_erases_formatting() {
        assert_eq!(normalize_site_text("a   +\n\t b"), "a + b");
        assert_eq!(normalize_site_text("a /* why */ + b"), "a + b");
        assert_eq!(normalize_site_text("a + b // trailing\n"), "a + b");
        assert_eq!(normalize_site_text("  a+b  "), "a+b");
        assert_eq!(normalize_site_text("/* why */a+b"), normalize_site_text("/* why */ a+b"));
    }

    #[test]
    fn normalization_handles_nested_block_comments() {
        assert_eq!(normalize_site_text("a /* outer /* inner */ still */ + b"), "a + b");
    }

    #[test]
    fn normalization_preserves_meaning() {
        // Literal suffixes and bases are meaning, not formatting.
        assert_ne!(normalize_site_text("1_000u64"), normalize_site_text("1000"));
        assert_ne!(normalize_site_text("0x10"), normalize_site_text("16"));
        assert_ne!(normalize_site_text("a + b"), normalize_site_text("a + c"));
    }

    #[test]
    fn division_is_not_mistaken_for_a_comment() {
        assert_eq!(normalize_site_text("a / b"), "a / b");
    }
}

mod summary {

    use cargo_gamma_lib::internals::model::*;

    #[test]
    fn score_uses_detected_over_valid() {
        let summary = Summary {
            flaky: 0,
            killed: 7,
            survived: 2,
            timeout: 1,
            out_of_memory: 0,
            unviable: 5,
            ignored: 3,
            uncovered: 0,
            not_built: 0,
            pending: 0,
        };

        assert_eq!(summary.valid(), 10);
        assert_eq!(summary.detected(), 7);
        assert!((summary.score() - 70.0).abs() < f64::EPSILON);
    }

    #[test]
    fn covered_score_excludes_uncovered_mutants() {
        let summary = Summary {
            flaky: 0,
            killed: 4,
            survived: 4,
            timeout: 0,
            out_of_memory: 0,
            unviable: 0,
            ignored: 0,
            uncovered: 92,
            not_built: 0,
            pending: 0,
        };

        assert!((summary.score() - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn an_empty_run_scores_one_hundred() {
        let summary = Summary::default();

        assert!((summary.score() - 100.0).abs() < f64::EPSILON);
    }

    /// The printable score and the gradeable score part company on an empty population: printing
    /// 100% is honest about a run that caught everything it tested, but a threshold judged against
    /// it would pass a run that tested nothing, forever.
    #[test]
    fn an_empty_population_has_no_score_to_grade() {
        assert_eq!(Summary::default().scored(), None);
    }

    /// A population made entirely of mutants that never ran is just as ungradeable, even though it
    /// is not empty — this is the shard that held nothing but suppressions.
    #[test]
    fn a_population_of_only_excluded_mutants_has_no_score_to_grade() {
        let summary = Summary {
            flaky: 0,
            unviable: 3,
            ignored: 7,
            not_built: 2,
            ..Summary::default()
        };

        assert_eq!(summary.valid(), 0);
        assert_eq!(summary.scored(), None);
    }

    /// Every outcome must reach its own counter, and no other.
    ///
    /// `of` is a ten-arm match, and every other test in this module builds a `Summary` by hand and
    /// never goes through it — so two arms could be swapped, reporting timeouts as out-of-memory,
    /// and every total, every score and every existing fixture would be unchanged. The population
    /// here gives each outcome a distinct multiplicity for exactly that reason: with one mutant per
    /// outcome a swap is invisible, because both counters would read one either way.
    ///
    /// `weight` is a `match` rather than a table so that adding an `Outcome` variant fails to
    /// compile here, rather than silently leaving the new outcome unpinned.
    #[test]
    fn every_outcome_is_tallied_into_its_own_counter() {
        const fn weight(outcome: Outcome) -> u32 {
            match outcome {
                Outcome::Killed => 1,
                Outcome::Survived => 2,
                Outcome::Timeout => 3,
                Outcome::OutOfMemory => 4,
                Outcome::Flaky => 5,
                Outcome::CompileError => 6,
                Outcome::Ignored => 7,
                Outcome::NoCoverage => 8,
                Outcome::NotBuilt => 9,
                Outcome::Pending => 10,
            }
        }

        let every = [
            Outcome::Killed,
            Outcome::Survived,
            Outcome::Timeout,
            Outcome::OutOfMemory,
            Outcome::Flaky,
            Outcome::CompileError,
            Outcome::Ignored,
            Outcome::NoCoverage,
            Outcome::NotBuilt,
            Outcome::Pending,
        ];

        let mutants: Vec<Mutant> = every
            .iter()
            .flat_map(|outcome| {
                (0..weight(*outcome)).map(|index| cargo_gamma_lib::testing::ci_fixture::mutant("src/lib.rs", index as usize, "m", *outcome))
            })
            .collect();

        assert_eq!(
            Summary::of(&mutants),
            Summary {
                killed: 1,
                survived: 2,
                timeout: 3,
                out_of_memory: 4,
                flaky: 5,
                unviable: 6,
                ignored: 7,
                uncovered: 8,
                not_built: 9,
                pending: 10,
            }
        );

        // Nothing was dropped on the way in, and nothing was counted twice.
        let counted = every.iter().map(|outcome| weight(*outcome)).sum::<u32>();

        assert_eq!(counted as usize, mutants.len());
    }

    /// Every counter is reachable through `count`, and reaches the outcome it belongs to.
    ///
    /// The totals below are computed from named fields while every reporter reads the same numbers
    /// through `count`, so two arms swapped there would misattribute a whole row of a breakdown
    /// while leaving every score and every total untouched. Distinct multiplicities are what make a
    /// swap visible.
    #[test]
    fn every_outcome_reads_back_the_counter_it_was_tallied_into() {
        let summary = Summary {
            killed: 1,
            survived: 2,
            timeout: 3,
            out_of_memory: 4,
            flaky: 5,
            unviable: 6,
            ignored: 7,
            uncovered: 8,
            not_built: 9,
            pending: 10,
        };

        for (outcome, expected) in [
            (Outcome::Killed, 1),
            (Outcome::Survived, 2),
            (Outcome::Timeout, 3),
            (Outcome::OutOfMemory, 4),
            (Outcome::Flaky, 5),
            (Outcome::CompileError, 6),
            (Outcome::Ignored, 7),
            (Outcome::NoCoverage, 8),
            (Outcome::NotBuilt, 9),
            (Outcome::Pending, 10),
        ] {
            assert_eq!(summary.count(outcome), expected, "{outcome}");
        }
    }

    /// The totals and the per-outcome classification are two statements of the same rule, and a
    /// reporter that mixes them — a table driven by [`Outcome::scoring`] beneath a score computed
    /// from these sums — is only honest while they agree.
    #[test]
    fn the_totals_agree_with_the_shared_classification() {
        let summary = Summary {
            killed: 7,
            timeout: 4,
            out_of_memory: 2,
            survived: 3,
            uncovered: 9,
            flaky: 1,
            unviable: 6,
            ignored: 8,
            not_built: 10,
            pending: 11,
        };

        let valid: u32 = Outcome::ALL
            .iter()
            .filter(|outcome| outcome.is_valid())
            .map(|outcome| summary.count(*outcome))
            .sum();

        let detected: u32 = Outcome::ALL
            .iter()
            .filter(|outcome| outcome.is_detected())
            .map(|outcome| summary.count(*outcome))
            .sum();

        assert_eq!(valid, summary.valid());
        assert_eq!(detected, summary.detected());
    }

    /// Anything that did run is gradeable, and grades exactly as the printed score does.
    #[test]
    fn a_scored_population_grades_at_its_printed_score() {
        let summary = Summary {
            flaky: 0,
            killed: 3,
            survived: 1,
            ..Summary::default()
        };

        assert_eq!(summary.scored(), Some(summary.score()));
    }

    /// The score is `detected` over `valid`, and uncovered mutants belong to the denominator only.
    ///
    /// Every existing score test builds a population where at least one of those decisions cannot be
    /// distinguished from its opposite: with `uncovered` at zero, moving it out of the denominator
    /// changes nothing, and the classification that counts untested code against the score goes
    /// unpinned. This population gives every scoring outcome a distinct multiplicity — killed 7,
    /// timeout 4, out-of-memory 2, survived 3 and uncovered 9 in the denominator only — so that
    /// moving *any* single outcome across the line changes the asserted number: the numerator would
    /// read differently, or the denominator would shrink, and 28.0% would not
    /// survive it. The non-scoring outcomes carry distinct counts too, to prove they stay out of
    /// both. Detected = 7, valid = 7 + 3 + 4 + 2 + 9 = 25, score = 700 / 25 = 28.0.
    #[test]
    fn the_score_is_detected_over_valid_with_uncovered_counted_against_it() {
        let summary = Summary {
            killed: 7,
            timeout: 4,
            out_of_memory: 2,
            survived: 3,
            uncovered: 9,
            flaky: 1,
            unviable: 6,
            ignored: 8,
            not_built: 10,
            pending: 11,
        };

        assert_eq!(summary.detected(), 7);
        assert_eq!(summary.valid(), 25);
        assert!(
            (summary.score() - 28.0).abs() < f64::EPSILON,
            "7 detected over 25 valid is 28.0%, not {}",
            summary.score()
        );
    }
}

mod glob {

    use cargo_gamma_lib::internals::discover::*;

    #[test]
    fn a_bare_name_matches_at_any_depth() {
        assert!(matches_glob("lexer.rs", "src/parse/lexer.rs"));
        assert!(!matches_glob("lexer.rs", "src/parse/parser.rs"));
    }

    #[test]
    fn a_star_does_not_cross_separators() {
        assert!(matches_glob("src/*.rs", "src/main.rs"));
        assert!(!matches_glob("src/*.rs", "src/deep/main.rs"));
    }

    #[test]
    fn a_double_star_crosses_separators() {
        assert!(matches_glob("src/**/*.rs", "src/deep/nested/main.rs"));
        assert!(matches_glob("src/**", "src/deep/nested/main.rs"));
    }

    #[test]
    fn a_double_star_matches_zero_segments() {
        // `**/` has to match no directory at all, or `--file src/**/*.rs` would silently skip every
        // top-level file directly under `src/`, generating fewer mutants while still exiting clean.
        assert!(matches_glob("src/**/*.rs", "src/main.rs"));

        // Matching zero segments must not turn into matching anything: the prefix and the extension
        // still bind.
        assert!(!matches_glob("src/**/*.rs", "other/main.rs"));
        assert!(!matches_glob("src/**/*.rs", "src/main.txt"));
    }

    #[test]
    fn a_question_mark_matches_one_character() {
        assert!(matches_glob("a?.rs", "ab.rs"));
        assert!(matches_glob("?.rs", "é.rs"));
        assert!(!matches_glob("a?.rs", "abc.rs"));
        assert!(!matches_glob("src/?.rs", "src/nested/a.rs"));
    }

    #[test]
    fn repeated_double_stars_have_bounded_work() {
        let pattern = format!("{}b.rs", "**a".repeat(64));
        let path = format!("{}.rs", "a".repeat(1_024));

        assert!(!matches_glob(&pattern, &path));
    }

    #[test]
    fn an_exact_pattern_matches_exactly() {
        assert!(matches_glob("src/main.rs", "src/main.rs"));
        assert!(!matches_glob("src/main.rs", "src/other.rs"));
    }

    #[test]
    fn a_pattern_and_a_path_are_compared_on_the_same_separators() {
        // Regression, issue-002. `walkdir` yields `src\a.rs` on Windows while every pattern anyone
        // writes uses `/`, so a `--file src/*.rs` matched nothing there and the run silently
        // examined no files at all.
        assert!(matches_glob("src/*.rs", &format!("src{}a.rs", std::path::MAIN_SEPARATOR)));
        assert!(matches_glob(
            "src/**/*.rs",
            &format!("src{sep}deep{sep}a.rs", sep = std::path::MAIN_SEPARATOR)
        ));

        // Forward slashes keep working on every platform, since that is what a pattern is written in.
        assert!(matches_glob("src/*.rs", "src/a.rs"));
    }
}

mod diff {

    use camino::Utf8Path;
    use cargo_gamma_lib::internals::fix::*;

    /// The diff's lines with its two header lines dropped, since `+++` and `---` are not edits.
    fn body(text: &str) -> impl Iterator<Item = &str> {
        text.lines().skip(2)
    }

    /// Two empty texts diff to nothing, rather than indexing off the end of the edit graph.
    ///
    /// A file holding nothing but directives, all of which are removed, is emptied — and a dry run
    /// asked to show that edit passes two empty strings here. The graph has one diagonal in that
    /// case, and the first round's insertion step reaches for the diagonal beside it, which is only
    /// in the vector when at least one of the two texts has a line in it.
    #[test]
    fn two_empty_texts_diff_to_nothing() {
        let text = diff("src/lib.rs".into(), "", "");

        assert_eq!(body(&text).count(), 0, "{text}");
    }

    /// One empty side is the ordinary shape of a file being emptied or filled, and still has to work.
    #[test]
    fn an_emptied_file_is_all_removals_and_a_filled_one_all_additions() {
        let emptied = diff("src/lib.rs".into(), "a\nb\n", "");
        let filled = diff("src/lib.rs".into(), "", "a\nb\n");

        assert_eq!(body(&emptied).collect::<Vec<_>>(), vec!["-a", "-b"], "{emptied}");
        assert_eq!(body(&filled).collect::<Vec<_>>(), vec!["+a", "+b"], "{filled}");
    }

    #[test]
    fn a_diff_shows_an_inserted_line_as_an_addition() {
        let text = diff("src/lib.rs".into(), "a\nb\n", "a\nnew\nb\n");

        assert!(text.contains("\n a\n"), "{text}");
        assert!(text.contains("\n+new\n"), "{text}");
        assert!(text.contains("\n b\n"), "{text}");
        assert!(!body(&text).any(|line| line.starts_with('-')), "{text}");
    }

    /// The case the previous insertion-only renderer could not express at all.
    #[test]
    fn a_diff_shows_a_deleted_line_as_a_removal() {
        let text = diff("src/lib.rs".into(), "a\ngone\nb\n", "a\nb\n");

        assert!(text.contains("\n-gone\n"), "{text}");
        assert!(!body(&text).any(|line| line.starts_with('+')), "{text}");
    }

    #[test]
    fn a_diff_of_a_file_against_itself_is_all_context() {
        let text = diff("src/lib.rs".into(), "a\nb\nc\n", "a\nb\nc\n");

        assert!(!body(&text).any(|line| line.starts_with(['+', '-'])), "{text}");
        assert_eq!(body(&text).count(), 3, "{text}");
    }

    /// Every line of both texts has to be accounted for, or a preview is quietly lying about what
    /// the tool is about to do.
    #[test]
    fn a_diff_accounts_for_every_line_of_both_texts() {
        let before = "one\ntwo\nthree\nfour\n";
        let after = "one\nTWO\nthree\nfive\nfour\n";
        let text = diff("src/lib.rs".into(), before, after);
        let kept: Vec<&str> = body(&text).filter_map(|line| line.strip_prefix(' ')).collect();
        let added: Vec<&str> = body(&text).filter_map(|line| line.strip_prefix('+')).collect();
        let removed: Vec<&str> = body(&text).filter_map(|line| line.strip_prefix('-')).collect();

        let mut old_side = kept.clone();
        old_side.extend(removed.iter().copied());
        old_side.sort_unstable();

        let mut new_side = kept;
        new_side.extend(added.iter().copied());
        new_side.sort_unstable();

        let mut expected_old: Vec<&str> = before.lines().collect();
        let mut expected_new: Vec<&str> = after.lines().collect();

        expected_old.sort_unstable();
        expected_new.sort_unstable();

        assert_eq!(old_side, expected_old, "{text}");
        assert_eq!(new_side, expected_new, "{text}");
    }

    #[test]
    fn the_diff_marks_only_the_added_lines() {
        let before = "a();\nb();\n";
        let after = "a();\n// added\nb();\n";

        let text = diff(Utf8Path::new("src/lib.rs"), before, after);

        assert!(text.contains("+// added"), "{text}");
        assert!(text.contains(" a();"), "{text}");
        let added = text.lines().skip(2).filter(|line| line.starts_with('+')).count();

        assert_eq!(added, 1, "{text}");
    }
}

mod removal {

    use core::iter::once;

    use cargo_gamma_lib::internals::fix::*;

    #[test]
    fn removing_a_line_leaves_the_rest_untouched() {
        let text = remove("a\nb\nc\n", &once(2).collect());

        assert_eq!(text, "a\nc\n");
    }

    #[test]
    fn removing_several_lines_does_not_shift_the_ones_still_to_go() {
        let text = remove("a\nb\nc\nd\n", &[1_usize, 3].into_iter().collect());

        assert_eq!(text, "b\nd\n");
    }

    #[test]
    fn removal_keeps_the_line_endings_the_file_already_had() {
        assert_eq!(remove("a\r\nb\r\n", &once(1).collect()), "b\r\n");
    }

    #[test]
    fn a_line_that_is_only_a_directive_is_removable() {
        assert!(removable("    // #[gamma::skip(arith, reason = \"x\")]"));
        assert!(removable("#[gamma::skip]"));
        assert!(!removable("// gamma::skip(arith)"), "the unsupported shorthand is not a directive");
    }

    /// The conservative half, and the one that matters: each of these would take code with it.
    #[test]
    fn a_directive_sharing_its_line_with_anything_else_is_not_removable() {
        assert!(!removable("if a < b { // #[gamma::skip(relational)]"), "trailing on code");
        assert!(!removable("#[cfg_attr(test, gamma::skip)]"), "wrapped in a predicate");
        assert!(!removable("#[gamma::skip(arith,"), "runs onto the next line");
        assert!(!removable("#[inline] #[gamma::skip]"), "sharing with another attribute");
        assert!(!removable("fn f() {}"), "not a directive at all");
    }
}

mod lookup {

    use std::collections::HashSet;

    use cargo_gamma_lib::internals::ops::registry::*;

    #[test]
    fn registry_names_are_unique() {
        let mut seen: HashSet<&str> = HashSet::default();

        for mutator in REGISTRY {
            assert!(seen.insert(mutator.name), "duplicate mutator name {}", mutator.name);
        }
    }

    #[test]
    fn registry_names_are_family_dot_transform() {
        for mutator in REGISTRY {
            let parts: Vec<&str> = mutator.name.split('.').collect();

            assert_eq!(parts.len(), 2, "{} is not family.transform", mutator.name);
            assert!(!parts[0].is_empty() && !parts[1].is_empty(), "{}", mutator.name);
            assert!(
                parts
                    .iter()
                    .all(|p| p.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())),
                "{} is not snake_case",
                mutator.name
            );
        }
    }

    #[test]
    fn every_mutator_has_a_description() {
        for mutator in REGISTRY {
            assert!(!mutator.description.is_empty(), "{}", mutator.name);
        }
    }

    #[test]
    fn full_names_resolve_to_themselves() {
        assert_eq!(resolve("arith.add_to_sub").unwrap(), vec!["arith.add_to_sub"]);
    }

    #[test]
    fn family_prefixes_resolve_to_the_family() {
        let resolved = resolve("relational").unwrap();

        assert_eq!(resolved.len(), 10);
        assert!(resolved.contains(&"relational.lt_to_le"));
        assert!(!resolved.contains(&"arith.add_to_sub"));
    }

    #[test]
    fn presets_resolve() {
        let arithmetic = resolve("@arithmetic").unwrap();

        assert!(arithmetic.contains(&"arith.add_to_sub"));
        assert!(arithmetic.contains(&"bitwise.and_to_or"));
        assert!(arithmetic.contains(&"shift.shl_to_shr"));
        assert!(!arithmetic.contains(&"relational.lt_to_le"));
    }

    #[test]
    fn aliases_resolve_case_insensitively() {
        let upper = resolve("ROR").unwrap();
        let lower = resolve("ror").unwrap();

        assert_eq!(upper, lower);
        assert!(upper.contains(&"relational.eq_to_ne"));
    }

    #[test]
    fn all_resolves_to_the_whole_registry() {
        assert_eq!(resolve("all").unwrap().len(), REGISTRY.len());
    }

    #[test]
    fn unknown_selectors_suggest_a_spelling() {
        let error = resolve("arith.add_to_subb").unwrap_err();

        assert!(error.to_string().contains("did you mean `arith.add_to_sub`?"), "{error}");
    }

    #[test]
    fn a_misspelled_preset_suggests_the_preset() {
        let error = resolve("@arithmetics").unwrap_err();

        assert!(error.to_string().contains("@arithmetic"), "{error}");
    }

    #[test]
    fn a_wildly_wrong_selector_points_at_the_registry() {
        let error = resolve("zzzzzzzzzzzz").unwrap_err();

        assert!(error.to_string().contains("cargo gamma list mutators"), "{error}");
    }

    #[test]
    fn every_preset_resolves_and_is_non_empty() {
        for preset in PRESETS {
            let resolved = resolve(&format!("@{}", preset.name)).unwrap();

            assert!(!resolved.is_empty(), "preset @{} is empty", preset.name);
        }
    }

    #[test]
    fn families_are_listed_in_registry_order_without_duplicates() {
        let families = families();
        let mut seen: HashSet<&str> = HashSet::default();

        for family in &families {
            assert!(seen.insert(*family), "duplicate family {family}");
        }

        assert_eq!(families.first().copied(), Some("fn_value"));
    }
}
