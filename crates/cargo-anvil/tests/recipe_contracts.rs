// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![expect(clippy::unwrap_used, reason = "panic-on-failure idioms are appropriate in integration tests")]

//! End-to-end contracts for the generated recipe surface.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
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

fn generated_with_catalog(catalog: &Catalog) -> Generated {
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
        catalog,
        &Cli {
            backends: Vec::new(),
            no_backends: true,
            dry_run: false,
            force: false,
        },
        temp.path(),
    )
    .unwrap();
    let read = |name: &str| std::fs::read_to_string(temp.path().join(".anvil").join(name)).unwrap_or_default();
    Generated {
        hub: read("anvil.just"),
        checks: read("checks.just"),
        setup: read("setup.just"),
        container: read("container.just"),
        temp,
    }
}

fn generated() -> Generated {
    generated_with_catalog(&Catalog::anvil())
}

fn run_just(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command.current_dir(root).args(args).env_remove("RUSTUP_TOOLCHAIN");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn prepend_path(directory: &Path) -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&current));
    std::env::join_paths(paths).unwrap()
}

fn install_fake_cargo(root: &Path) -> PathBuf {
    let bin = root.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write(
        &bin.join("cargo.ps1"),
        r#"
if ($args -contains 'metadata') {
    if ($env:FAKE_CARGO_METADATA) {
        Write-Output $env:FAKE_CARGO_METADATA
        exit 0
    }
    [pscustomobject]@{
        packages = @(
            [pscustomobject]@{
                name = 'fixture'
                version = '0.1.0'
                targets = @([pscustomobject]@{ doctest = $true })
            },
            [pscustomobject]@{
                name = 'bin-only'
                version = '0.1.0'
                targets = @([pscustomobject]@{ doctest = $false })
            },
            [pscustomobject]@{
                name = 'macro-package'
                version = '0.1.0'
                targets = @([pscustomobject]@{ doctest = $true })
            }
        )
    } | ConvertTo-Json -Depth 5 -Compress
    exit 0
}
if (($args -contains '--dry-run') -and ($args | Where-Object { $_ -like 'workspace-rust-version=*' })) {
    Write-Output 'workspace-rust-version=1.95'
    exit 0
}
if ($env:FAKE_CARGO_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_LOG -Value (
        "MIRIFLAGS=$($env:MIRIFLAGS) RUSTFLAGS=$($env:RUSTFLAGS) " +
        "RUSTUP_AUTO_INSTALL=$($env:RUSTUP_AUTO_INSTALL) ARGS=" +
        ($args -join ' ')
    )
}
if (($args -join ' ') -match '(^| )rustup run( |$)') {
    & rustup run 1.95 rustc --version
    exit $LASTEXITCODE
}
if ($env:FAKE_CARGO_OUTPUT) { Write-Output $env:FAKE_CARGO_OUTPUT }
if ($env:FAKE_CARGO_EXIT) { exit [int]$env:FAKE_CARGO_EXIT }
"#,
    );

    #[cfg(windows)]
    write(
        &bin.join("cargo.cmd"),
        "@echo off\r\npwsh -NoProfile -File \"%~dp0cargo.ps1\" %*\r\nexit /b %ERRORLEVEL%\r\n",
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let executable = bin.join("cargo");
        write(
            &executable,
            "#!/bin/sh\nexec pwsh -NoProfile -File \"$(dirname \"$0\")/cargo.ps1\" \"$@\"\n",
        );
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(executable, permissions).unwrap();
    }

    bin
}

fn install_fake_rustup(bin: &Path) {
    write(
        &bin.join("rustup.ps1"),
        r#"
if ($env:FAKE_RUSTUP_LOG) {
    Add-Content -LiteralPath $env:FAKE_RUSTUP_LOG -Value (
        "RUSTUP_AUTO_INSTALL=$($env:RUSTUP_AUTO_INSTALL) ARGS=" + ($args -join ' ')
    )
}
if ($env:FAKE_RUSTUP_EXIT) { exit [int]$env:FAKE_RUSTUP_EXIT }
"#,
    );

    #[cfg(windows)]
    write(
        &bin.join("rustup.cmd"),
        "@echo off\r\npwsh -NoProfile -File \"%~dp0rustup.ps1\" %*\r\nexit /b %ERRORLEVEL%\r\n",
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let executable = bin.join("rustup");
        write(
            &executable,
            "#!/bin/sh\nexec pwsh -NoProfile -File \"$(dirname \"$0\")/rustup.ps1\" \"$@\"\n",
        );
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(executable, permissions).unwrap();
    }
}

#[test]
fn generated_recipes_are_split_by_responsibility() {
    let generated = generated();
    for import in ["setup.just", "checks.just", "container.just"] {
        let declaration = if import == "container.just" {
            format!("import? '{import}'")
        } else {
            format!("import '{import}'")
        };
        assert!(generated.hub.contains(&declaration));
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
         cargo +{{ rust_nightly }} coverage-gate {{ anvil_explicit_package_args }} run \
         --no-coverage-target aarch64-pc-windows-msvc"
    ));
    assert!(!recipes.contains("Invoke-AnvilLcovReport"));
    assert!(!recipes.contains("llvm-cov.rsp"));
}

#[test]
fn setup_uses_lazy_inventory_and_exact_install_policy() {
    let generated = generated();
    let recipes = &generated.setup;
    for contract in [
        "installed_cargo_tools := `cargo install --list`",
        "semver_matches(installed, \">=\" + minimum)",
        "cargo install --locked --version =",
        "cargo binstall --no-confirm --locked --version =",
        "anvil-toolchain-stable-install installer=\"install\": (anvil-tool-cargo-each-install installer)",
    ] {
        assert!(recipes.contains(contract), "missing setup contract: {contract}");
    }
    assert!(generated.setup.contains("set lazy"));
    assert!(!recipes.contains("--disable-strategies compile"));
}

#[test]
fn validation_disables_rustup_auto_install_and_preserves_tool_failures() {
    if !Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let catalog = Catalog::anvil()
        .into_builder()
        .replace_artifact(
            cargo_anvil::artifacts::justfile::recipe("anvil-tool-cargo-each-validate-prereqs")
                .unwrap()
                .with_body("anvil-tool-cargo-each-validate-prereqs:\n"),
        )
        .build()
        .unwrap();
    let generated = generated_with_catalog(&catalog);
    let root = generated.temp.path();
    let fake_bin = install_fake_cargo(root);
    install_fake_rustup(&fake_bin);

    let cargo_log = root.join("validation-cargo.log");
    let cargo_failure = Command::new("just")
        .arg("anvil-component-default-clippy-validate-prereqs")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("RUSTUP_TOOLCHAIN", "selected")
        .env("FAKE_CARGO_LOG", &cargo_log)
        .env("FAKE_CARGO_EXIT", "23")
        .output()
        .unwrap();
    assert!(!cargo_failure.status.success());
    let cargo_call = std::fs::read_to_string(&cargo_log).unwrap();
    assert!(cargo_call.contains("RUSTUP_AUTO_INSTALL=0"), "{cargo_call}");
    assert!(cargo_call.contains("clippy --version"), "{cargo_call}");
    assert!(!cargo_call.contains("install --locked"), "{cargo_call}");

    let rustup_log = root.join("validation-rustup.log");
    let rustup_failure = Command::new("just")
        .arg("anvil-msrv-test-validate-prereqs")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("FAKE_RUSTUP_LOG", &rustup_log)
        .env("FAKE_RUSTUP_EXIT", "19")
        .output()
        .unwrap();
    assert!(!rustup_failure.status.success());
    let rustup_call = std::fs::read_to_string(&rustup_log).unwrap();
    assert!(rustup_call.contains("RUSTUP_AUTO_INSTALL=0"), "{rustup_call}");
    assert!(rustup_call.contains("run 1.95 rustc --version"), "{rustup_call}");

    let inventory_failure = Command::new("just")
        .args(["_check-tool", "cargo-example", "1.2.3"])
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("FAKE_CARGO_OUTPUT", "selected Cargo failed")
        .env("FAKE_CARGO_EXIT", "17")
        .output()
        .unwrap();
    assert!(!inventory_failure.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&inventory_failure.stdout),
        String::from_utf8_lossy(&inventory_failure.stderr)
    );
    assert!(diagnostic.contains("backtick failed with exit code"), "{diagnostic}");
    assert!(!diagnostic.contains("cargo install --locked --version"), "{diagnostic}");
}

#[test]
fn released_domain_tool_versions_are_pinned() {
    let generated = generated();
    let recipes = &generated.setup;
    for pin in [
        "cargo_aprz_version := \"1.2.0\"",
        "cargo_coverage_gate_version := \"0.6.0\"",
        "cargo_delta_version := \"0.4.0\"",
        "cargo_each_version := \"0.4.0\"",
    ] {
        assert!(recipes.contains(pin), "missing released tool pin: {pin}");
    }
    assert!(!recipes.contains("cargo_semver_checks_version"));
}

#[test]
fn ensure_target_dir_creates_a_missing_directory_and_is_idempotent() {
    let generated = generated();
    let root = generated.temp.path();
    let target = root.join("target");
    if target.exists() {
        std::fs::remove_dir_all(&target).unwrap();
    }

    for _ in 0..2 {
        let output = run_just(root, &["_ensure-target-dir"], &[("ANVIL_IMPACT", "off")]);
        assert!(
            output.status.success(),
            "_ensure-target-dir failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(target.is_dir());
    }
}

#[test]
fn msrv_recipes_use_only_the_declared_root_version() {
    let generated = generated();
    assert!(generated.checks.contains("cargo '+{workspace-rust-version}' test"));
    assert!(
        generated
            .setup
            .contains("rustup toolchain install '{workspace-rust-version}' --profile minimal")
    );
    assert!(generated.setup.contains("rustup run '{workspace-rust-version}' rustc --version"));
    assert!(
        generated
            .checks
            .contains("anvil_msrv_selection := if workspace_rust_version == \"\"")
    );
    assert!(!generated.all_recipes().contains("ANVIL_MSRV_TOOLCHAIN"));
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
fn doc_tests_select_only_doctest_capable_packages() {
    if !Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let catalog = Catalog::anvil()
        .into_builder()
        .replace_artifact(
            cargo_anvil::artifacts::justfile::recipe("anvil-doc-test-validate-prereqs")
                .unwrap()
                .with_body("anvil-doc-test-validate-prereqs:\n"),
        )
        .build()
        .unwrap();
    let generated = generated_with_catalog(&catalog);
    let root = generated.temp.path();
    let impact = root.join("impact");
    std::fs::create_dir_all(&impact).unwrap();
    write(
        &impact.join("affected.packages"),
        "fixture@0.1.0\nbin-only@0.1.0\nmacro-package@0.1.0\n",
    );
    let log = root.join("cargo.log");
    let fake_bin = install_fake_cargo(root);
    let output = Command::new("just")
        .arg("anvil-doc-test")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env_remove("RUSTUP_TOOLCHAIN")
        .env("ANVIL_IMPACT", "consume")
        .env("ANVIL_IMPACT_INPUT_DIR", &impact)
        .env("FAKE_CARGO_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "doctest selection failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = std::fs::read_to_string(&log).unwrap();
    let doc_commands = commands.lines().filter(|line| line.contains("test --doc")).collect::<Vec<_>>();
    assert_eq!(doc_commands.len(), 2, "both feature configurations must run:\n{commands}");
    for command in doc_commands {
        assert!(
            command.contains("--package fixture@0.1.0"),
            "library package was dropped:\n{command}"
        );
        assert!(
            command.contains("--package macro-package@0.1.0"),
            "proc-macro package was dropped:\n{command}"
        );
        assert!(
            !command.contains("bin-only"),
            "bin-only package reached cargo test --doc:\n{command}"
        );
    }

    std::fs::remove_file(&log).unwrap();
    write(&impact.join("affected.packages"), "bin-only@0.1.0\n");
    let skipped = Command::new("just")
        .arg("anvil-doc-test")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env_remove("RUSTUP_TOOLCHAIN")
        .env("ANVIL_IMPACT", "consume")
        .env("ANVIL_IMPACT_INPUT_DIR", &impact)
        .env("FAKE_CARGO_LOG", &log)
        .output()
        .unwrap();
    assert!(skipped.status.success());
    assert!(
        String::from_utf8_lossy(&skipped.stdout).contains("no affected doctest-capable packages"),
        "skip reason missing:\n{}",
        String::from_utf8_lossy(&skipped.stdout)
    );
    assert!(!log.exists(), "bin-only selection must not invoke cargo test --doc");
}

#[test]
fn loom_rejects_declared_support_without_a_loom_target() {
    if !Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let catalog = Catalog::anvil()
        .into_builder()
        .replace_artifact(
            cargo_anvil::artifacts::justfile::recipe("anvil-loom-validate-prereqs")
                .unwrap()
                .with_body("anvil-loom-validate-prereqs:\n"),
        )
        .build()
        .unwrap();
    let generated = generated_with_catalog(&catalog);
    let root = generated.temp.path();
    let fake_bin = install_fake_cargo(root);
    let missing_target = r#"{"packages":[{"name":"fixture","version":"0.1.0","features":{"loom":[]},"dependencies":[],"targets":[{"name":"ordinary","kind":["test"],"required-features":[]}]}]}"#;
    let rejected = Command::new("just")
        .arg("anvil-loom")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("ANVIL_IMPACT", "off")
        .env("FAKE_CARGO_METADATA", missing_target)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(diagnostic.contains("fixture@0.1.0"), "{diagnostic}");
    assert!(diagnostic.contains("required-features"), "{diagnostic}");

    let valid_target = r#"{"packages":[{"name":"fixture","version":"0.1.0","features":{"loom":[]},"dependencies":[],"targets":[{"name":"loom","kind":["test"],"required-features":["loom"]}]}]}"#;
    let log = root.join("loom.log");
    let accepted = Command::new("just")
        .arg("anvil-loom")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("ANVIL_IMPACT", "off")
        .env("FAKE_CARGO_METADATA", valid_target)
        .env("FAKE_CARGO_LOG", &log)
        .output()
        .unwrap();
    assert!(
        accepted.status.success(),
        "valid Loom target failed:\n{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let invocation = std::fs::read_to_string(log).unwrap();
    assert!(
        invocation.contains("--each-target test --target-required-feature loom"),
        "{invocation}"
    );
}

#[test]
fn mutants_diff_includes_committed_and_uncommitted_changes() {
    if (cfg!(windows) && cfg!(target_arch = "aarch64"))
        || !Command::new("pwsh")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        || Command::new("git").arg("--version").output().is_err()
    {
        return;
    }

    let catalog = Catalog::anvil()
        .into_builder()
        .replace_artifact(
            cargo_anvil::artifacts::justfile::recipe("anvil-mutants-diff-validate-prereqs")
                .unwrap()
                .with_body("anvil-mutants-diff-validate-prereqs:\n"),
        )
        .build()
        .unwrap();
    let generated = generated_with_catalog(&catalog);
    let root = generated.temp.path();
    let run_git = |args: &[&str]| {
        let output = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_git(&["init", "-q"]);
    run_git(&["config", "user.email", "fixture@example.invalid"]);
    run_git(&["config", "user.name", "Fixture"]);
    run_git(&["config", "core.autocrlf", "false"]);
    run_git(&["config", "core.safecrlf", "false"]);
    run_git(&["config", "commit.gpgsign", "false"]);
    run_git(&["add", "-A"]);
    run_git(&["commit", "-qm", "base"]);
    let base = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let base = base.trim();

    write(
        &root.join("crate/src/lib.rs"),
        "pub fn value() -> u8 { 1 }\npub fn committed() {}\n",
    );
    run_git(&["add", "-A"]);
    run_git(&["commit", "-qm", "committed"]);
    write(
        &root.join("crate/src/lib.rs"),
        "pub fn value() -> u8 { 1 }\npub fn committed() {}\npub fn uncommitted() {}\n",
    );

    let fake_bin = install_fake_cargo(root);
    let log = root.join("mutants.log");
    let output = Command::new("just")
        .arg("anvil-mutants-diff")
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("ANVIL_IMPACT", "off")
        .env("BASE_REF", base)
        .env("RUNNER_TEMP", root)
        .env("FAKE_CARGO_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mutants diff failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let invocation = std::fs::read_to_string(log).unwrap();
    assert!(invocation.contains("mutants --in-diff"), "{invocation}");
    let diff = std::fs::read_to_string(root.join("anvil-mutants-diff.diff")).unwrap();
    assert!(diff.contains("committed"), "{diff}");
    assert!(diff.contains("uncommitted"), "{diff}");
}

#[test]
fn examples_honor_default_exclusions_and_explicit_selection() {
    if !Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let catalog = Catalog::anvil()
        .into_builder()
        .replace_artifact(
            cargo_anvil::artifacts::justfile::recipe("anvil-examples-validate-prereqs")
                .unwrap()
                .with_body("anvil-examples-validate-prereqs:\n"),
        )
        .build()
        .unwrap();
    let generated = generated_with_catalog(&catalog);
    let root = generated.temp.path();
    let fake_bin = install_fake_cargo(root);
    let metadata = r#"{"packages":[{"name":"fixture","version":"0.1.0","metadata":{"anvil":{"examples":{"no-run":["blocked"]}}},"targets":[{"name":"ok","kind":["example"]},{"name":"blocked","kind":["example"]}]}]}"#;
    let default_log = root.join("examples-default.log");
    let default_run = Command::new("just")
        .args(["anvil-examples", "--run"])
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("ANVIL_IMPACT", "off")
        .env("FAKE_CARGO_METADATA", metadata)
        .env("FAKE_CARGO_LOG", &default_log)
        .output()
        .unwrap();
    assert!(
        default_run.status.success(),
        "default example execution failed:\n{}",
        String::from_utf8_lossy(&default_run.stderr)
    );
    let default_calls = std::fs::read_to_string(default_log).unwrap();
    let run_calls = default_calls.lines().filter(|line| line.contains(" run ")).collect::<Vec<_>>();
    assert_eq!(run_calls.len(), 1, "{default_calls}");
    assert!(
        run_calls[0].contains("--example ok"),
        "eligible example was not run:\n{default_calls}"
    );
    assert!(!run_calls[0].contains("blocked"), "excluded example was run:\n{default_calls}");

    let explicit_log = root.join("examples-explicit.log");
    let explicit = Command::new("just")
        .args(["anvil-examples", "--run", "--package", "fixture", "--example", "blocked"])
        .current_dir(root)
        .env("PATH", prepend_path(&fake_bin))
        .env("ANVIL_IMPACT", "off")
        .env("FAKE_CARGO_METADATA", metadata)
        .env("FAKE_CARGO_LOG", &explicit_log)
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "explicit example execution failed:\n{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let explicit_calls = std::fs::read_to_string(explicit_log).unwrap();
    assert!(
        explicit_calls
            .lines()
            .any(|line| line.contains(" run ") && line.contains("--example blocked")),
        "explicit selection must override the default exclusion:\n{explicit_calls}"
    );
}

#[test]
fn miri_profiles_set_their_distinct_environment() {
    if !Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    let mut builder = Catalog::anvil().into_builder();
    for recipe_name in [
        "anvil-miri-tree-borrows-validate-prereqs",
        "anvil-miri-strict-provenance-validate-prereqs",
        "anvil-miri-race-coverage-validate-prereqs",
    ] {
        builder = builder.replace_artifact(
            cargo_anvil::artifacts::justfile::recipe(recipe_name)
                .unwrap()
                .with_body(format!("{recipe_name}:\n")),
        );
    }
    let generated = generated_with_catalog(&builder.build().unwrap());
    let root = generated.temp.path();
    let fake_bin = install_fake_cargo(root);

    for (recipe, miri_flags, rust_flags) in [
        ("anvil-miri-tree-borrows", "-Zmiri-tree-borrows", "--cfg miri_tree_borrows"),
        (
            "anvil-miri-strict-provenance",
            "-Zmiri-strict-provenance",
            "--cfg miri_strict_provenance",
        ),
        ("anvil-miri-race-coverage", "-Zmiri-many-seeds=", "--cfg miri_race_coverage"),
    ] {
        let log = root.join(format!("{recipe}.log"));
        let output = Command::new("just")
            .arg(recipe)
            .current_dir(root)
            .env("PATH", prepend_path(&fake_bin))
            .env("ANVIL_IMPACT", "off")
            .env("FAKE_CARGO_LOG", &log)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{recipe} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let invocation = std::fs::read_to_string(log).unwrap();
        assert!(invocation.contains(miri_flags), "{invocation}");
        assert!(invocation.contains(rust_flags), "{invocation}");
        assert!(invocation.contains("miri test"), "{invocation}");
    }
}

#[test]
fn stable_toolchain_selection_preserves_override_and_fallback_precedence() {
    let generated = generated();
    let root = generated.temp.path();
    let fake_bin = install_fake_cargo(root);
    let path = prepend_path(&fake_bin);
    let evaluate = |environment: &[(&str, &str)]| {
        let mut command = Command::new("just");
        command
            .args(["--evaluate", "anvil_stable_toolchain_arg"])
            .current_dir(root)
            .env("PATH", &path)
            .env_remove("RUSTUP_TOOLCHAIN");
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().unwrap()
    };

    let environment = evaluate(&[("RUSTUP_TOOLCHAIN", "selected")]);
    assert!(environment.status.success());
    assert_eq!(String::from_utf8(environment.stdout).unwrap().trim(), "");

    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    let toolchain_file = evaluate(&[]);
    assert!(toolchain_file.status.success());
    assert_eq!(String::from_utf8(toolchain_file.stdout).unwrap().trim(), "");

    std::fs::remove_file(root.join("rust-toolchain.toml")).unwrap();
    let fallback = evaluate(&[]);
    assert!(fallback.status.success());
    assert_eq!(String::from_utf8(fallback.stdout).unwrap().trim(), "'+1.95'");
}

#[test]
fn default_component_install_targets_the_effective_stable_selection() {
    let generated = generated();
    let root = generated.temp.path();
    let fake_bin = install_fake_cargo(root);
    install_fake_rustup(&fake_bin);
    let rustup_log = root.join("component-rustup.log");
    let run = |environment: &[(&str, &str)]| {
        let mut command = Command::new("just");
        command
            .args(["--set", "workspace_rust_version", "1.95", "_install-component", "default", "clippy"])
            .current_dir(root)
            .env("PATH", prepend_path(&fake_bin))
            .env("FAKE_RUSTUP_LOG", &rustup_log)
            .env_remove("RUSTUP_TOOLCHAIN");
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().unwrap()
    };

    let fallback = run(&[]);
    assert!(fallback.status.success());
    let fallback_stdout = std::fs::read_to_string(&rustup_log).unwrap();
    assert!(
        fallback_stdout.contains("component add --toolchain 1.95 clippy"),
        "unexpected fallback plan: {fallback_stdout}"
    );

    std::fs::write(&rustup_log, "").unwrap();
    let selected = run(&[("RUSTUP_TOOLCHAIN", "selected")]);
    assert!(selected.status.success());
    let selected_output = std::fs::read_to_string(&rustup_log).unwrap();
    assert!(selected_output.contains("component add clippy"));
    assert!(!selected_output.contains("--toolchain"));

    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    std::fs::write(&rustup_log, "").unwrap();
    let file_selected = run(&[]);
    assert!(file_selected.status.success());
    let file_output = std::fs::read_to_string(&rustup_log).unwrap();
    assert!(file_output.contains("component add clippy"));
    assert!(!file_output.contains("--toolchain"));
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
    assert!(dockerfile.contains("setup_toolchain=stable"));
    assert!(dockerfile.contains("RUSTUP_TOOLCHAIN=\"${setup_toolchain}\" just anvil-setup"));
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

    std::fs::remove_file(root.join("Cargo.toml")).unwrap();
    let container_value = run_just(root, &["_anvil-root-msrv"], &[("ANVIL_RUST_VERSION", "container-msrv")]);
    assert!(container_value.status.success());
    assert_eq!(String::from_utf8(container_value.stdout).unwrap().trim(), "container-msrv");
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

#[test]
fn removing_the_container_group_keeps_the_remaining_recipes_parseable() {
    let mut builder = Catalog::anvil().into_builder();
    for artifact in cargo_anvil::artifacts::container::all() {
        builder = builder.without_artifact(artifact);
    }
    let catalog = builder.build().unwrap();
    let generated = generated_with_catalog(&catalog);
    assert!(!generated.temp.path().join(".anvil/container.just").exists());

    let output = run_just(generated.temp.path(), &["--dump"], &[]);
    assert!(
        output.status.success(),
        "the optional container import must permit a container-free catalog:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn container_tag_rejects_non_file_toolchains_and_linked_inputs() {
    if !Command::new("pwsh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return;
    }

    for spelling in ["rust-toolchain", "rust-toolchain.toml"] {
        let generated = generated();
        let path = generated.temp.path().join(spelling);
        std::fs::create_dir(&path).unwrap();
        write(&path.join("payload"), "not a toolchain file\n");
        let output = run_just(generated.temp.path(), &["anvil-container-tag"], &[]);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(diagnostic.contains(spelling), "{diagnostic}");
        assert!(diagnostic.contains("regular file"), "{diagnostic}");
    }

    for relative in ["rust-toolchain", ".anvil/container/linked-input"] {
        let generated = generated();
        let root = generated.temp.path();
        let target = root.join("link-target");
        write(&target, "linked content\n");
        let link = root.join(relative);
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        if symlink_file(&target, &link).is_err() {
            continue;
        }
        let output = run_just(root, &["anvil-container-tag"], &[]);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(diagnostic.contains("regular files"), "{diagnostic}");
        assert!(diagnostic.contains(relative.rsplit('/').next().unwrap()), "{diagnostic}");
    }
}

#[cfg(unix)]
fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git").current_dir(root).args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
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
