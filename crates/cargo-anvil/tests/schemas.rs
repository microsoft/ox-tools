// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))] // miri can't sandbox FS ops these tests do (TempDir, assert_cmd, etc.)
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

//! Schema validation for emitted files.
//!
//! Each test generates the full output of `cargo anvil` for a
//! particular backend into a tempdir, then runs an external schema
//! validator (`actionlint`, `taplo`, `just`) over the relevant files.
//!
//! If the validator isn't installed, the test is skipped — never failed.
//! in cloud workflows we enforce installation via the `anvil-tools-install` recipe;
//! locally the test suite degrades gracefully.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration tests favor concise assertions over Result plumbing"
)]

use std::path::Path;
use std::process::{Command, Output};

use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn empty_workspace() -> TempDir {
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

fn run_with_backend(backend: &str) -> TempDir {
    let tmp = empty_workspace();
    let args = Cli {
        backends: vec![backend.to_owned()],
        no_backends: false,
        dry_run: false,
        force: false,
    };
    run_update(&cargo_anvil::Catalog::anvil(), &args, tmp.path()).unwrap();
    tmp
}

fn try_run(cmd: &mut Command) -> Option<Output> {
    match cmd.output() {
        Ok(o) => Some(o),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("unexpected error spawning validator: {e}"),
    }
}

fn prepend_path(command: &mut Command, directory: &Path) {
    let paths = std::iter::once(directory.to_path_buf()).chain(
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>()),
    );
    command.env("PATH", std::env::join_paths(paths).unwrap());
}

fn write_fake_cargo_tools(directory: &Path) {
    std::fs::create_dir_all(directory).unwrap();

    #[cfg(windows)]
    {
        write(
            &directory.join("cargo.cmd"),
            "@echo off\r\n\
             if \"%1 %2\"==\"install --list\" exit /b 0\r\n\
             if \"%1\"==\"binstall\" if defined ANVIL_TEST_TOOLCHAIN_FILE \
             > \"%ANVIL_TEST_TOOLCHAIN_FILE%\" echo %RUSTUP_TOOLCHAIN%\r\n\
             exit /b 0\r\n",
        );
        write(&directory.join("cargo-binstall.cmd"), "@echo off\r\nexit /b 0\r\n");
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        write(
            &directory.join("cargo"),
            "#!/bin/sh\n\
             if [ \"$1 $2\" = \"install --list\" ]; then exit 0; fi\n\
             if [ \"$1\" = \"binstall\" ] && [ -n \"$ANVIL_TEST_TOOLCHAIN_FILE\" ]; then\n\
               printf '%s\\n' \"$RUSTUP_TOOLCHAIN\" > \"$ANVIL_TEST_TOOLCHAIN_FILE\"\n\
             fi\n\
             exit 0\n",
        );
        write(&directory.join("cargo-binstall"), "#!/bin/sh\nexit 0\n");
        for name in ["cargo", "cargo-binstall"] {
            std::fs::set_permissions(directory.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

#[test]
fn taplo_validates_emitted_toml_files() {
    let tmp = run_with_backend("github");
    let mut cmd = Command::new("taplo");
    cmd.args([
        "check",
        "Cargo.toml",
        "deny.toml",
        "rustfmt.toml",
        ".delta.toml",
        ".anvil/manifest.toml",
    ])
    .current_dir(tmp.path());

    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: taplo not installed");
        return;
    };
    assert!(
        out.status.success(),
        "taplo rejected emitted TOML:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn handwritten_root_justfile_receives_the_required_unstable_setting() {
    let tmp = empty_workspace();
    write(&tmp.path().join("Justfile"), "# Existing repository recipes.\n");
    run_update(
        &cargo_anvil::Catalog::anvil(),
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        tmp.path(),
    )
    .unwrap();

    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile", "--list"]).current_dir(tmp.path());
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: just not installed");
        return;
    };
    assert!(
        out.status.success(),
        "generated imports must enable their required unstable features:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn container_variadic_command_preserves_the_whitespace_contract() {
    let tmp = run_with_backend("github");
    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile", "anvil-container", "just", "--version"])
        .current_dir(tmp.path())
        .env("ANVIL_IN_CONTAINER", "1");
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: just not installed");
        return;
    };
    assert!(
        out.status.success(),
        "container command arguments must remain a whitespace-delimited string:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn setup_recognizes_an_explicit_prebuilt_cargo_binstall() {
    let tmp = run_with_backend("github");
    let bin = tmp.path().join("fake-bin");
    write_fake_cargo_tools(&bin);
    let cargo_binstall = bin.join(if cfg!(windows) { "cargo-binstall.cmd" } else { "cargo-binstall" });

    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile", "--evaluate", "cargo_binstall_found"])
        .current_dir(tmp.path())
        .env("ANVIL_CARGO_BINSTALL", cargo_binstall);
    prepend_path(&mut cmd, &bin);
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: just not installed");
        return;
    };
    assert!(
        out.status.success(),
        "cargo-binstall detection failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
}

#[test]
fn cargo_tool_install_uses_the_configured_toolchain() {
    let tmp = run_with_backend("github");
    let bin = tmp.path().join("fake-bin");
    let observed = tmp.path().join("observed-toolchain");
    write_fake_cargo_tools(&bin);
    let cargo_binstall = bin.join(if cfg!(windows) { "cargo-binstall.cmd" } else { "cargo-binstall" });

    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile", "_install-tool", "cargo-each", "0.8.0", "binstall"])
        .current_dir(tmp.path())
        .env("RUSTUP_TOOLCHAIN", "1.95")
        .env("ANVIL_CARGO_BINSTALL", cargo_binstall)
        .env("ANVIL_CARGO_INSTALL_TOOLCHAIN", "stable")
        .env("ANVIL_TEST_TOOLCHAIN_FILE", &observed);
    prepend_path(&mut cmd, &bin);
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: just not installed");
        return;
    };
    assert!(
        out.status.success(),
        "Cargo tool installation failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(observed).unwrap().trim(), "stable");
}

#[test]
fn actionlint_validates_emitted_workflows() {
    let tmp = run_with_backend("github");
    let mut workflows = std::fs::read_dir(tmp.path().join(".github/workflows"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "yml" || extension == "yaml"))
        .collect::<Vec<_>>();
    workflows.sort();
    assert!(!workflows.is_empty(), "the GitHub backend must emit workflows to validate");
    let mut cmd = Command::new("actionlint");
    cmd.args(workflows).current_dir(tmp.path());
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: actionlint not installed");
        return;
    };
    assert!(
        out.status.success(),
        "actionlint rejected emitted workflows:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn just_lists_emitted_recipes() {
    let tmp = run_with_backend("github");
    let mut cmd = Command::new("just");
    cmd.args(["--justfile", "Justfile", "--list"]).current_dir(tmp.path());
    let Some(out) = try_run(&mut cmd) else {
        eprintln!("skipping: just not installed");
        return;
    };
    assert!(
        out.status.success(),
        "just --list failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    for (recipe, summary) in [
        (
            "anvil-full",
            "# Run both pull-request and scheduled tiers across the full workspace.",
        ),
        ("anvil-pr", "# Run every pull-request check group."),
        ("anvil-pr-fast", "# Run pull-request static analysis and repository policy checks."),
        ("anvil-pr-msrv", "# Run pull-request compatibility tests under the declared MSRV."),
        ("anvil-pr-mutants", "# Run pull-request mutation testing for changed code."),
        ("anvil-pr-runtime-analysis", "# Run pull-request Miri, Careful, and Loom checks."),
        ("anvil-pr-slow", "# Run all slow pull-request groups sequentially."),
        ("anvil-pr-test", "# Run pull-request tests, coverage, doctests, and examples."),
        ("anvil-scheduled", "# Run every scheduled check group across the full workspace."),
        (
            "anvil-scheduled-advisories",
            "# Recheck external advisories, dependency risk, and evolving lints.",
        ),
        (
            "anvil-scheduled-exhaustive",
            "# Run full mutation, feature-combination, and benchmark checks.",
        ),
        (
            "anvil-scheduled-runtime-analysis",
            "# Run full-workspace Miri profiles on the schedule.",
        ),
        (
            "anvil-scheduled-test",
            "# Run full-workspace tests, coverage, doctests, and examples.",
        ),
    ] {
        let found = listing.lines().any(|line| {
            let line = line.trim();
            line.starts_with(recipe) && line.contains(summary)
        });
        assert!(found, "`just --list` did not contain '{recipe}\\s+{summary}':\n{listing}");
    }
}

#[test]
fn emitted_powershell_scripts_disable_profiles() {
    let tmp = run_with_backend("github");
    let mut script_count = 0;
    let mut workflow_shell_count = 0;
    for entry in walkdir::WalkDir::new(tmp.path()) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() {
            continue;
        }
        let contents = std::fs::read_to_string(entry.path()).unwrap();
        assert!(
            !contents.contains("[script(\"pwsh\")]"),
            "{} contains a profile-loading PowerShell script attribute",
            entry.path().display()
        );
        assert!(
            !contents.lines().any(|line| line.trim() == "shell: pwsh"),
            "{} contains a profile-loading GitHub Actions PowerShell shell",
            entry.path().display()
        );
        script_count += contents.matches("[script(\"pwsh\", \"-NoProfile\")]").count();
        workflow_shell_count += contents.matches("shell: pwsh -NoProfile").count();
    }
    assert!(script_count > 0, "expected emitted PowerShell script recipes");
    assert!(workflow_shell_count > 0, "expected an emitted GitHub Actions PowerShell shell");
}

#[test]
fn ado_yaml_emitted_files_have_consistent_indent() {
    let tmp = run_with_backend("ado");
    let mut count = 0_usize;
    for entry in [tmp.path().join(".pipelines"), tmp.path().join(".anvil/ado")]
        .into_iter()
        .flat_map(walkdir::WalkDir::new)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|s| s == "yml"))
    {
        count += 1;
        let text = std::fs::read_to_string(entry.path()).unwrap();
        assert!(!text.contains('\t'), "tab indentation in {}", entry.path().display());
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start_matches(' ').len();
            assert_eq!(
                indent % 2,
                0,
                "non-aligned indent ({indent} spaces) in {}: {line}",
                entry.path().display()
            );
        }
    }
    assert!(count >= 11, "expected at least 11 emitted ADO yml files, got {count}");
}
