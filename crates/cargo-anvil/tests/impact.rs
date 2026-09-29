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
    temp
}

fn generated_workspace() -> TempDir {
    generated_workspace_with(&Catalog::anvil())
}

fn just(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command
        .current_dir(root)
        .args(args)
        .env_remove("BASE_REF")
        .env_remove("SYSTEM_PULLREQUEST_TARGETBRANCH")
        .env_remove("GITHUB_BASE_REF");
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
    let recipes = std::fs::read_to_string(temp.path().join(".anvil/checks.just")).unwrap();

    for tier in ["modified", "affected", "required"] {
        assert!(recipes.contains(&format!("--{tier} --format packages")));
        assert!(recipes.contains(&format!("--output target/anvil/impact/{tier}.packages")));
        assert!(recipes.contains(&format!("anvil_{tier}_selection")));
    }
    assert!(recipes.contains("delta --config .delta.toml impact --base-ref"));
    assert!(!recipes.contains("_anvil-impact-format"));
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
    let path = std::env::join_paths(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|path| !path.ends_with("target/debug") && !path.ends_with("target\\debug")),
    )
    .unwrap();
    Command::new("just")
        .arg("anvil-examples")
        .current_dir(root)
        .env("PATH", path)
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
        assert!(stdout(&output).contains("no affected packages; skipping"));
    }
    assert!(
        !temp.path().join("target/anvil/careful-sysroot.id").exists(),
        "careful must not rewrite its marker for an empty selection"
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
    write(
        &temp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crate\"]\n",
    );

    for invocation in [
        vec!["anvil-msrv-test-setup", "install"],
        vec!["anvil-msrv-test-validate-prereqs"],
        vec!["anvil-msrv-test"],
    ] {
        let output = just(temp.path(), &invocation, &[("ANVIL_IMPACT", "off")]);
        assert!(
            output.status.success(),
            "{} must no-op without a root MSRV:\n{}",
            invocation[0],
            stderr(&output)
        );
        assert!(stdout(&output).contains("no root MSRV declared; skipping"));
    }
}
