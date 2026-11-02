// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in integration tests"
)]

use std::path::Path;
use std::process::{Command, Output};

use cargo_anvil::test_support::{Cli, run_update};
use cargo_anvil::{Catalog, artifacts};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn generated_workspace_with(catalog: &Catalog) -> TempDir {
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
    install_fake_cargo_each(temp.path());
    temp
}

fn generated_workspace() -> TempDir {
    generated_workspace_with(&Catalog::anvil())
}

const FAKE_CARGO_EACH: &str = r#"
$eachIndex = -1
for ($i = 0; $i -lt $args.Count; $i++) {
    if ($args[$i] -eq 'each') {
        $eachIndex = $i
        break
    }
}
if ($eachIndex -lt 0) {
    & '__REAL_CARGO__' @args
    exit $LASTEXITCODE
}
$options = @($args[($eachIndex + 1)..($args.Count - 1)])
$selection = @()
$commandIndex = -1
for ($i = 0; $i -lt $options.Count; $i++) {
    if ($options[$i] -eq '--') {
        $commandIndex = $i
        break
    }
}
if ($commandIndex -lt 0) {
    for ($i = 0; $i -lt $options.Count; $i++) {
        if ($options[$i] -in @('cargo', 'pwsh', 'rustup')) {
            $commandIndex = $i - 1
            break
        }
    }
}
if ($commandIndex -lt 0) {
    $dryRunIndex = [Array]::IndexOf($options, '--dry-run')
    if ($dryRunIndex -ge 0 -and ($dryRunIndex + 1) -lt $options.Count) {
        $commandIndex = $dryRunIndex
    }
}
if ($commandIndex -lt 0) {
    [Console]::Error.WriteLine('cargo each fixture could not locate the command')
    exit 2
}
if ($options -contains '--none') {
    [Console]::Error.WriteLine('cargo each: selection resolved to no work; nothing to do')
    exit 0
}
for ($i = 0; $i -lt $commandIndex; $i++) {
    if ($options[$i] -eq '--package-file') {
        $path = $options[++$i]
        while (-not (Test-Path -LiteralPath $path -PathType Leaf) -and
               ($i + 1) -lt $commandIndex -and
               -not $options[$i + 1].StartsWith('--')) {
            $path += ' ' + $options[++$i]
        }
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            [Console]::Error.WriteLine("could not read package file '$path'")
            exit 2
        }
        $selection += @(Get-Content -LiteralPath $path | ForEach-Object { $_.Trim() } | Where-Object { $_ })
    } elseif ($options[$i] -eq '--workspace') {
        $selection += 'fixture@0.1.0'
    }
}
if ($selection.Count -eq 0) {
    [Console]::Error.WriteLine('cargo each: selection resolved to no work; nothing to do')
    exit 0
}
$command = @($options[($commandIndex + 1)..($options.Count - 1)])
if ($options -contains '--dry-run') {
    $manifest = Get-Content -LiteralPath (Join-Path (Get-Location) 'Cargo.toml') -Raw
    $workspaceRustVersion = if ($manifest -match 'rust-version\s*=\s*"([^"]+)"') {
        $Matches[1]
    } else {
        ''
    }
    foreach ($argument in $command) {
        Write-Output ($argument.Replace('{workspace-rust-version}', $workspaceRustVersion))
    }
    exit 0
}
$expanded = @()
foreach ($argument in $command) {
    if ($argument -eq '{packages}') {
        foreach ($package in $selection) {
            $expanded += '--package'
            $expanded += $package
        }
    } else {
        $expanded += $argument
    }
}
& $expanded[0] @($expanded[1..($expanded.Count - 1)])
exit $LASTEXITCODE
"#;

fn install_fake_cargo_each(root: &Path) {
    let bin = root.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let real_cargo = real_cargo.to_string_lossy().replace('\'', "''");
    let script = FAKE_CARGO_EACH.replace("__REAL_CARGO__", &real_cargo);
    write(&bin.join("cargo.ps1"), &script);
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
}

fn fixture_path(root: &Path) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(root.join("fake-bin")).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|path| !path.ends_with("target/debug") && !path.ends_with("target\\debug")),
        ),
    )
    .unwrap()
}

fn just(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command
        .current_dir(root)
        .args(args)
        .env_remove("BASE_REF")
        .env_remove("SYSTEM_PULLREQUEST_TARGETBRANCH")
        .env_remove("GITHUB_BASE_REF")
        .env("PATH", fixture_path(root));
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn impact_recipe_delegates_to_cargo_delta_package_outputs() {
    let temp = generated_workspace();
    let checks = std::fs::read_to_string(temp.path().join(".anvil/checks.just")).unwrap();
    let helpers = std::fs::read_to_string(temp.path().join(".anvil/helpers.just")).unwrap();

    for tier in ["modified", "affected", "required"] {
        assert!(checks.contains(&format!("--{tier} --format packages")));
        assert!(checks.contains(&format!("--output target/anvil/impact/{tier}.packages")));
        assert!(helpers.contains(&format!("anvil_{tier}_selection")));
    }
    assert!(checks.contains("delta --config .delta.toml impact --base-ref"));
    assert!(!checks.contains("_anvil-impact-format"));
}

#[test]
fn impact_off_is_a_tool_free_noop() {
    let temp = generated_workspace();
    let output = just(temp.path(), &["anvil-impact"], &[("ANVIL_IMPACT", "off")]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(!temp.path().join("target/anvil/impact").exists());
}

#[test]
fn invalid_impact_mode_fails_during_just_evaluation() {
    let temp = generated_workspace();
    let output = just(temp.path(), &["--evaluate", "anvil_affected_selection"], &[("ANVIL_IMPACT", "on")]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("ANVIL_IMPACT must be unset, 'consume', or 'off'"));
}

#[test]
fn consume_mode_quotes_the_injected_package_directory() {
    let temp = generated_workspace();
    let output = just(
        temp.path(),
        &["--evaluate", "anvil_affected_selection"],
        &[("ANVIL_IMPACT", "consume"), ("ANVIL_IMPACT_INPUT_DIR", "fixture cache")],
    );

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "--package-file 'fixture cache/affected.packages'");
}

#[test]
fn off_mode_selects_the_complete_workspace() {
    let temp = generated_workspace();
    let output = just(temp.path(), &["--evaluate", "anvil_required_selection"], &[("ANVIL_IMPACT", "off")]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "--workspace");
}

#[test]
fn explicit_package_arguments_are_omitted_only_for_the_complete_workspace() {
    let temp = generated_workspace();
    for (mode, expected) in [("off", ""), ("consume", "'{packages}'")] {
        let output = just(
            temp.path(),
            &["--evaluate", "anvil_explicit_package_args"],
            &[("ANVIL_IMPACT", mode)],
        );
        assert!(output.status.success(), "stderr:\n{}", stderr(&output));
        assert_eq!(stdout(&output).trim(), expected);
    }
}

#[test]
fn base_ref_precedence_is_evaluated_without_shell_logic() {
    let temp = generated_workspace();
    for (env, expected) in [
        (vec![("BASE_REF", "refs/heads/release")], "refs/heads/release"),
        (vec![("SYSTEM_PULLREQUEST_TARGETBRANCH", "refs/heads/ado-main")], "origin/ado-main"),
        (vec![("GITHUB_BASE_REF", "github-main")], "origin/github-main"),
    ] {
        let output = just(temp.path(), &["--evaluate", "anvil_base_ref"], &env);
        assert!(output.status.success(), "stderr:\n{}", stderr(&output));
        assert_eq!(stdout(&output).trim(), expected);
    }
}

fn examples_catalog() -> Catalog {
    Catalog::anvil()
        .into_builder()
        .replace_artifact(artifacts::justfile::recipe("anvil-examples").unwrap().with_body(
            "anvil-examples: anvil-examples-validate-prereqs anvil-impact\n    \
                     cargo each {{ anvil_affected_selection }} --once -- pwsh -NoProfile -File \
                     {{ quote(env(\"FAKE_CHECK_SCRIPT\")) }} '{packages}'\n",
        ))
        .replace_artifact(
            artifacts::justfile::recipe("anvil-examples-validate-prereqs")
                .unwrap()
                .with_body("anvil-examples-validate-prereqs:\n"),
        )
        .build()
        .unwrap()
}

fn catalog_with_noop_validations(names: &[&str]) -> Catalog {
    let mut builder = Catalog::anvil().into_builder();
    for name in names {
        builder = builder.replace_artifact(artifacts::justfile::recipe(name).unwrap().with_body(format!("{name}:\n")));
    }
    builder.build().unwrap()
}

fn run_consumed_examples(root: &Path, impact_dir: &Path, log: &Path) -> Output {
    let script = root.join("fake-check.ps1");
    write(&script, "Add-Content -LiteralPath $env:FAKE_CARGO_LOG -Value ($args -join ' ')\n");
    Command::new("just")
        .arg("anvil-examples")
        .current_dir(root)
        .env("PATH", fixture_path(root))
        .env("FAKE_CARGO_LOG", log)
        .env("FAKE_CHECK_SCRIPT", script)
        .env("ANVIL_IMPACT", "consume")
        .env("ANVIL_IMPACT_INPUT_DIR", impact_dir)
        .output()
        .unwrap()
}

#[test]
fn consumed_package_files_drive_skip_selection_and_missing_input_failure() {
    let temp = generated_workspace_with(&examples_catalog());
    let root = temp.path();
    let impact = root.join("impact fixture");
    std::fs::create_dir_all(&impact).unwrap();
    let package_file = impact.join("affected.packages");
    let log = root.join("cargo.log");

    write(&package_file, "fixture@0.1.0\n");
    let selected = run_consumed_examples(root, &impact, &log);
    assert!(selected.status.success(), "stderr:\n{}", stderr(&selected));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("--package fixture@0.1.0"),
        "selected packages must reach the generated public check\nlog: {logged}\nstdout: {}\nstderr: {}",
        stdout(&selected),
        stderr(&selected)
    );

    let _ = std::fs::remove_file(&log);
    write(&package_file, "");
    let empty = run_consumed_examples(root, &impact, &log);
    assert!(empty.status.success(), "stderr:\n{}", stderr(&empty));
    assert!(!log.exists(), "empty package files must not invoke the inner Cargo command");

    std::fs::remove_file(&package_file).unwrap();
    let missing = run_consumed_examples(root, &impact, &log);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("could not read package file"),
        "missing consumed input must fail closed:\n{}",
        stderr(&missing)
    );
}

#[test]
fn empty_affected_selection_skips_careful_and_mutants_before_side_effects() {
    let catalog = catalog_with_noop_validations(&["anvil-careful-validate-prereqs", "anvil-mutants-diff-validate-prereqs"]);
    let temp = generated_workspace_with(&catalog);
    let impact = temp.path().join("impact");
    std::fs::create_dir_all(&impact).unwrap();
    write(&impact.join("affected.packages"), "");

    for recipe in ["anvil-careful", "anvil-mutants-diff"] {
        let output = just(
            temp.path(),
            &[recipe],
            &[
                ("ANVIL_IMPACT", "consume"),
                ("ANVIL_IMPACT_INPUT_DIR", impact.to_str().unwrap()),
                ("BASE_REF", "definitely-missing"),
            ],
        );
        assert!(
            output.status.success(),
            "{recipe} must skip before tool/base/cache work:\n{}",
            stderr(&output)
        );
        if recipe == "anvil-careful" {
            let combined = format!("{}{}", stdout(&output), stderr(&output));
            assert!(combined.contains("selection resolved to no work; nothing to do"), "{combined}");
        }
    }
    assert!(
        !temp.path().join("target/anvil/careful").exists(),
        "careful must not create an identity target directory for an empty selection"
    );
    assert!(
        !temp.path().read_dir().is_ok_and(|mut entries| entries
            .any(|entry| entry.is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(".anvil-mutants-diff-")))),
        "mutants must not create a diff handoff for an empty selection"
    );
}

#[test]
fn msrv_recipes_are_noops_without_a_root_msrv() {
    let catalog = Catalog::anvil()
        .into_builder()
        .replace_artifact(
            artifacts::justfile::recipe("anvil-tool-cargo-each-install")
                .unwrap()
                .with_body("anvil-tool-cargo-each-install installer=\"install\":\n"),
        )
        .replace_artifact(
            artifacts::justfile::recipe("anvil-tool-cargo-each-validate-prereqs")
                .unwrap()
                .with_body("anvil-tool-cargo-each-validate-prereqs:\n"),
        )
        .build()
        .unwrap();
    let temp = generated_workspace_with(&catalog);
    let manifest_path = temp.path().join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    write(
        &manifest_path,
        &manifest.replace("[workspace.package]\nrust-version = \"1.95\"\n", ""),
    );

    for invocation in [
        vec!["--set", "workspace_rust_version", "", "anvil-msrv-test-setup", "install"],
        vec!["--set", "workspace_rust_version", "", "anvil-msrv-test-validate-prereqs"],
        vec!["--set", "workspace_rust_version", "", "anvil-msrv-test"],
    ] {
        let output = Command::new("just")
            .args(&invocation)
            .current_dir(temp.path())
            .env("ANVIL_IMPACT", "off")
            .env("PATH", fixture_path(temp.path()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} must no-op without a root MSRV:\n{}",
            invocation[0],
            stderr(&output)
        );
        let combined = format!("{}{}", stdout(&output), stderr(&output));
        assert!(combined.contains("selection resolved to no work; nothing to do"), "{combined}");
    }
}
