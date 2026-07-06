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

    let cargo = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("lto = \"thin\""),
        "user-authored [profile.release] must survive migration; got:\n{cargo}"
    );
    assert!(
        cargo.contains("anvil-workspace-lints"),
        "anvil workspace lints region must be spliced into Cargo.toml"
    );

    let deny = std::fs::read_to_string(tmp.path().join("deny.toml")).unwrap();
    assert!(
        deny.contains("RUSTSEC-9999-0001"),
        "user-authored deny.toml content must survive migration; got:\n{deny}"
    );
    assert!(deny.contains("anvil-deny"), "anvil deny region must be spliced into deny.toml");

    // Idempotence: re-run leaves everything alone.
    let outcome2 = run(&tmp);
    assert!(
        !outcome2.plan.has_changes(),
        "second migration run should be a no-op; plan: {:#?}",
        outcome2.plan.items()
    );
}
