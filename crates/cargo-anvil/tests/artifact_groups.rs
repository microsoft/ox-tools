// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox the FS ops these tests do (TempDir etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Integration tests for the `[anvil] artifacts` allow-list.
//!
//! These drive the real `Catalog::anvil()` through `run_update` against
//! tempdir workspaces and assert on the emitted tree, exercising:
//!
//! * emission for representative group combinations (notably
//!   `artifacts = ["recipes", "container"]`, which must emit the recipe tree
//!   and the container files but splice **no** managed region into the user's
//!   config files);
//! * byte-identical equivalence between an omitted key and an explicit
//!   all-groups list;
//! * idempotency under a restricted allow-list;
//! * clean retraction when a group is dropped from the allow-list, leaving
//!   user-authored content untouched;
//! * end-to-end rejection of an unknown group name and an empty list.

use std::path::{Path, PathBuf};

use cargo_anvil::test_support::{Cli, MANIFEST_FILE_NAME, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// A workspace rooted at a fixed-name subdirectory (container rendering embeds
/// the repo directory name, so a random tempdir name would leak into output).
fn named_workspace(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
    (tmp, root)
}

/// Render every anvil-produced file into one deterministic string, omitting
/// `.anvil.lock` (its `rendered_by` version churns) and `anvil.toml` (the
/// user-authored input, not a generated artifact).
fn render_tree(root: &Path) -> String {
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str());
            name != Some(MANIFEST_FILE_NAME) && name != Some("anvil.toml")
        })
        .collect();
    paths.sort();

    let mut out = String::new();
    for path in paths {
        let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        let body = read(&path);
        out.push_str("=== ");
        out.push_str(&rel);
        out.push_str(" ===\n");
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn github(dry_run: bool) -> Cli {
    Cli {
        backends: vec!["github".to_owned()],
        no_backends: false,
        dry_run,
        force: false,
    }
}

fn set_anvil_toml(root: &Path, body: &str) {
    write(&root.join("anvil.toml"), body);
}

/// The headline scenario: `artifacts = ["recipes", "container"]` on a repo that
/// already owns its own `[workspace.lints]` and `deny.toml`. anvil must emit
/// the recipe tree and the container files while leaving every user config file
/// byte-for-byte untouched — no managed region spliced anywhere.
#[test]
fn recipes_and_container_emits_recipes_and_container_but_no_config_regions() {
    let (_tmp, root) = named_workspace("repo");

    // A repo that already declares its own lints and deny config — the exact
    // collision that forced this feature.
    let user_cargo = "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n\
         [workspace.lints.clippy]\nunwrap_used = \"warn\"\n";
    write(&root.join("Cargo.toml"), user_cargo);
    let user_deny = "[bans]\nmultiple-versions = \"deny\"\n";
    write(&root.join("deny.toml"), user_deny);
    let user_member = read(&root.join("crates/alpha/Cargo.toml"));

    set_anvil_toml(
        &root,
        "[anvil]\nartifacts = [\"recipes\", \"container\"]\n\n\
         [container]\nenabled = true\nimage = \"ghcr.io/acme/rust-dev:1.2.3\"\n",
    );

    run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root).unwrap();

    // Recipe tree present.
    assert!(root.join("justfiles/anvil/mod.just").is_file());
    assert!(root.join("justfiles/anvil/tiers.just").is_file());
    // Container files present.
    assert!(root.join("justfiles/anvil/container.just").is_file());
    // Justfile imports region present (recipes group includes it).
    assert!(read(&root.join("Justfile")).contains("anvil-imports"));

    // No config region spliced into any user config file, and the pre-existing
    // files are untouched byte-for-byte.
    assert_eq!(read(&root.join("Cargo.toml")), user_cargo, "root Cargo.toml must be untouched");
    assert_eq!(read(&root.join("deny.toml")), user_deny, "deny.toml must be untouched");
    assert_eq!(
        read(&root.join("crates/alpha/Cargo.toml")),
        user_member,
        "member Cargo.toml must be untouched"
    );
    assert!(!root.join("rustfmt.toml").exists(), "rustfmt.toml must not be created");
    assert!(!root.join(".gitattributes").exists(), ".gitattributes must not be created");
    assert!(!root.join("clippy.toml").exists(), "clippy.toml must not be created");
    assert!(!root.join("spellcheck.toml").exists(), "spellcheck.toml must not be created");
    assert!(!root.join(".delta.toml").exists(), ".delta.toml must not be created");

    // `backends` not selected -> no CI files even though --backend github was given.
    assert!(!root.join(".github").exists(), "backends group not selected -> no CI files");

    // No anvil-managed sentinel anywhere in a user config file.
    for name in ["Cargo.toml", "deny.toml", "crates/alpha/Cargo.toml"] {
        assert!(
            !read(&root.join(name)).contains("anvil-managed"),
            "{name} must carry no managed region"
        );
    }
}

/// The hard compatibility requirement: an omitted `artifacts` key is
/// byte-identical to listing every group explicitly.
#[test]
fn omitted_key_is_byte_identical_to_all_groups_listed() {
    let (_tmp_a, root_a) = named_workspace("repo");
    run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root_a).unwrap();

    let (_tmp_b, root_b) = named_workspace("repo");
    set_anvil_toml(
        &root_b,
        "[anvil]\nartifacts = [\"recipes\", \"config\", \"backends\", \"container\"]\n",
    );
    run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root_b).unwrap();

    assert_eq!(
        render_tree(&root_a),
        render_tree(&root_b),
        "an explicit all-groups list must emit the same tree as an omitted key"
    );
}

/// Idempotency under a restricted allow-list: applying twice with the same
/// list produces no changes on the second (dry) run.
#[test]
fn restricted_allow_list_is_idempotent() {
    let (_tmp, root) = named_workspace("repo");
    set_anvil_toml(
        &root,
        "[anvil]\nartifacts = [\"recipes\", \"container\"]\n\n\
         [container]\nenabled = true\nimage = \"img:1\"\n",
    );

    run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root).unwrap();
    let outcome = run_update(&cargo_anvil::Catalog::anvil(), &github(true), &root).unwrap();
    assert!(
        !outcome.plan.has_changes(),
        "second run with the same allow-list must report no changes"
    );
}

/// Retraction: apply with every group (config regions + backend files land),
/// then re-apply with only `recipes`. The dropped groups' artifacts are
/// cleanly retracted — managed regions removed, owned files deleted — while the
/// recipe tree and all user-authored content survive.
#[test]
fn dropping_groups_retracts_only_anvil_owned_artifacts() {
    let (_tmp, root) = named_workspace("repo");

    // A user file and user-authored content that must survive both runs.
    write(&root.join("NOTES.md"), "user notes\n");

    // First run: omitted key -> every group. Recipes, config regions, and the
    // github backend files all land.
    run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root).unwrap();
    assert!(read(&root.join("Cargo.toml")).contains("anvil-managed"), "config region landed");
    assert!(root.join("deny.toml").is_file(), "deny.toml created by config group");
    assert!(root.join("rustfmt.toml").is_file(), "rustfmt.toml created by config group");
    assert!(root.join(".github/workflows/anvil-pr.yml").is_file(), "backend files landed");
    assert!(root.join("justfiles/anvil/mod.just").is_file(), "recipe tree landed");

    // Second run: only `recipes`. Config + backends retract.
    set_anvil_toml(&root, "[anvil]\nartifacts = [\"recipes\"]\n");
    run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root).unwrap();

    // Recipe tree survives.
    assert!(root.join("justfiles/anvil/mod.just").is_file(), "recipe tree must survive");
    assert!(read(&root.join("Justfile")).contains("anvil-imports"), "imports region survives");

    // Config regions retract by splicing the managed region *out* of the host
    // file (anvil owns the region, not the whole user-config file). The
    // sentinel is gone from every config host; a host anvil created solely to
    // carry the region is left behind region-free rather than deleted, so user
    // content added outside a managed region can never be destroyed.
    assert!(
        !read(&root.join("Cargo.toml")).contains("anvil-managed"),
        "config region must be spliced out of Cargo.toml"
    );
    assert!(
        !read(&root.join("rustfmt.toml")).contains("anvil-managed"),
        "rustfmt.toml region must be spliced out"
    );
    // Owned backend files are deleted outright.
    assert!(
        !root.join(".github/workflows/anvil-pr.yml").exists(),
        "backend files must be retracted"
    );

    // User content untouched throughout.
    assert_eq!(read(&root.join("NOTES.md")), "user notes\n", "user file must be untouched");

    // A third dry run with the restricted list is clean (retraction converged).
    let outcome = run_update(&cargo_anvil::Catalog::anvil(), &github(true), &root).unwrap();
    assert!(!outcome.plan.has_changes(), "post-retraction dry run must be clean");
}

/// An unknown group name is rejected end-to-end through `run_update`.
#[test]
fn unknown_group_name_is_rejected() {
    let (_tmp, root) = named_workspace("repo");
    set_anvil_toml(&root, "[anvil]\nartifacts = [\"recipes\", \"widgets\"]\n");
    let err = run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown artifact group 'widgets'"), "got: {err}");
}

/// An empty allow-list is rejected end-to-end with a remove-the-key hint.
#[test]
fn empty_allow_list_is_rejected() {
    let (_tmp, root) = named_workspace("repo");
    set_anvil_toml(&root, "[anvil]\nartifacts = []\n");
    let err = run_update(&cargo_anvil::Catalog::anvil(), &github(false), &root)
        .unwrap_err()
        .to_string();
    assert!(err.contains("empty list"), "got: {err}");
}
