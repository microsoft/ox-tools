// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![expect(clippy::unwrap_used, reason = "panic-on-failure idioms are appropriate in integration tests")]

//! End-to-end contracts for the generated recipe surface.
//!
//! Domain-tool internals are tested in cargo-delta, cargo-each,
//! cargo-coverage-gate, and cargo-aprz. These tests pin Anvil's wiring to those
//! interfaces instead of re-testing their implementations through generated
//! shell programs.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use cargo_anvil::Catalog;
use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn generated() -> (TempDir, String) {
    let temp = TempDir::new().unwrap();
    write(
        &temp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crate\"]\n\
         [workspace.package]\nrust-version = \"1.95\"\n",
    );
    write(
        &temp.path().join("crate/Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         rust-version = \"1.95\"\n",
    );
    write(&temp.path().join("crate/src/lib.rs"), "pub fn value() -> u8 { 1 }\n");
    run_update(
        &Catalog::anvil(),
        &Cli {
            backends: Vec::new(),
            no_backends: true,
            dry_run: false,
            force: false,
        },
        temp.path(),
    )
    .unwrap();
    let recipes = std::fs::read_to_string(temp.path().join(".anvil/anvil.just")).unwrap();
    (temp, recipes)
}

#[test]
fn common_checks_are_direct_cargo_each_invocations() {
    let (_, recipes) = generated();
    for command in [
        "cargo each {{ anvil_affected_selection }} --once -- cargo {{ anvil_stable_toolchain_arg }} clippy '{packages}'",
        "cargo each {{ anvil_required_selection }} --once -- cargo {{ anvil_stable_toolchain_arg }} hack '{packages}'",
        "cargo +{{ rust_nightly_external_types }} each {{ anvil_affected_selection }} --filter target-kind:lib",
        "cargo each {{ anvil_affected_selection }} --each-target test --target-required-feature loom",
    ] {
        assert!(recipes.contains(command), "missing direct command shape: {command}");
    }
}

#[test]
fn coverage_is_one_domain_tool_invocation() {
    let (_, recipes) = generated();
    assert!(recipes.contains(
        "cargo +{{ rust_nightly }} each {{ anvil_affected_selection }} --once -- \
         cargo +{{ rust_nightly }} coverage-gate '{packages}' run \
         --no-coverage-target aarch64-pc-windows-msvc"
    ));
    assert!(!recipes.contains("Invoke-AnvilLcovReport"));
    assert!(!recipes.contains("llvm-cov.rsp"));
}

#[test]
fn impact_outputs_are_generic_package_files() {
    let (_, recipes) = generated();
    for tier in ["modified", "affected", "required"] {
        assert!(recipes.contains(&format!("--{tier} --format packages")));
        assert!(recipes.contains(&format!("target/anvil/impact/{tier}.packages")));
    }
    assert!(!recipes.contains("include_affected.txt"));
    assert!(!recipes.contains("_anvil-impact-format"));
}

#[test]
fn examples_use_cargo_each_target_discovery_and_timeouts() {
    let (_, recipes) = generated();
    assert!(recipes.contains("--each-target example --timeout {{ timeout }}s --keep-going"));
    assert!(recipes.contains("--example '{target}'"));
    assert!(recipes.contains("env.ANVIL_EXAMPLE=\"1\""));
    assert!(!recipes.contains("System.Diagnostics.ProcessStartInfo"));
}

#[test]
fn miri_uses_cargo_each_for_filtering_parallelism_and_cleanup() {
    let (_, recipes) = generated();
    assert!(recipes.contains("--exclude-filter 'metadata:anvil.miri.exclude=true'"));
    assert!(recipes.contains("--jobs {{ anvil_miri_jobs }} --keep-going"));
    assert!(recipes.contains("miri test --package '{spec}'"));
    assert!(!recipes.contains("compiler-artifact"));
    assert!(!recipes.contains("ForEach-Object -Parallel"));
}

#[test]
fn setup_uses_lazy_inventory_and_exact_install_policy() {
    let (_, recipes) = generated();
    for contract in [
        "set lazy",
        "installed_cargo_tools := `cargo install --list`",
        "semver_matches(installed, \">=\" + minimum)",
        "cargo install --locked --version =",
        "cargo binstall --no-confirm --locked --disable-strategies compile",
        "anvil-toolchain-stable-install installer=\"install\": (anvil-tool-cargo-each-install installer)",
    ] {
        assert!(recipes.contains(contract), "missing setup contract: {contract}");
    }
    assert!(!recipes.contains("falling back to cargo install"));
}

#[test]
fn released_domain_tool_versions_are_pinned() {
    let (_, recipes) = generated();
    for pin in [
        "cargo_aprz_version := \"1.2.0\"",
        "cargo_coverage_gate_version := \"0.6.0\"",
        "cargo_delta_version := \"0.4.0\"",
        "cargo_each_version := \"0.3.0\"",
    ] {
        assert!(recipes.contains(pin), "missing released tool pin: {pin}");
    }
}

#[test]
fn shell_scripts_are_limited_to_domain_exceptions() {
    let (_, recipes) = generated();
    let recipe_names = recipes
        .lines()
        .filter_map(|line| {
            let head = line.split_once(':')?.0.split_whitespace().next()?;
            (head.starts_with("anvil-") || head.starts_with("_anvil-")).then_some(head)
        })
        .collect::<BTreeSet<_>>();

    for portable in [
        "anvil-impact",
        "anvil-clippy",
        "anvil-fmt",
        "anvil-llvm-cov",
        "anvil-external-types",
        "anvil-loom",
        "anvil-msrv-test",
    ] {
        assert!(recipe_names.contains(portable));
    }

    for retired_helper in [
        "_anvil-impact-include",
        "_anvil-impact-format",
        "_anvil-impact-snapshot",
        "_anvil-resolve-stable",
        "_anvil-stable-toolchain-args",
    ] {
        assert!(!recipes.contains(retired_helper), "retired helper survived: {retired_helper}");
    }
}

#[test]
fn container_context_and_setup_use_the_composed_recipe_file() {
    let (temp, recipes) = generated();
    let ignore = std::fs::read_to_string(temp.path().join(".anvil/container/Dockerfile.dockerignore")).unwrap();
    let dockerfile = std::fs::read_to_string(temp.path().join(".anvil/container/Dockerfile")).unwrap();

    assert!(ignore.contains("!.anvil/anvil.just"));
    assert!(!ignore.contains("justfiles/anvil"));
    assert!(dockerfile.contains("import '.anvil/anvil.just'"));
    assert!(dockerfile.contains("ARG ANVIL_RUST_VERSION"));
    assert!(dockerfile.contains("RUSTUP_TOOLCHAIN=\"${ANVIL_RUST_VERSION}\" just anvil-setup binstall"));
    assert!(recipes.contains("--build-arg', \"ANVIL_RUST_VERSION=$rootMsrv\""));
    assert!(recipes.contains("else if anvil_stable_toolchain_arg == \"\" { \"cargo install --locked --version =\""));
}

#[test]
fn container_identity_uses_the_declared_msrv_not_the_installed_patch() {
    let (temp, recipes) = generated();
    let output = Command::new("just")
        .arg("_anvil-container-root-msrv")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "1.95");
    assert!(!recipes.contains("rustc '+{workspace-rust-version}' --version"));

    write(
        &temp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crate\"]\n",
    );
    let output = Command::new("just")
        .arg("_anvil-container-root-msrv")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "none");
}

#[test]
fn generated_root_imports_only_the_composed_file() {
    let (temp, _) = generated();
    let root = std::fs::read_to_string(temp.path().join("Justfile")).unwrap();
    assert!(root.contains("import '.anvil/anvil.just'"));
    assert!(!root.contains("justfiles/anvil"));
}
