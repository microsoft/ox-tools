// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Regressions in the published documentation and sibling runtime policy.

use std::fs;

use super::{root_dir, workspace_dir};

fn published_document(relative: &str) -> String {
    fs::read_to_string(root_dir().join(relative)).expect("the document is part of the checkout")
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
fn issue_020_the_documented_option_names_are_the_ones_that_exist() {
    // The README and the advice both named `--exclude`, which the tool does not accept. Advice
    // nobody can follow is worse than none: it is followed, it fails, and the tool is blamed.
    let readme = published_document("README.md");

    assert!(readme.contains("--exclude-file"), "the real option is undocumented");
    assert!(
        !readme.contains("`--exclude`"),
        "the option that does not exist is documented again"
    );
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
    let source = fs::read_to_string(workspace_dir().join("crates/cargo-gamma-rt/src/runtime.rs")).expect("the runtime source");
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

    let manifest = fs::read_to_string(workspace_dir().join("crates/cargo-gamma-rt/Cargo.toml")).expect("the runtime manifest");
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
