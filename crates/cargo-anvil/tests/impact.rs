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

use cargo_anvil::Catalog;
use cargo_anvil::test_support::{Cli, run_update};
use tempfile::TempDir;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn generated_workspace() -> TempDir {
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
    temp
}

fn just(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("just");
    command.current_dir(root).args(args);
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
    let recipes = std::fs::read_to_string(temp.path().join(".anvil/anvil.just")).unwrap();

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
