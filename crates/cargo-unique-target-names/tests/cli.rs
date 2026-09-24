// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The command-line contract: the argument vector Cargo passes, the exit code
//! each verdict maps to, and the text the tool prints.
//!
//! These call `run` directly rather than spawning the built binary. `main` does
//! nothing but hand `std::env::args_os` to `run` and turn the outcome's exit
//! code into a process exit code, so a subprocess would exercise the same code
//! behind a process boundary and observe less of it.

// miri cannot sandbox the filesystem work these fixtures do, nor the `cargo
// metadata` subprocess that reads them.
#![cfg(not(miri))]

mod common;

use std::fs;
use std::path::Path;

use cargo_unique_target_names::{EXIT_UNREADABLE_WORKSPACE, EXIT_USAGE, Outcome, run};
use common::{Member, workspace};

/// Runs the tool the way Cargo invokes it: `cargo unique-target-names ...`,
/// with the subcommand name as the first argument after the driver.
fn invoke(manifest: &Path) -> Outcome {
    run([
        "cargo".as_ref(),
        "unique-target-names".as_ref(),
        "--manifest-path".as_ref(),
        manifest.as_os_str(),
    ])
}

#[test]
fn a_clean_workspace_exits_zero_and_says_so() {
    let temp = workspace(&[
        Member::new("alpha", "alpha_shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "beta_shared", "beta_tool", "beta_lib_example"),
    ]);
    let outcome = invoke(&temp.path().join("Cargo.toml"));
    let diagnostic = outcome.render();
    assert_eq!(outcome.exit_code(), 0, "{diagnostic}");
    assert!(
        diagnostic.contains("All workspace targets uplift to their own path"),
        "{diagnostic}"
    );
}

#[test]
fn a_contended_workspace_exits_one_and_names_the_remedy() {
    let temp = workspace(&[
        Member::new("alpha", "shared", "alpha_tool", "alpha_lib_example"),
        Member::new("beta", "shared", "beta_tool", "beta_lib_example"),
    ]);
    let outcome = invoke(&temp.path().join("Cargo.toml"));
    let diagnostic = outcome.render();
    assert_eq!(outcome.exit_code(), 1, "{diagnostic}");
    assert!(
        diagnostic.contains("cargo-unique-target-names: 2 targets uplift to the same files"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("Rename the reported targets so each one uplifts to its own path."),
        "{diagnostic}"
    );
}

/// A workspace that cannot be read is neither clean nor contended, and must be
/// distinguishable from both.
#[test]
fn an_unreadable_workspace_exits_with_its_own_code() {
    let temp = tempfile::tempdir().expect("temporary directory must be creatable");
    let manifest = temp.path().join("Cargo.toml");
    fs::write(&manifest, "this is not a manifest\n").expect("manifest must be writable");
    let outcome = invoke(&manifest);
    let diagnostic = outcome.render();
    assert_eq!(
        outcome.exit_code(),
        EXIT_UNREADABLE_WORKSPACE,
        "a broken workspace must not share an exit code with a finding: {diagnostic}"
    );
    assert!(diagnostic.contains("failed to read workspace metadata from cargo"), "{diagnostic}");
}

/// A command line clap rejects must come back as an outcome rather than
/// terminating the caller, and must not borrow the exit code that already
/// means "the workspace could not be read".
#[test]
fn a_rejected_command_line_carries_its_own_code() {
    let outcome = run(["cargo", "unique-target-names", "--no-such-option"]);
    let diagnostic = outcome.render();
    assert!(matches!(outcome, Outcome::Usage(_)), "{diagnostic}");
    assert_eq!(outcome.exit_code(), EXIT_USAGE, "{diagnostic}");
    assert_ne!(
        outcome.exit_code(),
        EXIT_UNREADABLE_WORKSPACE,
        "a bad invocation must not look like a broken workspace: {diagnostic}"
    );
    assert!(diagnostic.contains("--no-such-option"), "{diagnostic}");
    assert!(
        !outcome.is_informational(),
        "a rejected command line belongs on stderr: {diagnostic}"
    );
}

/// Help and version are requests, not failures: clap reports them as errors,
/// but they succeed and belong on stdout.
#[test]
fn asking_for_help_or_version_succeeds() {
    for request in ["--help", "--version"] {
        let outcome = run(["cargo", "unique-target-names", request]);
        let diagnostic = outcome.render();
        assert!(matches!(outcome, Outcome::Help(_)), "{request}: {diagnostic}");
        assert_eq!(outcome.exit_code(), 0, "{request}: {diagnostic}");
        assert!(outcome.is_informational(), "{request}: {diagnostic}");
        assert!(!diagnostic.is_empty(), "{request}: clap's answer must be carried out");
    }
}
