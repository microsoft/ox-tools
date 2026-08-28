// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox the FS ops these tests do (TempDir, run_update).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]
#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]
#![expect(
    clippy::panic,
    reason = "integration tests panic on unmet preconditions for readable failure output"
)]

//! Consumer-upgrade coverage for generated artifact retirement and relocation.
//!
//! The snapshot tests describe a fresh tree. This exercises the path an
//! existing adopter actually takes: a repository generated before the move,
//! with its `.anvil.lock` and its assets under `justfiles/anvil/container/`,
//! updated by a binary that emits `.anvil/container/` and no longer emits the
//! standalone stable-toolchain resolver.

use std::path::Path;

use cargo_anvil::test_support::{Cli, Decision, Manifest, RunOutcome, Target, run_update};
use cargo_anvil::{Artifact, Catalog, artifacts};
use tempfile::TempDir;

/// A hand-authored customization file: never catalog-tracked, so `cargo anvil`
/// can neither move it nor report it. The drivers warn about one left here.
const LEGACY_CUSTOMIZE: &str = "justfiles/anvil/container/customize.sh";
const LEGACY_RESOLVER: &str = ".anvil/resolve-stable-toolchain.ps1";

/// Where a generated container asset lived before the move. Every asset,
/// including the entry recipe, sat directly under `justfiles/anvil/container/`.
fn pre_move_path(current: &str) -> String {
    let name = current.rsplit('/').next().expect("split always yields one element");
    format!("justfiles/anvil/container/{name}")
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
    tmp
}

fn local() -> Cli {
    Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force: false,
    }
}

/// The current container assets, paired with their pre-move locations.
fn container_assets() -> Vec<(String, String)> {
    artifacts::container::all()
        .into_iter()
        .map(|artifact| match artifact {
            Artifact::OwnedFile(spec) => (spec.path.to_owned(), pre_move_path(spec.path)),
            Artifact::Region(_) => panic!("container artifacts are owned files"),
        })
        .collect()
}

/// Rewrite a freshly generated tree into the shape the previous release
/// produced: every generated container asset under `justfiles/anvil/container/`,
/// tracked at that path by `.anvil.lock`.
fn rewind_to_pre_move_layout(root: &Path) {
    let mut manifest = Manifest::load(root).unwrap();
    for (current, previous) in container_assets() {
        let to = root.join(&previous);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(root.join(&current), &to).unwrap();
        let checksum = manifest
            .files
            .remove(&current)
            .unwrap_or_else(|| panic!("{current} must be tracked by the fresh lock"));
        manifest.files.insert(previous, checksum);
    }
    write(&root.join(LEGACY_RESOLVER), "");
    manifest.files.insert(
        LEGACY_RESOLVER.to_owned(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
    );
    // Provenance of the older build. It is recorded, never a gate.
    manifest.catalog_checksum = Some("sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned());
    manifest.tool_version = Some("0.2.0".to_owned());
    manifest.save(root).unwrap();
    std::fs::remove_dir(root.join(".anvil/container")).unwrap();
}

fn decision_for(outcome: &RunOutcome, path: &str) -> Decision {
    outcome
        .plan
        .items()
        .iter()
        .find(|item| matches!(&item.target, Target::File { path: candidate } if candidate == path))
        .unwrap_or_else(|| panic!("no plan item for {path}"))
        .decision
}

#[test]
fn upgrading_from_the_pre_move_layout_relocates_generated_container_assets() {
    let tmp = workspace();
    let root = tmp.path();
    run_update(&Catalog::anvil(), &local(), root).unwrap();
    rewind_to_pre_move_layout(root);

    let assets = container_assets();
    let (customized_current, customized_previous) = assets
        .iter()
        .find(|(current, _)| current.ends_with("entrypoint.sh"))
        .cloned()
        .expect("the entry point is part of the container group");

    // One adopter-edited generated asset, and one hand-authored customization
    // file the catalog has never tracked.
    let customized_body = "#!/bin/sh\n# locally patched entry point\n";
    write(&root.join(&customized_previous), customized_body);
    write(&root.join(LEGACY_CUSTOMIZE), "# hand-authored customization\n");

    let outcome = run_update(&Catalog::anvil(), &local(), root).unwrap();
    assert!(outcome.applied);

    let manifest = Manifest::load(root).unwrap();
    assert!(
        !root.join(LEGACY_RESOLVER).exists(),
        "the untouched previously-owned resolver must be removed"
    );
    assert!(
        !manifest.files.contains_key(LEGACY_RESOLVER),
        "the retired resolver must be dropped from the lock"
    );
    assert_eq!(
        decision_for(&outcome, LEGACY_RESOLVER),
        Decision::Remove,
        "an untouched retired resolver must be removed during upgrade"
    );
    for (current, previous) in &assets {
        // Every asset is re-emitted at its new location and tracked there.
        assert!(root.join(current).is_file(), "{current} must be written at the new location");
        assert_eq!(
            decision_for(&outcome, current),
            Decision::Write,
            "{current} must be freshly written"
        );
        assert!(manifest.files.contains_key(current), "{current} must be tracked at the new path");
        assert!(!manifest.files.contains_key(previous), "{previous} must be dropped from the lock");

        if previous == &customized_previous {
            continue;
        }
        // Untouched old assets are removed outright.
        assert!(!root.join(previous).exists(), "untouched orphan {previous} must be removed");
        assert_eq!(
            decision_for(&outcome, previous),
            Decision::Remove,
            "{previous} must be removed as an untouched orphan"
        );
    }

    // The adopter's edit survives at the old path, with ownership transferred.
    assert_eq!(
        std::fs::read_to_string(root.join(&customized_previous)).unwrap(),
        customized_body,
        "a customized orphan must keep the adopter's content"
    );
    assert_eq!(
        decision_for(&outcome, &customized_previous),
        Decision::OrphanedKept,
        "a customized orphan must transfer ownership rather than be deleted"
    );
    assert_ne!(
        std::fs::read_to_string(root.join(&customized_current)).unwrap(),
        customized_body,
        "the new location must carry the current template, not the adopter's old edit"
    );

    // The recipe keeps its identity across the move: it is emitted at the new
    // path and imported from there.
    let recipe = std::fs::read_to_string(root.join("justfiles/anvil/container.just")).unwrap();
    assert!(recipe.contains("anvil-container"), "the entry recipe must survive the move");
    let module = std::fs::read_to_string(root.join("justfiles/anvil/mod.just")).unwrap();
    assert!(
        module.contains("import 'container.just'"),
        "mod.just must import the flattened recipe:\n{module}"
    );

    // A hand-authored customization file is invisible to the catalog, so the
    // update neither moves nor deletes it. The drivers warn at run time.
    assert_eq!(
        std::fs::read_to_string(root.join(LEGACY_CUSTOMIZE)).unwrap(),
        "# hand-authored customization\n",
        "an untracked customization file must be left exactly as the adopter wrote it"
    );

    // The migration is complete: a second update is a no-op.
    let settled = run_update(&Catalog::anvil(), &local(), root).unwrap();
    assert!(
        !settled.plan.has_changes(),
        "the upgraded tree must be steady; plan: {:#?}",
        settled.plan.items()
    );
}
