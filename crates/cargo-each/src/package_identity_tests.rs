// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! In-process identity regressions with controlled metadata, input, and execution.

#![cfg_attr(coverage_nightly, coverage(off))]

use std::path::{Path, PathBuf};

use cargo_metadata::Metadata;
use clap::Parser as _;
use clap::error::ErrorKind;
use serde_json::{Value, json};

use super::*;
use crate::cli::CargoCli;

fn checkout_path(checkout: &str) -> PathBuf {
    PathBuf::from(if cfg!(windows) { r"C:\checkouts" } else { "/checkouts" }).join(checkout)
}

fn expected_selector(checkout: &str, package: &str) -> String {
    let prefix = if cfg!(windows) {
        "file:///C:/checkouts"
    } else {
        "file:///checkouts"
    };
    let checkout = checkout.replace(' ', "%20");
    match package {
        "alpha" => format!("{prefix}/{checkout}/local%20alpha#alpha@0.1.0"),
        "root-package" => format!("{prefix}/{checkout}#root-package@0.1.0"),
        unexpected => panic!("unexpected package: {unexpected}"),
    }
}

fn expected_record(checkout: &str, package: &str, id: &str) -> Value {
    let directory = match package {
        "alpha" => checkout_path(checkout).join("local alpha"),
        "root-package" => checkout_path(checkout),
        unexpected => panic!("unexpected package: {unexpected}"),
    };
    json!({
        "manifest_path": directory.join("Cargo.toml"),
        "name": package,
        "cargo_spec": expected_selector(checkout, package),
        "id": id,
    })
}

fn workspace(checkout: &str, member_id: &str) -> Workspace {
    let metadata: Metadata = serde_json::from_value(workspace_metadata(checkout, member_id)).expect("controlled collision metadata");
    Workspace::from_metadata(&metadata)
}

fn workspace_metadata(checkout: &str, member_id: &str) -> Value {
    let root = checkout_path(checkout);
    let mut metadata: Value =
        serde_json::from_str(include_str!("../tests/fixtures/doctest-metadata.json")).expect("checked-in metadata fixture");
    let mut local = metadata["packages"][0].clone();
    local["name"] = json!("alpha");
    local["id"] = json!(member_id);
    local["manifest_path"] = json!(root.join("local alpha").join("Cargo.toml"));
    local["targets"] = json!([local["targets"][0]]);
    local["targets"][0]["name"] = json!("custom_library_name");
    local["targets"][0]["src_path"] = json!(root.join("local alpha").join("src").join("lib.rs"));

    let mut registry = local.clone();
    registry["id"] = json!("registry+https://github.com/rust-lang/crates.io-index#alpha@0.1.0");
    registry["source"] = json!("registry+https://github.com/rust-lang/crates.io-index");
    registry["manifest_path"] = json!(root.join("vendor").join("alpha").join("Cargo.toml"));

    let mut root_package = local.clone();
    root_package["name"] = json!("root-package");
    root_package["id"] = json!("literal-root-id");
    root_package["manifest_path"] = json!(root.join("Cargo.toml"));
    root_package["targets"][0]["name"] = json!("root_package");
    root_package["targets"][0]["src_path"] = json!(root.join("src").join("lib.rs"));
    root_package["dependencies"] = json!([{
        "name": "alpha",
        "source": "registry+https://github.com/rust-lang/crates.io-index",
        "req": "=0.1.0",
        "kind": null,
        "rename": null,
        "optional": false,
        "uses_default_features": true,
        "features": [],
        "target": null,
        "registry": null,
    }]);
    metadata["packages"] = json!([root_package, local, registry]);
    metadata["workspace_members"] = json!(["literal-root-id", member_id]);
    metadata["workspace_default_members"] = json!(["literal-root-id"]);
    metadata["workspace_root"] = json!(root);
    metadata["target_directory"] = json!(root.join("target"));
    metadata
}

fn args(options: &[&str]) -> EachArgs {
    let CargoCli::Each(args) =
        CargoCli::try_parse_from(["cargo", "each"].into_iter().chain(options.iter().copied())).expect("identity arguments");
    args
}

fn selection_without_files(args: &EachArgs) -> Result<Selection, crate::error::EachError> {
    Selection::from_sources_with_reader(
        &args.packages,
        &args.package_files,
        args.workspace,
        &args.exclude,
        args.none,
        |path| panic!("unexpected package-file read: {}", path.display()),
    )
}

fn list(checkout: &str, options: &[&str]) -> Value {
    let mut options = options.to_vec();
    options.push("--list-json");
    let args = args(&options);
    let mut output = Vec::new();
    let code = run_workspace_with(
        &args,
        selection_without_files,
        |manifest, toolchain| {
            assert_eq!(manifest, None);
            assert_eq!(toolchain, None);
            Ok(workspace(checkout, "literal-local-id"))
        },
        &mut output,
        |_, _, _, _| panic!("listing must not execute a child command"),
    )
    .expect("listing");
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(output.last(), Some(&b'\n'));
    serde_json::from_slice(&output).expect("structured selection")
}

#[test]
fn listing_resolves_only_workspace_packages_and_applies_existing_filters() {
    let checkout = "checkout with spaces";
    let alpha = expected_record(checkout, "alpha", "literal-local-id");
    let root = expected_record(checkout, "root-package", "literal-root-id");
    for (options, expected) in [
        (vec!["-p", "alpha@0.1.0"], json!([alpha])),
        (vec![], json!([root])),
        (vec!["--workspace"], json!([alpha, root])),
        (vec!["--workspace", "--filter", "not dep:alpha"], json!([alpha])),
        (vec!["--workspace", "--exclude", "root-package"], json!([alpha])),
        (vec!["--none"], json!([])),
        (vec!["--workspace", "--filter", "bin"], json!([])),
    ] {
        assert_eq!(list(checkout, &options), expected, "{options:?}");
    }
}

#[test]
fn target_names_do_not_select_packages() {
    let workspace = workspace("checkout with spaces", "literal-local-id");
    let selection = selection_without_files(&args(&["-p", "custom_library_name", "--list-json"])).expect("selection");
    let error = selection.resolve(&workspace).expect_err("a target name is not a package selector");
    assert_eq!(
        error.to_string(),
        "package selector `custom_library_name` did not match any workspace member"
    );
}

#[test]
fn command_dispatch_is_source_qualified_in_both_package_modes() {
    let checkout = "checkout with spaces";
    for (mode, label) in [(vec![], "alpha"), (vec!["--each-target", "lib"], "alpha::custom_library_name")] {
        let mut options = vec!["-p", "alpha@0.1.0", "--chdir", "--keep-going", "--jobs", "2", "--timeout", "3s"];
        options.extend(mode);
        options.extend([
            "--",
            "cargo",
            "check",
            "--offline",
            "-p",
            "{cargo-spec}",
            "legacy={spec}",
            "target={name}",
        ]);
        let args = args(&options);
        let mut output = Vec::new();
        let mut executions = 0;
        let code = run_workspace_with(
            &args,
            selection_without_files,
            |manifest, toolchain| {
                assert_eq!((manifest, toolchain), (None, None));
                Ok(workspace(checkout, "literal-local-id"))
            },
            &mut output,
            |plan, keep_going, jobs, timeout| {
                executions += 1;
                assert!(keep_going);
                assert_eq!(jobs.get(), 2);
                assert_eq!(timeout, Some(Duration::from_secs(3)));
                assert_eq!(
                    plan,
                    &Plan {
                        invocations: vec![Invocation {
                            label: Some(label.to_owned()),
                            argv: vec![
                                "cargo".to_owned(),
                                "check".to_owned(),
                                "--offline".to_owned(),
                                "-p".to_owned(),
                                expected_selector(checkout, "alpha"),
                                "legacy=alpha@0.1.0".to_owned(),
                                "target=alpha".to_owned(),
                            ],
                            work_dir: Some(checkout_path(checkout).join("local alpha")),
                        }],
                    }
                );
                Ok(ExitCode::from(17))
            },
        )
        .expect("scripted command dispatch");
        assert_eq!(executions, 1);
        assert_eq!(code, ExitCode::from(17));
        assert_eq!(output, b"");
    }
}

#[test]
fn explicit_root_selection_does_not_dispatch_other_default_members() {
    let checkout = "checkout with spaces";
    let args = args(&[
        "-p",
        "root-package@0.1.0",
        "--",
        "cargo",
        "check",
        "--offline",
        "-p",
        "{cargo-spec}",
    ]);
    let mut output = Vec::new();
    let mut executions = 0;
    let code = run_workspace_with(
        &args,
        selection_without_files,
        |manifest, toolchain| {
            assert_eq!((manifest, toolchain), (None, None));
            let mut metadata = workspace_metadata(checkout, "literal-local-id");
            metadata["workspace_default_members"] = json!(["literal-local-id"]);
            let metadata: Metadata = serde_json::from_value(metadata).expect("controlled root-package metadata");
            let workspace = Workspace::from_metadata(&metadata);
            assert_eq!(
                workspace.default_member_names,
                std::collections::HashSet::from(["alpha".to_owned()])
            );
            assert_eq!(
                workspace
                    .members
                    .iter()
                    .find(|member| member.name == "root-package")
                    .expect("root package is a member")
                    .manifest_path,
                workspace.root_manifest_path,
            );
            Ok(workspace)
        },
        &mut output,
        |plan, keep_going, jobs, timeout| {
            executions += 1;
            assert!(!keep_going);
            assert_eq!(jobs.get(), 1);
            assert_eq!(timeout, None);
            assert_eq!(
                plan,
                &Plan {
                    invocations: vec![Invocation {
                        label: Some("root-package".to_owned()),
                        argv: vec![
                            "cargo".to_owned(),
                            "check".to_owned(),
                            "--offline".to_owned(),
                            "-p".to_owned(),
                            expected_selector(checkout, "root-package"),
                        ],
                        work_dir: None,
                    }],
                }
            );
            Ok(ExitCode::SUCCESS)
        },
    )
    .expect("explicit root-package dispatch");
    assert_eq!(executions, 1);
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(output, b"");
}

#[test]
fn portable_package_file_resolves_against_each_checkout_without_filesystem_mutation() {
    for (checkout, id) in [
        ("original checkout", "original literal ID"),
        ("relocated checkout", "relocated literal ID"),
    ] {
        let manifest = checkout_path(checkout).join("Cargo.toml");
        let args = args(&[
            "--manifest-path",
            manifest.to_str().expect("synthetic UTF-8 path"),
            "--package-file",
            "selection.txt",
            "--list-json",
        ]);
        let mut reads = 0;
        let mut output = Vec::new();
        let code = run_workspace_with(
            &args,
            |args| {
                Selection::from_sources_with_reader(
                    &args.packages,
                    &args.package_files,
                    args.workspace,
                    &args.exclude,
                    args.none,
                    |path| {
                        assert_eq!(path, Path::new("selection.txt"));
                        reads += 1;
                        Ok("\u{feff}alpha@0.1.0\r\n".as_bytes().to_vec())
                    },
                )
            },
            |path, toolchain| {
                assert_eq!(path, Some(manifest.as_path()));
                assert_eq!(toolchain, None);
                Ok(workspace(checkout, id))
            },
            &mut output,
            |_, _, _, _| panic!("listing must not execute commands"),
        )
        .expect("relocated selection");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(reads, 1);
        let actual: Value = serde_json::from_slice(&output).expect("listing JSON");
        assert_eq!(actual, json!([expected_record(checkout, "alpha", id)]));
    }
}

#[test]
fn explicit_metadata_toolchain_preserves_its_exact_literal_id() {
    let checkout = "checkout with spaces";
    for (toolchain, id) in [
        ("metadata-toolchain-a", "alpha 0.1.0 (path+file:///opaque/source)"),
        ("metadata-toolchain-b", "opaque metadata ID with no parseable selector"),
    ] {
        let args = args(&["-p", "alpha", "--metadata-toolchain", toolchain, "--list-json"]);
        let mut output = Vec::new();
        let code = run_workspace_with(
            &args,
            selection_without_files,
            |manifest, selected_toolchain| {
                assert_eq!(manifest, None);
                assert_eq!(selected_toolchain, Some(toolchain));
                Ok(workspace(checkout, id))
            },
            &mut output,
            |_, _, _, _| panic!("listing must not execute commands"),
        )
        .expect("toolchain-specific listing");
        assert_eq!(code, ExitCode::SUCCESS);
        let actual: Value = serde_json::from_slice(&output).expect("listing JSON");
        assert_eq!(actual, json!([expected_record(checkout, "alpha", id)]));
    }
}

#[test]
fn listing_rejects_execution_and_json_record_options_in_process() {
    for options in [
        vec!["--", "echo"],
        vec!["--once"],
        vec!["--dry-run"],
        vec!["--each-target", "lib"],
        vec!["--chdir"],
        vec!["--keep-going"],
        vec!["--timeout", "3s"],
        vec!["--jobs", "2"],
        vec!["--json-lines", "{}"],
        vec!["--json-lines-file", "input.jsonl"],
    ] {
        let error = CargoCli::try_parse_from(["cargo", "each", "--list-json"].into_iter().chain(options.iter().copied()))
            .expect_err("listing conflicts with execution and JSON-record input");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{options:?}");
    }
    let error = CargoCli::try_parse_from([
        "cargo",
        "each",
        "--json-lines",
        "{}",
        "--metadata-toolchain",
        "fixture",
        "--",
        "echo",
    ])
    .expect_err("JSON-record mode does not query metadata");
    assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
}
