// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use crate::cfg::CfgSet;
use crate::model::{Channel, Mutant, Outcome};
use crate::ops::collect;
use crate::ops::registry::Selection;
use crate::parse::SourceFile;

pub(super) fn file(source: &str) -> SourceFile {
    SourceFile::parse("test.rs", source.to_owned()).unwrap()
}

pub(super) fn mutants_of(source: &str, mutators: &str) -> (SourceFile, Vec<Mutant>) {
    let parsed = file(source);
    let selection = Selection::parse(mutators).unwrap();
    let candidates = collect::collect(&parsed, &selection);
    let mut mutants = collect::into_mutants(&parsed, "p", candidates);

    for (index, mutant) in mutants.iter_mut().enumerate() {
        mutant.ordinal = u32::try_from(index).unwrap() + 1;
    }

    (parsed, mutants)
}

#[test]
fn a_file_with_no_directives_yields_none() {
    assert!(directives(&file("fn f(a: i32) -> i32 { a + 1 }")).unwrap().is_empty());
}

/// A leftover `#[mutants::skip]` has to be inert rather than fatal: the attribute belongs to
/// another tool, and rejecting it would break every codebase that still carries one.
#[test]
fn the_mutants_skip_attribute_is_ignored() {
    let source = "#[mutants::skip]\nfn f(a: i32, b: i32) -> i32 { a + b }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();

    assert!(found.is_empty(), "{found:?}");
    assert_eq!(suppress(&mut mutants, &found), 0);
}

/// Its comment spellings are prose now, in both the bracketed and the bare form, and neither is a
/// usage error the way an unresolvable `gamma::` comment is. Nor is an argument vocabulary that was
/// never ours to police, which the tool that owns the attribute is still free to use.
#[test]
fn the_mutants_skip_comment_spellings_are_ignored() {
    for source in [
        "// #[mutants::skip(arith)]\nfn f(a: i32, b: i32) -> i32 { a + b }",
        "// mutants::skip\nfn f(a: i32, b: i32) -> i32 { a + b }",
        "#[cfg_attr(test, mutants::skip)]\nfn f(a: i32) -> i32 { a + 1 }",
        "#[mutants::skip(ticket = \"T-1\")]\nfn f(a: i32) -> i32 { a + 1 }",
    ] {
        let (parsed, mut mutants) = mutants_of(source, "arith");
        let found = directives(&parsed).expect(source);

        assert!(found.is_empty(), "{source}: {found:?}");
        assert_eq!(suppress(&mut mutants, &found), 0, "{source}");
    }
}

#[test]
fn a_bare_skip_covers_every_mutator() {
    let source = "#[gamma::skip]\nfn f(a: i32, b: i32) -> i32 { a + b }";
    let (parsed, mut mutants) = mutants_of(source, "all");
    let found = directives(&parsed).unwrap();
    let count = suppress(&mut mutants, &found);

    assert_eq!(count, mutants.len());
    assert!(count > 0);
}

#[test]
fn a_directive_only_suppresses_the_mutators_it_names() {
    let source = "// #[gamma::skip(arith.add_to_sub)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let _ = suppress(&mut mutants, &found);

    for mutant in &mutants {
        let expected = if mutant.mutator == ("arith.add_to_sub").into() {
            Outcome::Ignored
        } else {
            Outcome::Pending
        };

        assert_eq!(mutant.outcome, expected, "{}", mutant.mutator);
    }
}

#[test]
fn the_reason_reaches_the_suppressed_mutant() {
    let source = "// #[gamma::skip(arith, reason = \"why not\")]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let _ = suppress(&mut mutants, &found);
    let suppression = mutants[0].suppression.as_ref().unwrap();

    assert_eq!(suppression.reason.as_deref(), Some("why not"));
    assert_eq!(suppression.channel, Channel::Comment);
    assert_eq!(suppression.line, Some(1));
}

#[test]
fn a_doc_comment_is_never_a_directive() {
    // A doc comment is published text. Giving it a second meaning would be a trap.
    let source = "/// #[gamma::skip(arith)]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn an_unrelated_attribute_is_ignored() {
    let source = "#[inline]\n#[serde(skip)]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn an_unknown_gamma_attribute_is_an_error_rather_than_silence() {
    // A misspelling that is silently ignored is the worst outcome: the source reads as if the
    // site is suppressed, and the mutants come back as survivors anyway.
    let source = "#[gamma::note]\nfn f(a: i32) -> i32 { a + 1 }";
    let error = directives(&file(source)).expect_err("an unknown directive is a usage error");

    assert!(error.to_string().contains("unknown directive `gamma::note`"), "{error}");
}

#[test]
fn a_stated_value_is_not_mistaken_for_a_misspelled_suppression() {
    // It shares the namespace and nothing else: it states the expression a return-value mutant
    // substitutes, and is read where mutants are made. Rejecting it here would make every file
    // that uses the feature fail before a single mutant was collected.
    let source = "#[gamma::value(7)]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn a_stated_value_written_as_a_comment_is_an_error_rather_than_silence() {
    // The comment form exists for statements and expressions, which cannot carry attributes. A
    // function can, so a value stated in a comment is a value nothing will ever read — and
    // silence would leave the author believing their site had the mutant they asked for.
    let source = "// #[gamma::value(7)]\nfn f(a: i32) -> i32 { a + 1 }";
    let error = directives(&file(source)).expect_err("a stated value in a comment is a usage error");

    assert!(error.to_string().contains("must be written as a real attribute"), "{error}");
}

#[test]
fn skipping_a_function_still_suppresses_the_value_it_states() {
    // The two attributes are answers to different questions — which mutant, and whether to run
    // it — so a site may carry both. Suppression is the one that has the last word.
    let source = "#[gamma::skip]\n#[gamma::value(7)]\nfn f() -> i32 { g() }";
    let (parsed, mut mutants) = mutants_of(source, "fn_value");

    assert_eq!(mutants.len(), 1, "{mutants:?}");
    assert_eq!(&*mutants[0].mutator, "fn_value.stated");

    let found = directives(&parsed).unwrap();
    let _ = suppress(&mut mutants, &found);

    assert!(mutants[0].suppression.is_some(), "{mutants:?}");
}

#[test]
fn an_unknown_mutants_attribute_is_still_ignored() {
    // `mutants` is another tool's namespace and has directives that mean nothing here, so
    // rejecting them would break a tree that is perfectly valid for the tool that owns them.
    let source = "#[mutants::note]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn a_cfg_attr_wrapped_directive_is_honored() {
    let source = "#[cfg_attr(test, gamma::skip)]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives(&file(source)).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].intent, Some(Intent::Skip));
}

#[test]
fn a_cfg_attr_whose_predicate_is_false_does_not_suppress_the_site() {
    let source = "#[cfg_attr(windows, gamma::skip)]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives_for(&file(source), &CfgSet::parse("unix\n")).unwrap();

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_cfg_attr_whose_predicate_is_unknown_still_suppresses_the_site() {
    let source = "#[cfg_attr(version(\"1.95\"), gamma::skip)]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives_for(&file(source), &CfgSet::parse("unix\n")).unwrap();

    assert_eq!(found.len(), 1);
}

#[test]
fn an_empty_all_predicate_applies_its_directive() {
    let source = "#[cfg_attr(all(), gamma::skip)]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives_for(&file(source), &CfgSet::parse("unix\n")).unwrap();

    assert_eq!(found.len(), 1);
}

#[test]
fn a_false_nested_cfg_attr_does_not_suppress_the_site() {
    let source = "#[cfg_attr(unix, cfg_attr(windows, gamma::skip))]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives_for(&file(source), &CfgSet::parse("unix\n")).unwrap();

    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_cfg_attr_wrapped_gamma_directive_keeps_its_arguments() {
    let source = "#[cfg_attr(feature = \"slow\", gamma::skip(arith, reason = \"why\"))]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives(&file(source)).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].reason.as_deref(), Some("why"));
}

#[test]
fn nested_cfg_attr_directives_keep_their_selectors_and_metadata() {
    let source = "#[cfg_attr(feature = \"outer\", cfg_attr(feature = \"inner\", gamma::skip(arith, reason = \"why\", tag = \"slow\", test_timeout_multiplier = 2.5)))]\nfn f(a: i32) -> i32 { a + 1 }";
    let found = directives(&file(source)).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].selectors, "arith");
    assert_eq!(found[0].reason.as_deref(), Some("why"));
    assert_eq!(found[0].tag.as_deref(), Some("slow"));
    assert_eq!(found[0].test_timeout_multiplier, Some(2.5));
}

#[test]
fn a_cfg_attr_wrapping_something_else_is_ignored() {
    let source = "#[cfg_attr(test, derive(Debug))]\nstruct S(i32);";

    assert!(directives(&file(source)).unwrap().is_empty());
}

/// A bare `#[cfg_attr]` with no argument list at all — no predicate, no attribute to apply —
/// carries nothing this can act on, and reaching for a predicate or attribute that is not
/// there must not panic; it has to be treated as though it named no directive.
#[test]
fn a_bare_cfg_attr_with_no_argument_list_is_ignored() {
    let source = "#[cfg_attr]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

/// `cfg_attr`'s argument list is a comma-separated sequence of meta items, and a token stream
/// that does not parse as one — a bare unbalanced expression — must be skipped instead of
/// failing the whole parse over one malformed attribute the compiler itself would also reject.
#[test]
fn cfg_attr_tokens_that_do_not_parse_as_a_meta_list_are_ignored() {
    let source = "#[cfg_attr(1)]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn malformed_nested_cfg_attr_tokens_are_ignored() {
    let source = "#[cfg_attr(test, cfg_attr(1))]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn trailing_tokens_after_named_values_are_rejected() {
    for source in [
        "#[gamma::skip(reason = \"why\" + unexpected)]\nfn f(a: i32) -> i32 { a + 1 }",
        "#[gamma::skip(tag = \"slow\" + unexpected)]\nfn f(a: i32) -> i32 { a + 1 }",
        "#[gamma::skip(test_timeout_multiplier = 2.0 + unexpected)]\nfn f(a: i32) -> i32 { a + 1 }",
        "// #[gamma::skip(reason = \"why\" + unexpected)]\nfn f(a: i32) -> i32 { a + 1 }",
    ] {
        let error = directives(&file(source)).expect_err("trailing named-value tokens are a usage error");

        assert!(error.is_usage(), "{error}");
        assert!(error.to_string().contains("trailing tokens"), "{error}");
    }
}

#[test]
fn unrelated_two_segment_attributes_are_ignored() {
    let source = "#[other::skip]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn single_segment_attributes_are_ignored() {
    let source = "#[gamma]\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn an_unrelated_comment_is_ignored() {
    let source = "// just explaining things\nfn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn comment_directives_without_argument_lists_select_everything() {
    let source = "// #[gamma::skip]\nfn f(a: i32, b: i32) -> i32 { a + b }";
    let found = directives(&file(source)).unwrap();

    assert!(found[0].selection.contains("arith.add_to_sub"));
    assert!(found[0].selectors.is_empty());
}

#[test]
fn a_deeply_nested_comment_directive_is_rejected_before_parsing() {
    let arguments = format!("{}arith{}", "(".repeat(100), ")".repeat(100));
    let source = format!("// #[gamma::skip({arguments})]\nfn f(a: i32) -> i32 {{ a + 1 }}");
    let error = directives(&file(&source)).expect_err("deep comment directive must be rejected");

    assert!(error.is_usage(), "{error}");
    assert!(error.to_string().contains("nests too deeply"), "{error}");
}

#[test]
fn extra_attributes_in_a_directive_comment_are_ignored() {
    let source = "// #[gamma::skip(arith)] #[cfg(unix)]\nfn f(a: i32, b: i32) -> i32 { a + b }";
    let found = directives(&file(source)).unwrap();

    assert_eq!(found.len(), 1);
    assert!(found[0].selection.contains("arith.add_to_sub"));
}

#[test]
fn the_shorthand_comment_spelling_is_ordinary_prose() {
    let source = "// gamma::skip(arith)\nfn f(a: i32, b: i32) -> i32 { a + b }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn a_misspelled_bracketed_comment_directive_is_an_error_rather_than_silence() {
    let source = "// #[gamma::skpi(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
    let error = directives(&file(source)).expect_err("a misspelled directive is a usage error");

    assert!(error.to_string().contains("unknown directive `gamma::skpi`"), "{error}");
}

#[test]
fn a_comment_that_opens_with_the_namespace_but_parses_as_nothing_is_an_error() {
    // Silence for this shape is never right: the comment announces itself as a directive.
    let source = "// #[gamma::skip(arith) and then some prose]\nfn f(a: i32) -> i32 { a + 1 }";
    let error = directives(&file(source)).expect_err("a malformed directive is a usage error");

    assert!(error.to_string().contains("test.rs:1"), "{error}");
}

#[test]
fn a_namespaced_comment_with_too_many_path_segments_is_an_error() {
    let source = "// #[gamma::skip::arith]\nfn f(a: i32) -> i32 { a + 1 }";
    let error = directives(&file(source)).expect_err("an unrecognized directive is a usage error");

    assert!(error.to_string().contains("is not a recognized directive"), "{error}");
}

#[test]
fn prose_that_merely_mentions_gamma_is_left_alone() {
    let source = "\
// gamma is what this project is called, and gamma::skip would suppress this
// see the gamma::skip docs for details
// #[derive(Debug)] is not a directive either
fn f(a: i32) -> i32 { a + 1 }";

    assert!(directives(&file(source)).unwrap().is_empty());
}

#[test]
fn expectation_directives_do_not_suppress() {
    let source = "#[gamma::expect_survived(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();

    assert_eq!(found[0].intent, Some(Intent::ExpectSurvived));
    assert_eq!(suppress(&mut mutants, &found), 0);
}

#[test]
fn an_expectation_is_recorded_on_every_mutant_it_governs() {
    let source = "#[gamma::expect_survived(arith, reason = \"deliberately untested\")]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let _suppressed = suppress(&mut mutants, &found);

    let expectation = mutants[0].expectation.as_ref().expect("the directive is recorded");

    assert!(!expectation.killed);
    assert_eq!(expectation.reason.as_deref(), Some("deliberately untested"));
}

#[test]
fn expect_killed_records_the_opposite_expectation() {
    let source = "#[gamma::expect_killed(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let _suppressed = suppress(&mut mutants, &found);

    assert!(mutants[0].expectation.as_ref().expect("the directive is recorded").killed);
}

#[test]
fn a_mutant_with_no_directive_carries_no_expectation() {
    let source = "fn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let _suppressed = suppress(&mut mutants, &found);

    assert!(mutants[0].expectation.is_none());
}

#[test]
fn suppressed_mutants_stay_in_the_population() {
    let source = "#[gamma::skip(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let before = mutants.len();
    let found = directives(&parsed).unwrap();
    let _ = suppress(&mut mutants, &found);

    assert_eq!(mutants.len(), before);
    assert!(mutants.iter().all(|mutant| mutant.outcome == Outcome::Ignored));
}

/// The whole point of the feature: a directive left behind after the code moved out from
/// under it must be named, or it reads forever as a live decision.
#[test]
fn a_skip_that_governs_nothing_is_named() {
    let source = "#[gamma::skip(arith, reason = \"stale\")]\nfn f(a: i32) -> bool { a > 1 }";
    let (parsed, mutants) = mutants_of(source, "arith,relational");
    let found = directives(&parsed).unwrap();
    let selection = Selection::parse("arith,relational").unwrap();
    let idle = idle("src/f.rs".into(), &mutants, &found, &selection);

    assert_eq!(idle.len(), 1, "{idle:?}");
    assert_eq!(idle[0].file, "src/f.rs");
    assert_eq!(idle[0].line, 1);
    assert_eq!(idle[0].selectors, "arith");
    assert_eq!(idle[0].reason.as_deref(), Some("stale"));
}

#[test]
fn a_skip_that_governs_a_mutant_is_not_named() {
    let source = "#[gamma::skip(arith)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let _ = suppress(&mut mutants, &found);
    let selection = Selection::parse("arith").unwrap();

    assert!(idle("src/f.rs".into(), &mutants, &found, &selection).is_empty());
}

/// The distinction the feature turns on. `--mutators relational` never offers an `arith` mutant to
/// anything, so every `skip(arith)` in the tree governs nothing — through no fault of its own.
/// Condemning those would make the report useless on any run that narrows the operators.
#[test]
fn a_skip_the_run_never_offered_a_mutant_is_not_named() {
    let source = "#[gamma::skip(arith)]\nfn f(a: i32) -> bool { a > 1 }";
    let (parsed, mutants) = mutants_of(source, "relational");
    let found = directives(&parsed).unwrap();
    let selection = Selection::parse("relational").unwrap();

    assert!(idle("src/f.rs".into(), &mutants, &found, &selection).is_empty(), "{found:?}");
}

/// A bare `skip` claims every operator, so it is in scope for whatever the run selected.
#[test]
fn a_skip_with_no_selectors_is_named_when_it_governs_nothing() {
    let source = "#[gamma::skip]\nfn f() {}\nfn g(a: i32) -> i32 { a + 1 }";
    let (parsed, mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();
    let selection = Selection::parse("arith").unwrap();

    assert_eq!(idle("src/f.rs".into(), &mutants, &found, &selection).len(), 1);
}

/// An expectation is a claim about a verdict, not a suppression, and the run already fails when
/// it is not met. Reporting it as unused would be reporting the same thing twice, in a form
/// that suggests deleting it.
#[test]
fn an_expectation_that_governs_nothing_is_not_named() {
    let source = "#[gamma::expect_killed(arith)]\nfn f(a: i32) -> bool { a > 1 }";
    let (parsed, mutants) = mutants_of(source, "arith,relational");
    let found = directives(&parsed).unwrap();
    let selection = Selection::parse("arith,relational").unwrap();

    assert!(idle("src/f.rs".into(), &mutants, &found, &selection).is_empty());
}

#[test]
fn a_file_whose_skips_all_still_apply_names_nothing() {
    let source = "#[gamma::skip(arith)]\nfn f(a: i32) -> i32 { a + 1 }\n#[gamma::skip(relational)]\nfn g(a: i32) -> bool { a > 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith,relational");
    let found = directives(&parsed).unwrap();
    let _ = suppress(&mut mutants, &found);
    let selection = Selection::parse("arith,relational").unwrap();

    assert!(idle("src/f.rs".into(), &mutants, &found, &selection).is_empty());
}

#[test]
fn gamma_attribute_sets_test_timeout_multiplier() {
    let source = "#[gamma(test_timeout_multiplier = 3.5)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].test_timeout_multiplier, Some(3.5));
    assert_eq!(found[0].intent, None);

    let _ = suppress(&mut mutants, &found);
    assert_eq!(mutants[0].test_timeout_multiplier, Some(3.5));
    assert_eq!(mutants[0].outcome, Outcome::Pending);
}

#[test]
fn gamma_attribute_with_positional_multiplier() {
    let source = "#[gamma(2.5)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].test_timeout_multiplier, Some(2.5));

    let _ = suppress(&mut mutants, &found);
    assert_eq!(mutants[0].test_timeout_multiplier, Some(2.5));
}

#[test]
fn gamma_test_timeout_multiplier_attribute_with_selector() {
    let source = "#[gamma::test_timeout_multiplier(arith, 4.0)]\nfn f(a: i32) -> bool { a + 1 > 0 }";
    let (parsed, mut mutants) = mutants_of(source, "arith,relational");
    let found = directives(&parsed).unwrap();

    let _ = suppress(&mut mutants, &found);
    for mutant in &mutants {
        if mutant.mutator.starts_with("arith.") {
            assert_eq!(mutant.test_timeout_multiplier, Some(4.0));
        } else {
            assert_eq!(mutant.test_timeout_multiplier, None);
        }
    }
}

#[test]
fn gamma_timeout_multiplier_comment_directive() {
    let source = "// #[gamma::test_timeout_multiplier(3.0)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].test_timeout_multiplier, Some(3.0));

    let _ = suppress(&mut mutants, &found);
    assert_eq!(mutants[0].test_timeout_multiplier, Some(3.0));
}

#[test]
fn expect_killed_with_timeout_multiplier() {
    let source = "#[gamma::expect_killed(arith, test_timeout_multiplier = 2.0)]\nfn f(a: i32) -> i32 { a + 1 }";
    let (parsed, mut mutants) = mutants_of(source, "arith");
    let found = directives(&parsed).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].intent, Some(Intent::ExpectKilled));
    assert_eq!(found[0].test_timeout_multiplier, Some(2.0));

    let _ = suppress(&mut mutants, &found);
    assert_eq!(mutants[0].test_timeout_multiplier, Some(2.0));
    assert!(mutants[0].expectation.as_ref().unwrap().killed);
}

#[test]
fn invalid_timeout_multiplier_is_rejected() {
    let source = "#[gamma::test_timeout_multiplier(-1.0)]\nfn f(a: i32) -> i32 { a + 1 }";
    let error = directives(&file(source)).unwrap_err();

    assert!(error.is_usage());
    assert!(
        error
            .to_string()
            .contains("timeout multiplier `-1.0` must be a number greater than zero"),
        "{error}"
    );
}

/// A directive multiplier must clear the same bound as the command line and the config file.
///
/// Without it the value reaches `Duration::mul_f64`, which panics on an unrepresentable result — so
/// a typo in a source comment would abort a completed build and baseline as an internal error
/// rather than being refused as bad input at parse time.
#[test]
fn an_unbounded_timeout_multiplier_is_refused_at_parse_time_in_both_directive_forms() {
    for source in [
        "#[gamma::test_timeout_multiplier(1e300)]\nfn f(a: i32) -> i32 { a + 1 }",
        "#[gamma::test_timeout_multiplier(test_timeout_multiplier = 1e300)]\nfn f(a: i32) -> i32 { a + 1 }",
    ] {
        let error = directives(&file(source)).unwrap_err();

        assert!(error.is_usage(), "{error}");
        assert!(
            error.to_string().contains("is unreasonably large"),
            "the positional and named forms must both reject it: {error}"
        );
    }
}

/// The bound must not narrow what a directive can legitimately ask for.
#[test]
fn a_reasonable_timeout_multiplier_still_survives_both_directive_forms() {
    for source in [
        "#[gamma::test_timeout_multiplier(2.5)]\nfn f(a: i32) -> i32 { a + 1 }",
        "#[gamma::test_timeout_multiplier(test_timeout_multiplier = 2.5)]\nfn f(a: i32) -> i32 { a + 1 }",
    ] {
        let mutants = directives(&file(source)).expect("a bounded multiplier is accepted");

        assert_eq!(mutants[0].test_timeout_multiplier, Some(2.5), "{source}");
    }
}

/// A directive may state one multiplier, so a second is refused rather than silently overriding
/// the first.
///
/// Keeping whichever arrived last leaves a directive that reads as though it says two things and
/// quietly does one, and it also split this channel from the proc-macro validator: the attribute
/// refused the same text at compile time, so adding or deleting the two `//` characters changed
/// whether the file built.
#[test]
fn a_second_timeout_multiplier_is_refused_in_every_spelling_and_order() {
    for arguments in [
        "2.0, 3.0",
        "2.0, arith, 3.0",
        "2.0, 3.0, 4.0",
        "factor = 2.0, multiplier = 3.0",
        "2.0, factor = 3.0",
        "factor = 2.0, 3.0",
        "test_timeout_multiplier = 2.0, arith, 3.0",
    ] {
        let source = format!("#[gamma::test_timeout_multiplier({arguments})]\nfn f(a: i32) -> i32 {{ a + 1 }}");
        let error = directives(&file(&source)).unwrap_err();

        assert!(error.is_usage(), "`{arguments}`: {error}");
        assert!(
            error.to_string().contains("a timeout multiplier is stated a second time"),
            "`{arguments}`: {error}"
        );
    }
}

/// Position carries no meaning: a multiplier written after its selectors states the same thing as
/// one written before them, and the proc-macro validator accepts exactly the same lists.
#[test]
fn a_positional_multiplier_is_read_wherever_in_the_argument_list_it_sits() {
    for arguments in ["2.5, arith", "arith, 2.5", "arith, 2.5,", "reason = \"slow\", 2.5"] {
        let source = format!("#[gamma::test_timeout_multiplier({arguments})]\nfn f(a: i32) -> i32 {{ a + 1 }}");
        let found = directives(&file(&source)).expect("a bounded multiplier is accepted");

        assert_eq!(found[0].test_timeout_multiplier, Some(2.5), "`{arguments}`");
    }
}
