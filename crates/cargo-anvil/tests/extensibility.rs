// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))] // miri can't sandbox the FS ops these tests do (TempDir, run_update).
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]
#![expect(clippy::unwrap_used, reason = "integration tests favor concise assertions over Result plumbing")]

//! Extensibility verification: a second front-end catalog (`demoforge`) and a
//! multi-level extension chain (`forge3`), exercised through the real engine.
//!
//! Proves the seam from [`extensibility.md`](../docs/design/extensibility.md):
//! a downstream catalog adds/overrides/drops artifacts and fans a member
//! region across the workspace, the single-tool guard distinguishes it from
//! `anvil`, and a third catalog can extend the second.

use std::path::Path;

use cargo_anvil::test_support::{Cli, Manifest, run_update};
use cargo_anvil::{Artifact, Catalog, RegionId, artifacts};
use tempfile::TempDir;

const EXTRA_FILE: &str = "justfiles/anvil/demoforge.just";
const METADATA_REGION: &str = "demoforge-metadata";
const CONTAINER_JUST: &str = "justfiles/anvil/container/container.just";
const CONTAINERFILE: &str = "justfiles/anvil/container/Containerfile";
const CONTAINER_RUNNER: &str = "justfiles/anvil/container/run-in-container.ps1";
const CONTAINER_AUTH: &str = "justfiles/anvil/container/auth.ps1";

/// The example downstream catalog: anvil's, customized four ways.
fn demoforge() -> Catalog {
    Catalog::anvil()
        .into_builder()
        .subcommand("demoforge")
        .about("DemoForge: an example anvil fork for tests")
        .version("9.9.9")
        // Append an owned file.
        .with_artifact(Artifact::owned_file(EXTRA_FILE, "# demoforge\nanvil-demo:\n    @echo hi\n"))
        // Override a built-in region (identity + gate preserved via with_body).
        .replace_artifact(artifacts::region::rustfmt().with_body("max_width = 80\n"))
        // Drop a built-in region.
        .without_artifact(artifacts::region::clippy())
        // Add a region replicated across every workspace member's manifest.
        .with_artifact(Artifact::member_region(RegionId::new(METADATA_REGION), "# managed by demoforge\n"))
        .build()
        .unwrap()
}

fn containerforge() -> Catalog {
    Catalog::anvil()
        .into_builder()
        .subcommand("containerforge")
        .about("ContainerForge: an anvil container catalog for tests")
        .version("9.9.9")
        .replace_artifact(artifacts::container::containerfile().with_body("FROM internal.example/base\n"))
        .with_artifact(artifacts::container::auth_powershell("# internal auth\n"))
        .build()
        .unwrap()
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// A two-member workspace, so member-region fan-out is observable.
fn workspace() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    );
    for member in ["alpha", "beta"] {
        write(
            &root.join(format!("crates/{member}/Cargo.toml")),
            &format!("[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        );
        write(&root.join(format!("crates/{member}/src/lib.rs")), "");
    }
    tmp
}

fn local(force: bool) -> Cli {
    Cli {
        backends: vec![],
        no_backends: true,
        dry_run: false,
        force,
    }
}

#[test]
fn demoforge_subcommand_parses() {
    let catalog = demoforge();
    // Cargo injects the subcommand token; it must be stripped.
    let cli = Cli::parse_from_cargo_args(&catalog, ["cargo-demoforge", "demoforge", "--dry-run"]).unwrap();
    assert!(cli.dry_run);
}

#[test]
fn demoforge_emits_extra_overrides_drops_and_fans_out() {
    let tmp = workspace();
    let catalog = demoforge();
    let outcome = run_update(&catalog, &local(false), tmp.path()).unwrap();
    assert!(outcome.applied);

    // Appended owned file is emitted.
    assert!(tmp.path().join(EXTRA_FILE).is_file(), "the extra owned file must be written");

    // Overridden built-in region carries the new body.
    let rustfmt = std::fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap();
    assert!(rustfmt.contains("max_width = 80"), "rustfmt override must take effect:\n{rustfmt}");

    // Dropped built-in region is not emitted.
    assert!(
        !tmp.path().join("clippy.toml").exists(),
        "dropped clippy region must not be written"
    );

    // Member region fans out across every member's manifest.
    for member in ["alpha", "beta"] {
        let manifest = std::fs::read_to_string(tmp.path().join(format!("crates/{member}/Cargo.toml"))).unwrap();
        assert!(
            manifest.contains(&format!("anvil-managed: {METADATA_REGION}")),
            "member {member} must carry the fanned-out region:\n{manifest}"
        );
    }

    // The lock is now owned by demoforge.
    let saved = Manifest::load(tmp.path()).unwrap();
    assert_eq!(saved.tool.as_deref(), Some("demoforge"));
    assert_eq!(saved.catalog_checksum, Some(catalog.checksum()));
}

#[test]
fn base_anvil_output_is_unaffected_by_the_fork() {
    // The fork's edits live in its own catalog; anvil's output for the same
    // workspace neither gains the extra file nor loses clippy.
    let tmp = workspace();
    run_update(&Catalog::anvil(), &local(false), tmp.path()).unwrap();
    assert!(!tmp.path().join(EXTRA_FILE).exists(), "anvil must not emit the fork's file");
    assert!(tmp.path().join("clippy.toml").is_file(), "anvil still emits clippy.toml");
    let rustfmt = std::fs::read_to_string(tmp.path().join("rustfmt.toml")).unwrap();
    assert!(!rustfmt.contains("max_width = 80"), "anvil keeps its own rustfmt body");
}

#[test]
fn public_container_artifacts_can_be_specialized_by_downstream_catalogs() {
    let base = workspace();
    run_update(&Catalog::anvil(), &local(false), base.path()).unwrap();
    let base_container = std::fs::read_to_string(base.path().join(CONTAINER_JUST)).unwrap();
    assert!(
        base_container.contains("anvil-container"),
        "base catalog must expose the public container command"
    );
    assert!(
        base.path().join(CONTAINER_RUNNER).is_file(),
        "base catalog must emit its public runner"
    );
    let base_runner = std::fs::read_to_string(base.path().join(CONTAINER_RUNNER)).unwrap();
    assert!(
        !base.path().join(CONTAINER_AUTH).exists(),
        "base catalog must not emit an environment-specific auth hook"
    );

    let configured = workspace();
    run_update(&containerforge(), &local(false), configured.path()).unwrap();
    let configured_container = std::fs::read_to_string(configured.path().join(CONTAINER_JUST)).unwrap();
    assert_eq!(
        configured_container, base_container,
        "downstream catalog must inherit the public container command unchanged"
    );
    let configured_containerfile = std::fs::read_to_string(configured.path().join(CONTAINERFILE)).unwrap();
    assert_eq!(
        configured_containerfile, "FROM internal.example/base\n",
        "downstream catalog must replace the public Containerfile"
    );
    let configured_runner = std::fs::read_to_string(configured.path().join(CONTAINER_RUNNER)).unwrap();
    assert_eq!(
        configured_runner, base_runner,
        "downstream catalog must inherit the public runner unchanged"
    );
    let configured_auth = std::fs::read_to_string(configured.path().join(CONTAINER_AUTH)).unwrap();
    assert_eq!(
        configured_auth, "# internal auth\n",
        "downstream catalog must be able to add its environment-specific auth hook"
    );
}

#[test]
fn guard_separates_anvil_and_demoforge() {
    let tmp = workspace();
    // demoforge takes ownership first.
    run_update(&demoforge(), &local(false), tmp.path()).unwrap();

    // anvil refuses against a demoforge-owned lock.
    let err = run_update(&Catalog::anvil(), &local(false), tmp.path()).unwrap_err();
    assert!(err.to_string().contains("managed by 'demoforge'"), "got: {err}");

    // --force switches ownership to anvil and reconciles normally: the fork's
    // orphaned extra file is removed, the dropped clippy region reappears.
    let outcome = run_update(&Catalog::anvil(), &local(true), tmp.path()).unwrap();
    assert!(outcome.applied);
    assert!(
        !tmp.path().join(EXTRA_FILE).exists(),
        "forced switch removes the fork's orphaned file"
    );
    assert!(
        tmp.path().join("clippy.toml").is_file(),
        "forced switch re-emits anvil's clippy region"
    );
    let saved = Manifest::load(tmp.path()).unwrap();
    assert_eq!(saved.tool.as_deref(), Some("anvil"));
}

#[test]
fn multi_level_chain_composes() {
    // forge3 extends demoforge's catalog: override the file demoforge added,
    // drop the region demoforge added.
    let forge3 = demoforge()
        .into_builder()
        .subcommand("forge3")
        .version("0.0.1")
        .replace_artifact(Artifact::owned_file(EXTRA_FILE, "# forge3 override\n"))
        .without_artifact(Artifact::member_region(RegionId::new(METADATA_REGION), ""))
        .build()
        .unwrap();

    assert_eq!(forge3.cli().subcommand, "forge3");

    let extra = forge3
        .artifacts()
        .iter()
        .find(|a| matches!(a, Artifact::OwnedFile(spec) if spec.path == EXTRA_FILE))
        .expect("forge3 still carries the extra file");
    assert_eq!(extra.body(), "# forge3 override\n", "forge3 overrides an ancestor artifact");

    // The metadata region demoforge added is gone in forge3.
    let has_metadata = forge3
        .artifacts()
        .iter()
        .any(|a| matches!(a, Artifact::Region(spec) if spec.id == RegionId::new(METADATA_REGION)));
    assert!(!has_metadata, "forge3 drops an ancestor artifact");

    // The chain's checksum reflects forge3's composed catalog, not anvil's.
    assert_ne!(forge3.checksum(), Catalog::anvil().checksum());
}
