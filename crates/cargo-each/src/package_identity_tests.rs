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

fn expected_invocation(checkout: &str, package: &str) -> Invocation {
    Invocation {
        label: Some(package.to_owned()),
        argv: vec![
            "cargo".to_owned(),
            "check".to_owned(),
            "--offline".to_owned(),
            "-p".to_owned(),
            expected_selector(checkout, package),
            format!("legacy={package}@0.1.0"),
        ],
        work_dir: None,
    }
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

fn dispatch(checkout: &str, options: &[&str]) -> Vec<Invocation> {
    let mut options = options.to_vec();
    options.extend(["--", "cargo", "check", "--offline", "-p", "{cargo-spec}", "legacy={spec}"]);
    let args = args(&options);
    let mut invocations = Vec::new();
    let code = run_workspace_with(
        &args,
        selection_without_files,
        |manifest| {
            assert_eq!(manifest, None);
            Ok(workspace(checkout, "literal-local-id"))
        },
        |plan, keep_going, jobs, timeout| {
            assert!(!keep_going);
            assert_eq!(jobs.get(), 1);
            assert_eq!(timeout, None);
            invocations.clone_from(&plan.invocations);
            Ok(ExitCode::SUCCESS)
        },
    )
    .expect("scripted dispatch");
    assert_eq!(code, ExitCode::SUCCESS);
    invocations
}

#[test]
fn dispatch_resolves_only_workspace_packages_and_applies_existing_filters() {
    let checkout = "checkout with spaces";
    for (options, expected) in [
        (vec!["-p", "alpha@0.1.0"], vec!["alpha"]),
        (vec![], vec!["root-package"]),
        (vec!["--workspace"], vec!["alpha", "root-package"]),
        (vec!["--workspace", "--filter", "not dep:alpha"], vec!["alpha"]),
        (vec!["--workspace", "--exclude", "root-package"], vec!["alpha"]),
        (vec!["--none"], vec![]),
        (vec!["--workspace", "--filter", "bin"], vec![]),
    ] {
        let expected = expected
            .iter()
            .map(|package| expected_invocation(checkout, package))
            .collect::<Vec<_>>();
        assert_eq!(dispatch(checkout, &options), expected, "{options:?}");
    }
}

#[test]
fn target_names_do_not_select_packages() {
    let workspace = workspace("checkout with spaces", "literal-local-id");
    let selection = selection_without_files(&args(&["-p", "custom_library_name", "--", "cargo", "check"])).expect("selection");
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
        let mut executions = 0;
        let code = run_workspace_with(
            &args,
            selection_without_files,
            |manifest| {
                assert_eq!(manifest, None);
                Ok(workspace(checkout, "literal-local-id"))
            },
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
    let mut executions = 0;
    let code = run_workspace_with(
        &args,
        selection_without_files,
        |manifest| {
            assert_eq!(manifest, None);
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
            "--",
            "cargo",
            "check",
            "--offline",
            "-p",
            "{cargo-spec}",
            "--manifest-path",
            "{manifest}",
        ]);
        let mut reads = 0;
        let mut executions = 0;
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
            |path| {
                assert_eq!(path, Some(manifest.as_path()));
                Ok(workspace(checkout, id))
            },
            |plan, _, _, _| {
                executions += 1;
                assert_eq!(
                    plan,
                    &Plan {
                        invocations: vec![Invocation {
                            label: Some("alpha".to_owned()),
                            argv: vec![
                                "cargo".to_owned(),
                                "check".to_owned(),
                                "--offline".to_owned(),
                                "-p".to_owned(),
                                expected_selector(checkout, "alpha"),
                                "--manifest-path".to_owned(),
                                checkout_path(checkout).join("local alpha").join("Cargo.toml").display().to_string(),
                            ],
                            work_dir: None,
                        }],
                    }
                );
                Ok(ExitCode::SUCCESS)
            },
        )
        .expect("relocated selection");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(reads, 1);
        assert_eq!(executions, 1);
    }
}

#[test]
fn aggregate_selection_options_are_not_exposed() {
    for option in ["--list-json", "--metadata-toolchain"] {
        let error = CargoCli::try_parse_from(["cargo", "each", option, "--", "cargo", "check"])
            .expect_err("aggregate selection APIs are outside the CLI surface");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{option}");
    }
    let error = CargoCli::try_parse_from(["cargo", "each"]).expect_err("execution requires a command");
    assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
}
