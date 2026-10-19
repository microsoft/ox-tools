// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Offline identity regressions using a directory replacement for crates.io.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn fixture(root: &Path) -> PathBuf {
    for directory in ["src", "local alpha/src", "vendor/alpha/src", ".cargo"] {
        fs::create_dir_all(root.join(directory)).expect("fixture directory");
    }
    fs::write(
        root.join("Cargo.toml"),
        r#"
[package]
name = "root-package"
version = "0.1.0"
edition = "2021"
[workspace]
members = ["local alpha"]
resolver = "2"
[dependencies]
alpha = "=0.1.0"
"#,
    )
    .expect("root manifest");
    fs::write(root.join("src/lib.rs"), "compile_error!(\"root package must not be selected\");").expect("root source");
    let manifest = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    fs::write(
        root.join("local alpha/Cargo.toml"),
        format!("{manifest}[lib]\nname = \"custom_library_name\"\n"),
    )
    .expect("local manifest");
    fs::write(root.join("local alpha/src/lib.rs"), "").expect("local source");
    fs::write(root.join("vendor/alpha/Cargo.toml"), manifest).expect("registry manifest");
    fs::write(
        root.join("vendor/alpha/src/lib.rs"),
        "compile_error!(\"registry package must not be selected\");",
    )
    .expect("registry source");
    fs::write(root.join("vendor/alpha/.cargo-checksum.json"), r#"{"files":{},"package":null}"#).expect("vendor checksum");
    fs::write(
        root.join(".cargo/config.toml"),
        r#"
[net]
offline = true
[source.crates-io]
replace-with = "fixture"
[source.fixture]
directory = "vendor"
"#,
    )
    .expect("offline configuration");
    root.join("Cargo.toml")
}

fn each(root: &Path) -> Command {
    let mut command = Command::cargo_bin("cargo-each").expect("cargo-each binary");
    command.current_dir(root).env("CARGO_NET_OFFLINE", "true").arg("each");
    command
}

fn list(root: &Path, selection: &[&str]) -> Vec<Value> {
    let output = each(root)
        .args(selection)
        .arg("--list-json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("structured selection")
}

#[test]
fn cargo_spec_executes_only_the_local_member_despite_registry_collision() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("checkout with spaces");
    fixture(&root);
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let locked = StdCommand::new(&cargo)
        .current_dir(&root)
        .args(["generate-lockfile", "--offline"])
        .output()
        .expect("offline lockfile");
    assert!(locked.status.success(), "{locked:?}");
    let ambiguous = StdCommand::new(&cargo)
        .current_dir(&root)
        .args(["pkgid", "--offline", "-p", "alpha@0.1.0"])
        .output()
        .expect("ambiguous Cargo command");
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"), "{ambiguous:?}");
    each(&root)
        .args(["-p", "alpha@0.1.0", "--"])
        .arg(&cargo)
        .args(["pkgid", "--offline", "--package", "{cargo-spec}"])
        .assert()
        .success()
        .stdout(predicate::str::contains("path+file:").and(predicate::str::contains("local%20alpha")));
    each(&root)
        .args(["-p", "alpha@0.1.0", "--"])
        .arg(&cargo)
        .args(["check", "--offline", "--package", "{cargo-spec}"])
        .assert()
        .success();
    each(&root)
        .args(["-p", "alpha", "--each-target", "lib", "--"])
        .arg(&cargo)
        .args(["check", "--offline", "--package", "{cargo-spec}", "--lib"])
        .assert()
        .success();
}

#[test]
fn structured_selection_is_filtered_workspace_only_and_resolves_after_relocation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let original = temp.path().join("original checkout");
    fixture(&original);
    let selection_file = temp.path().join("selection.txt");
    fs::write(&selection_file, "alpha@0.1.0\n").expect("portable selection");
    let before = list(&original, &["-p", "alpha@0.1.0"]);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0]["name"], "alpha");
    assert_eq!(before[0].as_object().expect("record").len(), 4);
    assert!(before[0]["cargo_spec"].as_str().expect("selector").contains("%20"));
    assert_eq!(list(&original, &[])[0]["name"], "root-package");
    assert_eq!(list(&original, &["--workspace"]).len(), 2);
    assert_eq!(list(&original, &["--workspace", "--filter", "not dep:alpha"]), before);
    assert_eq!(list(&original, &["--workspace", "--exclude", "root-package"]), before);
    assert!(list(&original, &["--none"]).is_empty());
    assert!(list(&original, &["--workspace", "--filter", "bin"]).is_empty());
    each(&original).args(["-p", "custom_library_name", "--list-json"]).assert().code(2);

    let relocated = temp.path().join("relocated checkout");
    fs::rename(&original, &relocated).expect("move checkout");
    let output = each(&relocated)
        .arg("--package-file")
        .arg(&selection_file)
        .arg("--list-json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let after: Vec<Value> = serde_json::from_slice(&output).expect("relocated selection");
    assert_eq!(after.len(), 1);
    assert_ne!(after[0]["cargo_spec"], before[0]["cargo_spec"]);
    assert_ne!(after[0]["id"], before[0]["id"]);
    assert_eq!(
        after[0]["manifest_path"],
        serde_json::json!(relocated.join("local alpha").join("Cargo.toml"))
    );
    each(&relocated)
        .arg("--package-file")
        .arg(&selection_file)
        .arg("--")
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["check", "--offline", "-p", "{cargo-spec}"])
        .assert()
        .success();
}

#[test]
fn explicit_metadata_toolchain_preserves_that_cargos_literal_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    fixture(temp.path());
    let active = StdCommand::new("rustup")
        .args(["show", "active-toolchain"])
        .output()
        .expect("active toolchain");
    assert!(active.status.success(), "{active:?}");
    let active = String::from_utf8(active.stdout).expect("toolchain UTF-8");
    let toolchain = active.split_whitespace().next().expect("active toolchain name");
    let metadata = StdCommand::new("cargo")
        .env("RUSTUP_TOOLCHAIN", toolchain)
        .current_dir(temp.path())
        .args(["metadata", "--no-deps", "--format-version", "1", "--offline"])
        .output()
        .expect("reference metadata");
    assert!(metadata.status.success(), "{metadata:?}");
    let metadata: Value = serde_json::from_slice(&metadata.stdout).expect("reference JSON");
    let local = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "alpha")
        .expect("local alpha");
    let output = each(temp.path())
        .env("CARGO", "nonexistent-cargo-must-not-be-used")
        .args(["-p", "alpha", "--metadata-toolchain", toolchain, "--list-json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let selected: Vec<Value> = serde_json::from_slice(&output).expect("selection");
    assert_eq!(selected[0]["id"], local["id"]);
    assert_eq!(selected[0]["manifest_path"], local["manifest_path"]);
}

#[test]
fn listing_rejects_execution_and_json_record_options() {
    for options in [
        vec!["--", "echo"],
        vec!["--once"],
        vec!["--dry-run"],
        vec!["--each-target", "lib"],
        vec!["--jobs", "2"],
        vec!["--json-lines", "{}"],
    ] {
        let mut command = Command::cargo_bin("cargo-each").expect("binary");
        command
            .args(["each", "--list-json"])
            .args(options)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("cannot be used with"));
    }
}
