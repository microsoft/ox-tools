// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![expect(clippy::unwrap_used, reason = "panic-on-failure idioms are appropriate in integration tests")]

//! End-to-end contracts for the generated recipe surface.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

use cargo_anvil::Catalog;
use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

struct Generated {
    temp: TempDir,
    hub: String,
    checks: String,
    setup: String,
    container: String,
}

impl Generated {
    fn all_recipes(&self) -> String {
        [&self.hub, &self.checks, &self.setup, &self.container]
            .into_iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn generated() -> Generated {
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
    let read = |name: &str| std::fs::read_to_string(temp.path().join(".anvil").join(name)).unwrap();
    Generated {
        hub: read("anvil.just"),
        checks: read("checks.just"),
        setup: read("setup.just"),
        container: read("container.just"),
        temp,
    }
}

fn run_just(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command.current_dir(root).args(args).env_remove("RUSTUP_TOOLCHAIN");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

#[test]
fn generated_recipes_are_split_by_responsibility() {
    let generated = generated();
    for import in ["setup.just", "checks.just", "container.just"] {
        assert!(generated.hub.contains(&format!("import '{import}'")));
    }
    assert!(generated.hub.contains("alias anvil := anvil-pr"));
    assert!(generated.checks.contains("anvil-clippy:"));
    assert!(generated.checks.contains("anvil-pr-fast:"));
    assert!(generated.checks.contains("anvil-impact:"));
    assert!(!generated.checks.contains("anvil-clippy-setup"));
    assert!(generated.setup.contains("anvil-clippy-setup"));
    assert!(generated.setup.contains("cargo_delta_version"));
    assert!(!generated.setup.contains("anvil-clippy:"));
    assert!(generated.container.contains("anvil-container *command:"));
    assert!(!generated.container.contains("anvil-clippy:"));
    assert!(!generated.all_recipes().contains("anvil-semver-check"));
}

#[test]
fn common_checks_are_direct_cargo_each_invocations() {
    let recipes = generated().checks;
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
    let recipes = generated().checks;
    assert!(recipes.contains(
        "cargo +{{ rust_nightly }} each {{ anvil_affected_selection }} --once -- \
         cargo +{{ rust_nightly }} coverage-gate '{packages}' run \
         --no-coverage-target aarch64-pc-windows-msvc"
    ));
    assert!(!recipes.contains("Invoke-AnvilLcovReport"));
    assert!(!recipes.contains("llvm-cov.rsp"));
}

#[test]
fn setup_uses_lazy_inventory_and_exact_install_policy() {
    let recipes = generated().setup;
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
    let recipes = generated().setup;
    for pin in [
        "cargo_aprz_version := \"1.2.0\"",
        "cargo_coverage_gate_version := \"0.6.0\"",
        "cargo_delta_version := \"0.4.0\"",
        "cargo_each_version := \"0.3.0\"",
    ] {
        assert!(recipes.contains(pin), "missing released tool pin: {pin}");
    }
    assert!(!recipes.contains("cargo_semver_checks_version"));
}

#[test]
fn shell_scripts_are_limited_to_domain_exceptions() {
    let recipes = generated().all_recipes();
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
    ] {
        assert!(recipe_names.contains(portable));
    }
    for retired in [
        "_anvil-impact-include",
        "_anvil-impact-format",
        "_anvil-impact-snapshot",
        "_anvil-resolve-stable",
        "_anvil-stable-toolchain-args",
    ] {
        assert!(!recipes.contains(retired), "retired helper survived: {retired}");
    }
}

#[test]
fn stable_toolchain_selection_preserves_override_and_fallback_precedence() {
    let generated = generated();
    let root = generated.temp.path();

    let environment = run_just(
        root,
        &["--evaluate", "anvil_stable_toolchain_arg"],
        &[("RUSTUP_TOOLCHAIN", "selected")],
    );
    assert!(environment.status.success());
    assert_eq!(String::from_utf8(environment.stdout).unwrap().trim(), "");

    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    let toolchain_file = run_just(root, &["--evaluate", "anvil_stable_toolchain_arg"], &[]);
    assert!(toolchain_file.status.success());
    assert_eq!(String::from_utf8(toolchain_file.stdout).unwrap().trim(), "");

    std::fs::remove_file(root.join("rust-toolchain.toml")).unwrap();
    let fallback = run_just(root, &["--evaluate", "anvil_stable_toolchain_arg"], &[]);
    assert!(fallback.status.success());
    assert_eq!(String::from_utf8(fallback.stdout).unwrap().trim(), "'+{workspace-rust-version}'");
}

#[test]
fn default_component_install_targets_the_effective_stable_selection() {
    let generated = generated();
    let root = generated.temp.path();

    let fallback = run_just(root, &["--dry-run", "_install-component", "default", "clippy"], &[]);
    assert!(fallback.status.success());
    let fallback_stdout = format!(
        "{}{}",
        String::from_utf8(fallback.stdout).unwrap(),
        String::from_utf8(fallback.stderr).unwrap()
    );
    assert!(
        fallback_stdout.contains("rustup component add --toolchain '{workspace-rust-version}' clippy"),
        "unexpected fallback plan: {fallback_stdout}"
    );

    let selected = run_just(
        root,
        &["--dry-run", "_install-component", "default", "clippy"],
        &[("RUSTUP_TOOLCHAIN", "selected")],
    );
    assert!(selected.status.success());
    let selected_output = format!(
        "{}{}",
        String::from_utf8(selected.stdout).unwrap(),
        String::from_utf8(selected.stderr).unwrap()
    );
    assert!(selected_output.contains("rustup component add clippy"));

    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    let file_selected = run_just(root, &["--dry-run", "_install-component", "default", "clippy"], &[]);
    assert!(file_selected.status.success());
    let file_output = format!(
        "{}{}",
        String::from_utf8(file_selected.stdout).unwrap(),
        String::from_utf8(file_selected.stderr).unwrap()
    );
    assert!(file_output.contains("rustup component add clippy"));
}

#[test]
fn container_context_and_setup_use_all_generated_recipe_files() {
    let generated = generated();
    let root = generated.temp.path();
    let ignore = std::fs::read_to_string(root.join(".anvil/container/Dockerfile.dockerignore")).unwrap();
    let dockerfile = std::fs::read_to_string(root.join(".anvil/container/Dockerfile")).unwrap();
    for path in ["anvil.just", "checks.just", "setup.just", "container.just"] {
        assert!(ignore.contains(&format!("!.anvil/{path}")));
        assert!(generated.container.contains(&format!("'.anvil/{path}'")));
    }
    assert!(dockerfile.contains("import '.anvil/anvil.just'"));
    assert!(dockerfile.contains("ARG ANVIL_RUST_VERSION"));
}

#[test]
fn container_identity_uses_the_declared_msrv_not_the_installed_patch() {
    let generated = generated();
    let root = generated.temp.path();
    let output = run_just(root, &["_anvil-root-msrv"], &[]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "1.95");

    write(&root.join("Cargo.toml"), "[workspace]\nresolver = \"2\"\nmembers = [\"crate\"]\n");
    let output = run_just(root, &["_anvil-root-msrv"], &[]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "none");
}

#[test]
fn generated_root_imports_only_the_hub() {
    let generated = generated();
    let root = std::fs::read_to_string(generated.temp.path().join("Justfile")).unwrap();
    assert!(root.contains("import '.anvil/anvil.just'"));
    assert!(!root.contains("checks.just"));
    assert!(!root.contains("setup.just"));
    assert!(!root.contains("container.just"));
}

#[cfg(unix)]
fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git").current_dir(root).args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[cfg(unix)]
fn pwsh_available() -> bool {
    Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(unix)]
#[test]
fn container_tag_frames_executable_modes_and_refuses_unstaged_mode_drift() {
    use std::os::unix::fs::PermissionsExt;

    if !pwsh_available() {
        return;
    }
    let generated = generated();
    let root = generated.temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "user.name", "Fixture"]);
    git(root, &["add", "."]);

    let tag = |root: &Path| run_just(root, &["anvil-container-tag"], &[]);
    let plain = tag(root);
    assert!(
        plain.status.success(),
        "plain tag failed:\n{}",
        String::from_utf8_lossy(&plain.stderr)
    );

    let setup = root.join(".anvil/setup.just");
    let mut permissions = std::fs::metadata(&setup).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&setup, permissions).unwrap();
    git(root, &["update-index", "--chmod=+x", ".anvil/setup.just"]);
    let executable = tag(root);
    assert!(
        executable.status.success(),
        "executable tag failed:\n{}",
        String::from_utf8_lossy(&executable.stderr)
    );
    assert_ne!(
        String::from_utf8_lossy(&plain.stdout).trim(),
        String::from_utf8_lossy(&executable.stdout).trim(),
        "the executable bit must contribute to the image tag"
    );

    let mut permissions = std::fs::metadata(&setup).unwrap().permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&setup, permissions).unwrap();
    let drift = tag(root);
    assert!(!drift.status.success(), "unstaged mode drift must be refused");
    let diagnostic = String::from_utf8_lossy(&drift.stderr);
    assert!(
        diagnostic.contains("working") && diagnostic.contains("Stage"),
        "unexpected mode-drift diagnostic:\n{diagnostic}"
    );
}
