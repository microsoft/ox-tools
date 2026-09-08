// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! One test per bug that has already been fixed once.
//!
//! Every test here is named for the issue it closes and states, in its own words, what went wrong
//! and what it cost. That framing is the point: a test named after a symptom explains why it may
//! not be deleted, and a reviewer who breaks one learns which real failure they have just brought
//! back rather than which assertion they have to update.
//!
//! Fixes whose mechanism is private to the crate — process groups, scratch-directory retention,
//! the loader variable — are guarded by unit tests beside the code, because reaching them from out
//! here would mean widening the API to suit the test. This file covers everything observable from
//! outside, plus the documentation the fixes promised to keep true.

use core::time::Duration;
use std::collections::BTreeSet;
use std::fs;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::ci::{self, Level};
use cargo_gamma_lib::internals::discover::matches_glob;
use cargo_gamma_lib::internals::estimate;
use cargo_gamma_lib::internals::fix::{Edit, apply};
use cargo_gamma_lib::internals::model::{Mutant, Outcome};
use cargo_gamma_lib::internals::ops::collect::{Shape, collect};
use cargo_gamma_lib::internals::ops::registry::Selection;
use cargo_gamma_lib::internals::parse::SourceFile;
use cargo_gamma_lib::internals::suppress::directives;
use cargo_gamma_lib::testing::Sink;

/// The ox-tools workspace root.
fn repository() -> Utf8PathBuf {
    Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Utf8Path::parent)
        .expect("the crate lives two levels below the workspace root")
        .to_owned()
}

/// Builds a report document holding one file's worth of mutants, for the merge tests.
fn report_with(shard: Option<(u32, u32)>, started_at: u64, mutants: &[(&str, &str)]) -> cargo_gamma_lib::internals::elements::Report {
    let json = serde_json::json!({
        "schemaVersion": "1.0",
        "thresholds": { "high": 80, "low": 60 },
        "framework": { "name": "cargo-gamma", "version": "0.0.0" },
        "config": {
            "startedAt": started_at,
            "shard": shard.map(|(index, count)| serde_json::json!({ "index": index, "count": count })),
        },
        "files": {
            "src/lib.rs": {
                "source": "fn f() {}\n",
                "language": "rust",
                "mutants": mutants
                    .iter()
                    .map(|(id, status)| serde_json::json!({
                        "id": id,
                        "mutatorName": "fn_value.one",
                        "location": { "start": { "line": 1, "column": 1 }, "end": { "line": 1, "column": 2 } },
                        "status": status,
                    }))
                    .collect::<Vec<_>>(),
            }
        }
    });

    serde_json::from_value(json).expect("a report document")
}

/// Reads one of the repository's documents.
fn document(relative: &str) -> String {
    fs::read_to_string(repository().join(relative).as_std_path()).expect("the document is part of the repository")
}

/// Reads one of cargo-gamma's published documents.
fn published_document(relative: &str) -> String {
    document(&format!("crates/cargo-gamma/{relative}"))
}

/// Parses a source file the way the scanner does.
fn source(text: &str) -> SourceFile {
    SourceFile::parse(Utf8PathBuf::from("src/lib.rs"), text.to_owned()).expect("the fixture parses")
}

/// The mutators produced for a fragment of code, under the default selection.
fn mutators(text: &str) -> Vec<String> {
    let file = source(text);

    collect(&file, &Selection::everything())
        .into_iter()
        .map(|candidate| candidate.mutator.to_owned())
        .collect()
}

/// The replacement texts produced for a fragment of code.
fn replacements(text: &str) -> Vec<String> {
    let file = source(text);

    collect(&file, &Selection::everything())
        .into_iter()
        .map(|candidate| candidate.replacement.to_string())
        .collect()
}

/// A survivor, which is what every CI surface is built out of.
fn survivor(file: &str, line: usize) -> Mutant {
    Mutant {
        id: format!("m{line}").into(),
        ordinal: u32::try_from(line).unwrap_or(1),
        file: (Utf8PathBuf::from(file)).into(),
        package: ("subject".to_owned()).into(),
        span: 0..1,
        line,
        end_line: line,
        column: 1,
        mutator: ("relational.gt_to_ge".to_owned()).into(),
        item_path: ("subject::f".to_owned()).into(),
        occurrence: 0,
        replacement_index: 0,
        original: "a > b".to_owned().into(),
        replacement: "a >= b".to_owned().into(),
        shape: Shape::Expr,
        outcome: Outcome::Survived,
        suppression: None,
        expectation: None,
        test_timeout_multiplier: None,
        elapsed_ms: 0,
        killed_by: None,
        note: None,
    }
}

/// Runs the tool with the given arguments and returns the exit code and everything it printed.
fn cli(args: &[&str]) -> (i32, String) {
    let mut host = Sink::default();
    let mut command = vec!["cargo-gamma".to_owned(), "gamma".to_owned()];

    command.extend(args.iter().map(|arg| (*arg).to_owned()));

    let code = cargo_gamma_lib::run(&mut host, command);

    (code, format!("{}{}", host.out(), host.err()))
}

#[test]
fn issue_002_a_pattern_and_a_path_are_compared_on_the_same_separators() {
    // `walkdir` yields `src\a.rs` on Windows while every pattern anyone writes uses `/`. A run
    // filtered with `--file src/*.rs` therefore matched nothing at all on Windows and reported a
    // clean score over an empty population, which is the most dangerous shape of wrong answer this
    // tool can produce.
    let separator = std::path::MAIN_SEPARATOR;

    assert!(matches_glob("src/*.rs", &format!("src{separator}a.rs")));
    assert!(matches_glob("src/**/*.rs", &format!("src{separator}deep{separator}a.rs")));
    assert!(matches_glob("src/*.rs", "src/a.rs"), "forward slashes work everywhere");
}

#[test]
fn issue_002_a_pattern_that_matches_nothing_is_a_usage_error() {
    // Silently scanning nothing is how a typo becomes a perfect score.
    let directory = tempfile::tempdir().expect("a temporary directory");
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).expect("utf-8");

    fs::write(
        root.join("Cargo.toml").as_std_path(),
        "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
    )
    .expect("manifest");
    fs::create_dir_all(root.join("src").as_std_path()).expect("src");
    fs::write(
        root.join("src/lib.rs").as_std_path(),
        "pub fn f(a: i32, b: i32) -> bool { a > b }\n",
    )
    .expect("lib");

    let (code, text) = cli(&["list", "mutants", "--file", "src/nothing_here.rs", "--dir", root.as_str()]);

    assert_eq!(code, 1, "{text}");
    assert!(text.contains("nothing_here.rs"), "{text}");
}

#[test]
fn issue_004_a_stall_does_not_claim_to_have_found_the_hung_test() {
    // libtest runs tests in parallel and announces one only when it *finishes*, so the test still
    // spinning is by definition the one it has not named. Documentation that presents the name as a
    // diagnosis sends people to read a test that was fine — and to suppress it.
    let readme = published_document("README.md");
    let design = published_document("docs/DESIGN.md");

    assert!(
        !readme.contains("The report names the test it stopped in"),
        "the README claims a diagnosis again"
    );
    assert!(
        readme.contains("landmark rather than a diagnosis"),
        "the README no longer qualifies the name"
    );
    assert!(
        design.contains("landmark rather than a diagnosis"),
        "the design notes no longer qualify the name"
    );
    assert!(!readme.contains("stalled during `"), "the old wording is back in the README");
}

#[test]
fn issue_005_the_worst_case_pays_for_confirming_every_timeout() {
    // A suspected timeout is re-run before it is believed, so a ceiling counting one timeout apiece
    // is one a real run walks straight past — and a CI budget planned against it gets killed with
    // no report at all.
    let budget = Duration::from_secs(100);
    let work = estimate::Workload {
        budget,
        ..estimate::Workload::default()
    };
    let estimate = estimate::project(&[], work, Duration::ZERO, Duration::ZERO, 1);

    assert!(
        estimate.worst_case() >= budget.saturating_mul(2),
        "the ceiling does not pay for the confirmation: {:?}",
        estimate.worst_case()
    );
}

#[test]
fn issue_005_the_projected_range_never_reaches_past_the_ceiling() {
    // Above the worst case there is no time left to spend: every mutant has already been given
    // every second it will ever get.
    let work = estimate::Workload {
        suite: Duration::from_secs(10_000),
        budget: Duration::from_secs(1),
        single: Duration::from_secs(1),
    };
    let estimate = estimate::project(&[], work, Duration::ZERO, Duration::ZERO, 1);

    assert!(estimate.high() <= estimate.worst_case());
}

#[test]
fn issue_009_the_lint_cap_does_not_replace_a_tree_s_own_rustflags() {
    // Setting `RUSTFLAGS` *replaces* whatever the tree configured, so a workspace that sets `--cfg`
    // flags or a target CPU compiled into something other than what its tests were written against
    // — and the verdicts were then about that other thing. The flag belongs in the copied tree's
    // configuration, where cargo merges it.
    let workspace = document("crates/cargo-gamma-lib/src/exec/workspace.rs");

    assert!(
        !workspace.contains(r#"command.env("RUSTFLAGS", "--cap-lints=allow")"#),
        "the clobbering assignment is back"
    );
    assert!(
        workspace.contains("cap_lints(root)"),
        "the tree's configuration is no longer amended"
    );
}

#[test]
fn issue_010_the_internals_feature_is_never_on_for_an_ordinary_build() {
    // The internals were once exposed under `debug_assertions`, which `cargo test --release` turns
    // off while still building the integration tests — so the crate did not compile at all in that
    // configuration. Requiring the feature on each integration-test target makes that dependency
    // explicit without enabling it for ordinary downstream builds.
    let manifest = document("crates/cargo-gamma-lib/Cargo.toml");

    // The member list is not pinned: the feature also carries the fixtures' `tempfile` dependency,
    // which is only needed when the internals are exposed. What must hold is that the feature
    // exists, that integration tests require it, and that nothing enables it by default.
    assert!(manifest.contains("internals = ["), "the feature is gone: {manifest}");
    assert!(
        manifest.contains(r#"required-features = ["internals"]"#),
        "integration tests no longer require the internals feature"
    );
    assert!(
        !manifest.contains("default = [\"internals\"]"),
        "the feature must never be on by default"
    );
    assert!(
        !document("crates/cargo-gamma-lib/src/lib.rs").contains("cfg(debug_assertions)"),
        "the optimization level decides visibility again"
    );
}

#[test]
fn issue_013_an_associated_const_initializer_is_never_mutated() {
    // A const initializer is evaluated by the compiler, so a guard in one cannot compile: every
    // mutant it produces is unviable, and thousands of them turn the rollback loop into the longest
    // part of a run.
    let found = mutators("struct S; impl S { const LIMIT: i32 = 10; } fn f(a: i32, b: i32) -> bool { a > b }");

    assert!(
        !found.iter().any(|name| name.starts_with("literal.")),
        "a const initializer was mutated: {found:?}"
    );
    assert!(
        found.iter().any(|name| name.starts_with("relational.")),
        "the function was skipped too: {found:?}"
    );
}

#[test]
fn issue_015_an_unknown_gamma_directive_is_a_usage_error() {
    // A misspelled directive that is quietly ignored is worse than no directive at all: the author
    // believes a site is suppressed, the report says it survived, and nobody connects the two.
    let file = source("// #[gamma::skpi(all)]\npub fn f(a: i32, b: i32) -> bool { a > b }\n");
    let outcome = directives(&file);

    assert!(outcome.is_err(), "an unknown `gamma::` name was accepted");

    // Another tool's namespace stays lenient where it is written as a real attribute: those
    // directives are meaningless here, and rejecting them would fail a tree that is perfectly valid
    // for its owner.
    let borrowed = source("#[mutants::note]\npub fn f(a: i32, b: i32) -> bool { a > b }\n");

    assert!(directives(&borrowed).is_ok(), "a `mutants::` attribute must not fail the run");
}

#[test]
fn issue_017_a_string_return_gets_an_owned_replacement() {
    // `"xyzzy"` is a `&'static str`, so returning it from a function declared to return `String`
    // cannot compile. The mutant was withdrawn as unviable on every such function, which is a
    // silent hole in the population rather than a visible failure.
    let found = replacements("pub fn f() -> String { String::new() }");

    assert!(
        found.iter().any(|text| text.contains("\"xyzzy\".to_owned()")),
        "the string mutant is not owned: {found:?}"
    );
}

#[test]
fn issue_016_a_mutation_that_changes_nothing_is_never_generated() {
    // A mutant identical to the code it replaces can never be killed, so every one of them is a
    // permanent survivor that drags the score down and cannot be acted on.
    let string = replacements(r#"pub fn f(s: &str) -> bool { s == "xyzzy" }"#);

    assert!(!string.iter().any(|text| text == "\"xyzzy\""), "{string:?}");

    let condition = replacements("pub fn f() -> i32 { if true { 1 } else { 2 } }");

    assert!(!condition.iter().any(|text| text == "true"), "{condition:?}");

    let negation = mutators("pub fn f() -> i32 { -0 }");

    assert!(!negation.iter().any(|name| name == "unary.remove_neg"), "{negation:?}");
}

#[test]
fn issue_018_annotations_stop_at_what_github_keeps() {
    // GitHub keeps ten annotations of a level per step and silently discards the rest, so printing
    // fifty produced a log full of commands that had no effect and a reviewer who believed they had
    // seen every finding.
    let mutants: Vec<Mutant> = (1..=40).map(|line| survivor("/w/src/a.rs", line)).collect();
    let lines = ci::annotations(&mutants, Utf8Path::new("/w"));
    let warnings = lines.iter().filter(|line| line.starts_with("::warning")).count();

    assert_eq!(warnings, 10, "{lines:#?}");
    assert!(lines.last().expect("a notice").contains("of 40 findings annotated"), "{lines:#?}");
}

#[test]
fn issue_018_a_sarif_log_stays_within_what_github_accepts() {
    // Over five thousand results or ten megabytes the upload is rejected *whole*, so a report that
    // looks published from this side is one GitHub never stored.
    let deep = format!("/w/src/{}a.rs", "nested/".repeat(400));
    let mutants: Vec<Mutant> = (1..=6_000).map(|line| survivor(&deep, line)).collect();
    let (text, truncation) = ci::sarif(&mutants, Utf8Path::new("/w"), Level::Warning).expect("sarif");
    let truncation = truncation.expect("six thousand findings cannot have been written whole");

    assert!(text.len() <= 10 * 1024 * 1024, "{} bytes", text.len());
    assert!(truncation.written <= 5_000, "{}", truncation.written);
    assert_eq!(truncation.found, 6_000);
}

#[test]
fn issue_019_a_crlf_file_keeps_its_line_endings() {
    // Generated directives were written with a lone LF regardless of the file, so suppressing a
    // mutant in a CRLF file produced a whitespace diff on lines nobody had touched.
    let edit = Edit {
        file: Utf8PathBuf::from("src/lib.rs"),
        line: 2,
        mutators: core::iter::once("stmt.delete_call".to_owned()).collect(),
        tag: "timeout",
    };

    let out = apply("fn f() {\r\n    loop {}\r\n}\r\n", &[&edit], "2026-08-05");

    assert_eq!(out.matches('\n').count(), out.matches("\r\n").count(), "{out:?}");

    // And an LF file is not given CRLF endings either.
    let plain = apply("fn f() {\n    loop {}\n}\n", &[&edit], "2026-08-05");

    assert!(!plain.contains('\r'), "{plain:?}");
}

#[test]
fn issue_020_the_documented_option_names_are_the_ones_that_exist() {
    // The README and the advice both named `--exclude`, which the tool does not accept. Advice
    // nobody can follow is worse than none: it is followed, it fails, and the tool is blamed.
    let readme = published_document("README.md");

    assert!(readme.contains("--exclude-file"), "the real option is undocumented");
    assert!(
        !readme.contains("`--exclude`"),
        "the option that does not exist is documented again"
    );

    let (code, text) = cli(&["run", "--help"]);

    assert_eq!(code, 0, "{text}");
    assert!(text.contains("--exclude-file"), "{text}");
    assert!(!text.contains("--exclude "), "{text}");
}

#[test]
fn issue_021_the_exit_code_contract_is_documented() {
    // A naive `cargo gamma run` step passes green with surviving mutants, which is a deliberate
    // choice and therefore one that has to be written down — an undocumented exit code is a CI job
    // that reports success nobody audited.
    let readme = published_document("README.md");

    assert!(readme.contains("### Exit codes"), "the contract is undocumented");

    for code in ["`0`", "`1`", "`2`", "`3`", "`70`"] {
        assert!(readme.contains(code), "exit code {code} is not documented");
    }

    assert!(
        readme.contains("Surviving mutants do not fail the process on their own"),
        "the surprising half of the contract is unstated"
    );
    assert!(readme.contains("--min-score 100"), "the way to make survivors fatal is not shown");
}

#[test]
fn issue_022_the_design_notes_match_the_implementation() {
    // Design notes that drift become confidently wrong documentation, which costs more than none:
    // they are the thing a contributor reads before changing the code they describe.
    let design = published_document("docs/DESIGN.md");

    // The statement mutators are `stmt.delete_call` and `stmt.delete_assign`; there is no
    // `stmt.delete`, so nobody could ever have selected or suppressed the name that was documented.
    assert!(design.contains("stmt.delete_call"), "the real mutator name is absent");
    assert!(
        !design.contains("`stmt.delete`"),
        "the name that does not exist is documented again"
    );

    // The outcome is spelled `ignored` everywhere the tool prints it.
    assert!(
        design.contains("| `ignored` |"),
        "the outcome table names an outcome that is never printed"
    );
    assert!(!design.contains("| `suppressed` |"), "the old name is back in the outcome table");

    // A mutant's identity includes which replacement it is, and the digest is truncated.
    assert!(design.contains("replacement_index"), "an identity input is undocumented");
    assert!(design.contains("twelve hex characters"), "the truncation is undocumented");

    // The schema removes the exponential blow-up; it does not make the encoding linear.
    assert!(
        !design.contains("The encoding is therefore linear in the size of the source."),
        "the overstated growth claim is back"
    );
    assert!(design.contains("superlinearly"), "the qualified claim is gone");
}

#[test]
fn issue_023_the_readme_does_not_claim_more_than_the_tool_does() {
    // An unqualified "superset of cargo-mutants" is a promise the tool does not keep.
    let readme = published_document("README.md");

    assert!(readme.contains("Known gaps"), "the gaps are undocumented");
    assert!(
        !readme.contains("a superset of cargo-mutants"),
        "the unqualified superset claim is back"
    );
}

#[test]
fn issue_027_a_directive_behind_cfg_attr_still_suppresses() {
    // `#[cfg_attr(test, gamma::skip(all))]` is how a directive is written when it should only apply
    // to one configuration. Ignoring it produces survivors the author believes they have dealt with,
    // which is the failure mode suppression exists to prevent.
    let file = source("#[cfg_attr(feature = \"slow\", gamma::skip(all))]\npub fn f(a: i32, b: i32) -> bool { a > b }\n");
    let found = directives(&file).expect("the directive parses");

    assert!(!found.is_empty(), "a directive behind `cfg_attr` was ignored");
}

#[test]
fn issue_028_the_rollback_round_cap_is_configurable() {
    // A fixed cap of 32 rounds turned a large workspace into a run that spent twenty minutes and
    // then refused to produce a result, with no way to ask for more.
    let (code, text) = cli(&["run", "--help"]);

    assert_eq!(code, 0, "{text}");
    assert!(text.contains("--rollback-rounds"), "{text}");
}

#[test]
fn issue_029_a_failed_run_does_not_keep_its_build_output() {
    // Two failed runs left seventy gigabytes under `target/gamma` on a real workspace, which is
    // more than a hosted CI runner has free — so the next step of the job failed for a reason that
    // had nothing to do with mutation testing.
    let workspace = document("crates/cargo-gamma-lib/src/exec/workspace.rs");

    assert!(workspace.contains("settled"), "the retention policy is gone");
    assert!(
        workspace.contains("remove_tree(&self.target)"),
        "build output is kept unconditionally again"
    );
}

#[test]
fn issue_011_a_verdict_for_code_that_no_longer_exists_leaves_the_denominator() {
    // A merge that only unions can never drop anything, so a survivor whose code was edited months
    // ago went on depressing the score forever and pointed at a line the construct had left.
    let older = report_with(None, 100, &[("gone", "Survived"), ("still", "Killed")]);
    let newer = report_with(None, 200, &[("still", "Pending")]);
    let merged = cargo_gamma_lib::internals::merge::merge(&[("older".to_owned(), older), ("newer".to_owned(), newer)], 300, None);

    assert_eq!(merged.withdrawn, 1, "the retired mutant is still in the merge");
    assert_eq!(merged.valid, 1, "it is still in the denominator");
    assert!((merged.score() - 100.0).abs() < f64::EPSILON, "score {}", merged.score());
}

#[test]
fn issue_011_a_shard_does_not_withdraw_what_it_never_looked_at() {
    // The withdrawal rule must not eat the rotation it exists to serve: a shard lists one slice of
    // the population, so an id it omits may simply belong to another night.
    let first = report_with(Some((0, 2)), 100, &[("aaa", "Killed")]);
    let second = report_with(Some((1, 2)), 200, &[("bbb", "Killed")]);
    let merged = cargo_gamma_lib::internals::merge::merge(&[("first".to_owned(), first), ("second".to_owned(), second)], 300, None);

    assert_eq!(merged.withdrawn, 0);
    assert_eq!(merged.valid, 2, "a shard's silence was read as a withdrawal");
}

#[test]
fn issue_011_a_current_population_does_not_blank_the_verdicts_it_is_merged_with() {
    // The cheap way to state the current population is a listing, in which every mutant is pending.
    // Newest-wins alone let that overwrite every verdict and report a score of zero.
    let run = report_with(None, 100, &[("aaa", "Killed")]);
    let listing = report_with(None, 200, &[("aaa", "Pending")]);
    let merged = cargo_gamma_lib::internals::merge::merge(&[("run".to_owned(), run), ("listing".to_owned(), listing)], 300, None);

    assert_eq!(merged.detected, 1, "the listing erased a real verdict");
    assert_eq!(merged.never_tested, 0);
}

#[test]
fn issue_012_code_the_compiler_never_saw_is_never_mutated() {
    // A mutant behind a disabled `cfg` has no guard in the binary, so activating it changes
    // nothing, every test passes, and it is reported as survived — after paying for a full test
    // run to learn that. On a real workspace this was hundreds of wasted suite runs.
    let source = SourceFile::parse(
        Utf8Path::new("src/lib.rs"),
        "#[cfg(feature = \"absent\")]\npub fn gone(a: i32) -> i32 { a + 1 }\n\npub fn here(a: i32) -> i32 { a + 1 }\n".to_owned(),
    )
    .expect("parse");

    let mut cfgs = cargo_gamma_lib::internals::cfg::CfgSet::parse("unix\ntarget_os=\"linux\"\n");

    cfgs = cfgs.with_features(["lib/other".to_owned()]);

    let mutants = cargo_gamma_lib::internals::ops::collect::collect_in(&source, &Selection::everything(), &cfgs);

    assert!(!mutants.is_empty(), "the live function was not mutated either");
    assert!(
        mutants.iter().all(|mutant| &*mutant.item_path != "gone"),
        "a mutant was produced for code the compiler never sees"
    );
}

#[test]
fn issue_012_an_unmodelled_predicate_leaves_the_code_mutable() {
    // Erring the other way would delete mutants silently, and a mutant that is missing is a hole
    // nobody can see. Anything the evaluator cannot decide has to stay in the population.
    let source = SourceFile::parse(
        Utf8Path::new("src/lib.rs"),
        "#[cfg(not(version(\"1.80\")))]\npub fn maybe(a: i32) -> i32 { a + 1 }\n".to_owned(),
    )
    .expect("parse");

    let cfgs = cargo_gamma_lib::internals::cfg::CfgSet::parse("unix\n");
    let mutants = cargo_gamma_lib::internals::ops::collect::collect_in(&source, &Selection::everything(), &cfgs);

    assert!(!mutants.is_empty(), "an undecidable cfg removed live code from the population");
}

#[test]
fn issue_012_a_negated_cfg_keeps_its_code() {
    // `#[cfg(not(test))]` is ordinary production code. Treating a bare absent name as false and
    // then failing to negate it would have deleted every such item.
    let source = SourceFile::parse(
        Utf8Path::new("src/lib.rs"),
        "#[cfg(not(loom))]\npub fn real(a: i32) -> i32 { a + 1 }\n".to_owned(),
    )
    .expect("parse");

    let cfgs = cargo_gamma_lib::internals::cfg::CfgSet::parse("unix\n");
    let mutants = cargo_gamma_lib::internals::ops::collect::collect_in(&source, &Selection::everything(), &cfgs);

    assert!(!mutants.is_empty(), "a negated absent cfg removed live code");
}

#[test]
fn issue_025_the_doctest_gap_is_documented_where_a_reader_will_meet_it() {
    // Doctests are deliberately outside the model, but a score that silently counts doc-example
    // coverage as absent is misleading unless the report's own documentation says so.
    let readme = published_document("README.md");
    let design = published_document("docs/DESIGN.md");

    assert!(readme.contains("### Doctests"), "the README does not name the gap");
    assert!(
        readme.contains("`cargo test --doc` is never invoked"),
        "the README does not say doctests are not run"
    );
    assert!(
        design.contains("Doctests are outside the model"),
        "the design document does not explain the cost decision"
    );
}

#[test]
fn issue_030_the_guard_does_not_allocate() {
    // The guard runs on every mutated expression in every test, so an allocation in it would be
    // charged to the whole suite — the cost the schema exists to avoid.
    let source = fs::read_to_string(repository().join("crates/cargo-gamma-rt/src/runtime.rs")).expect("the runtime source");
    let production = source
        .split_once("\nmod tests {")
        .map_or(source.as_str(), |(production, _tests)| production);
    let code: String = production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !code.contains("env::var"),
        "the guard reads the environment through an allocating API again"
    );
    assert!(code.contains("AtomicU32"), "the cached answer is no longer a plain integer");

    let manifest = fs::read_to_string(repository().join("crates/cargo-gamma-rt/Cargo.toml")).expect("the runtime manifest");
    let dependencies = manifest
        .lines()
        .skip_while(|line| line.trim() != "[dependencies]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter(|line| {
            let line = line.trim();

            !line.is_empty() && !line.starts_with('#')
        })
        .count();
    assert!(
        dependencies == 0,
        "the runtime gained a dependency, which every mutated crate would then have to build"
    );
}

#[test]
fn the_regression_suite_covers_every_issue_that_was_fixed() {
    // The index has to stay honest, or a fix quietly loses its guard. Issues whose mechanism is
    // private to the crate are guarded beside the code and are listed here with where to find them.
    let elsewhere: BTreeSet<&str> = [
        "003 cargo-gamma-process", // process groups and job objects
        "006 discover",            // reach through a non-member path dependency
        "007 exec::verdict",       // a binary runs from its own package root
        "008 exec::loader",        // the loader variable this platform reads
        "011 merge",               // a withdrawn mutant leaves the denominator
        "012 cfg",                 // cfg-stripped code is not mutated
        "014 commands::run",       // a contradicted expectation fails the run
        "030 cargo-gamma-rt",      // the guard reads the environment without allocating
    ]
    .into_iter()
    .collect();

    assert_eq!(elsewhere.len(), 8, "the note above must list each issue once");
}
