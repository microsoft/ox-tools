// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Fixture-driven integration tests for `cargo anvil`.
//!
//! Each scenario lives under `tests/fixtures/<name>/`. The runner
//! copies the fixture into a temporary directory, invokes
//! `run_update`, and asserts the scenario-specific invariants.
//!
//! Complementary to the in-memory unit tests under `src/run.rs`
//! (which seed file contents inline). These fixtures are reviewable
//! by reading actual files on disk, which helps when designing new
//! migration paths or onboarding scenarios.

#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]
#![expect(
    clippy::panic,
    reason = "integration tests panic on unmet preconditions for readable failure output"
)]
#![expect(
    clippy::doc_markdown,
    reason = "fixture names like `opt-outs` look like code but are directory names"
)]

use std::path::{Path, PathBuf};

use cargo_anvil::test_support::{Cli, Decision, RunOutcome, Target, run_update};
use tempfile::TempDir;

const FIXTURES_ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Copy a fixture directory tree into a fresh tempdir and return the
/// tempdir handle (which deletes its contents on drop).
fn stage_fixture(name: &str) -> TempDir {
    let src: PathBuf = [FIXTURES_ROOT, "tests", "fixtures", name].iter().collect();
    assert!(src.is_dir(), "fixture {name} missing at {}", src.display());
    let tmp = TempDir::new().unwrap();
    copy_tree(&src, tmp.path());
    tmp
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry.unwrap();
        let rel = entry.path().strip_prefix(from).unwrap();
        let dest = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
        } else if entry.file_type().is_file() {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

fn local_only_args() -> Cli {
    Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force: false,
    }
}

fn run(tmp: &TempDir) -> RunOutcome {
    run_update(&cargo_anvil::Catalog::anvil(), &local_only_args(), tmp.path()).unwrap()
}

fn region_decision(outcome: &RunOutcome, host: &str, id: &str) -> Decision {
    outcome
        .plan
        .items()
        .iter()
        .find(|i| matches!(&i.target, Target::Region { host: h, id: rid } if h == host && rid == id))
        .unwrap_or_else(|| panic!("missing region item for {host}#{id}"))
        .decision
}

/// Read a TOML host anvil wrote and assert it **parses**.
///
/// Substring assertions are what let a broken host survive: a `deny.toml`
/// carrying two `[advisories]` headers contains every string these fixtures
/// look for and still fails the first `cargo deny` that reads it. Anything
/// anvil writes to a `.toml` host has to be a file TOML accepts.
fn read_parsing_toml(tmp: &TempDir, relpath: &str) -> String {
    let path = tmp.path().join(relpath);
    let text = std::fs::read_to_string(&path).unwrap();
    if let Err(error) = text.parse::<toml_edit::DocumentMut>() {
        panic!("{relpath} is not valid TOML: {error}\n---\n{text}\n---");
    }
    text
}

/// `single-crate`: a manifest with a bare `[package]` and no
/// `[workspace]` should still get the per-crate lints region (not the
/// workspace one), the Justfile imports region, and the full
/// justfiles/anvil/ tree.
#[test]
fn single_crate_emits_crate_lints_and_justfiles() {
    let tmp = stage_fixture("single-crate");
    run(&tmp);

    let cargo = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("anvil-managed: anvil-lints"),
        "single-crate fixture should receive the per-crate lints region; got:\n{cargo}"
    );
    assert!(
        !cargo.contains("anvil-workspace-lints"),
        "single-crate fixture must not receive the workspace lints region"
    );

    for rel in [
        "Justfile",
        "justfiles/anvil/mod.just",
        "justfiles/anvil/tools.just",
        "justfiles/anvil/helpers.just",
        "justfiles/anvil/checks/fmt.just",
        "justfiles/anvil/checks/miri.just",
        "justfiles/anvil/groups/pr-fast.just",
        "justfiles/anvil/groups/scheduled-exhaustive.just",
        "justfiles/anvil/tiers.just",
        "justfiles/anvil/versions.just",
    ] {
        assert!(tmp.path().join(rel).is_file(), "expected {rel} to be written");
    }

    // Idempotence: a second run must not change anything.
    let outcome2 = run(&tmp);
    assert!(
        !outcome2.plan.has_changes(),
        "second run should be a no-op; plan: {:#?}",
        outcome2.plan.items()
    );
}

/// `opt-outs`: a user who emptied the rustfmt managed region after a
/// first run keeps that opt-out across re-runs (LeaveAlone decision).
#[test]
fn empty_region_is_treated_as_opt_out() {
    use cargo_anvil::CommentSyntax;
    use cargo_anvil::test_support::{rustfmt_region_id, upsert_region};

    let tmp = stage_fixture("opt-outs");
    run(&tmp); // seed manifest and templates

    // Simulate the user emptying the managed region.
    let rustfmt_path = tmp.path().join("rustfmt.toml");
    let body = std::fs::read_to_string(&rustfmt_path).unwrap();
    let emptied = upsert_region(&body, rustfmt_region_id(), "", CommentSyntax::Hash).unwrap();
    std::fs::write(&rustfmt_path, &emptied).unwrap();

    // Re-run and check the rustfmt region is LeaveAlone.
    let outcome = run(&tmp);
    assert_eq!(region_decision(&outcome, "rustfmt.toml", rustfmt_region_id()), Decision::LeaveAlone);
    let after = std::fs::read_to_string(&rustfmt_path).unwrap();
    assert_eq!(after, emptied, "opt-out region must not be re-populated");
}

/// `customized`: a user edit inside a managed region with an unchanged
/// template should be left alone on subsequent runs.
#[test]
fn user_edit_inside_region_is_left_alone() {
    use cargo_anvil::CommentSyntax;
    use cargo_anvil::test_support::{rustfmt_region_id, upsert_region};

    let tmp = stage_fixture("customized");
    run(&tmp);

    let rustfmt_path = tmp.path().join("rustfmt.toml");
    let body = std::fs::read_to_string(&rustfmt_path).unwrap();
    let custom = upsert_region(&body, rustfmt_region_id(), "edition = \"2021\"\n", CommentSyntax::Hash).unwrap();
    std::fs::write(&rustfmt_path, custom).unwrap();

    let outcome = run(&tmp);
    assert_eq!(region_decision(&outcome, "rustfmt.toml", rustfmt_region_id()), Decision::LeaveAlone);
    let after = std::fs::read_to_string(&rustfmt_path).unwrap();
    assert!(
        after.contains("edition = \"2021\""),
        "user customization must be preserved; got:\n{after}"
    );
}

/// `deny-conflict`: a `deny.toml` whose hand-written `[advisories]` sets
/// `yanked` to something other than the managed body's value. No output keeps
/// both — TOML forbids the repeated key — so the region is refused, the host is
/// left byte-for-byte alone, and the rest of the onboarding still happens.
#[test]
fn a_conflicting_toml_host_is_refused_not_corrupted() {
    let tmp = stage_fixture("deny-conflict");

    let outcome = run(&tmp);

    let after = read_parsing_toml(&tmp, "deny.toml");
    assert!(
        after.contains("yanked = \"warn\""),
        "the repository's own value is never overwritten;\ngot:\n{after}"
    );
    assert!(
        !after.contains("anvil-deny-advisories"),
        "the conflicting region is not spliced in;\ngot:\n{after}"
    );
    assert_eq!(
        after.matches("[advisories]").count(),
        1,
        "and no duplicate header is produced;\ngot:\n{after}"
    );
    assert_eq!(
        region_decision(&outcome, "deny.toml", "anvil-deny-advisories"),
        Decision::LeaveAlone,
        "the conflicting region is planned as a no-op"
    );
    assert!(
        outcome.plan.refusals().iter().any(|reason| reason.contains("yanked")),
        "the refusal names the key that disagrees; got: {:#?}",
        outcome.plan.refusals()
    );

    // The refusal is scoped to the region it applies to. The rest of the host,
    // and the rest of the onboarding, still happens -- which is what makes
    // refusing tolerable rather than a wall.
    assert!(
        after.contains("anvil-deny-licenses"),
        "the non-conflicting sections are still written;\ngot:\n{after}"
    );
    assert!(
        tmp.path().join("justfiles/anvil/mod.just").is_file(),
        "other artifacts are still written"
    );
}

/// `migration`: a workspace that already has a hand-written
/// `Justfile`, a `[workspace.lints]` block, and a `deny.toml` should
/// get anvil's regions spliced in without losing any user content.
#[test]
fn migration_preserves_user_content() {
    let tmp = stage_fixture("migration");
    run(&tmp);

    let justfile = std::fs::read_to_string(tmp.path().join("Justfile")).unwrap();
    assert!(
        justfile.contains("my-custom-recipe"),
        "user-authored Justfile recipes must survive migration; got:\n{justfile}"
    );
    assert!(
        justfile.contains("anvil-imports"),
        "anvil imports region must be spliced into the existing Justfile"
    );

    let cargo = read_parsing_toml(&tmp, "Cargo.toml");
    assert!(
        cargo.contains("lto = \"thin\""),
        "user-authored [profile.release] must survive migration; got:\n{cargo}"
    );
    assert!(
        cargo.contains("anvil-workspace-lints"),
        "anvil workspace lints region must be spliced into Cargo.toml"
    );

    // The defect this fixture used to hide: the hand-written `[advisories]`
    // declares an `ignore` list the managed body does not, so adoption cannot
    // simply delete the table. Appending the region regardless produced a
    // second `[advisories]` header, which TOML rejects outright -- and every
    // assertion below still passed, because they only ever looked for a
    // substring of its text.
    let deny = read_parsing_toml(&tmp, "deny.toml");
    assert!(
        deny.contains("RUSTSEC-9999-0001"),
        "user-authored deny.toml content must survive migration; got:\n{deny}"
    );
    assert!(deny.contains("anvil-deny"), "anvil deny region must be spliced into deny.toml");
    assert_eq!(
        deny.matches("[advisories]").count(),
        1,
        "the hand-written table must be adopted, not duplicated; got:\n{deny}"
    );
    // Kept configuration has to land *inside* the table the region opens, or
    // it silently changes meaning -- a relocated `ignore` that ends up under
    // `[bans]` is a different setting that cargo-deny will not honor.
    let advisories = deny.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        advisories["advisories"]["ignore"].as_array().unwrap().len(),
        1,
        "the user's accepted advisory must still be an [advisories] entry; got:\n{deny}"
    );

    // Idempotence: re-run leaves everything alone.
    let outcome2 = run(&tmp);
    assert!(
        !outcome2.plan.has_changes(),
        "second migration run should be a no-op; plan: {:#?}",
        outcome2.plan.items()
    );
}
