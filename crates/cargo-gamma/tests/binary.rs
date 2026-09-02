// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! The installed executable, launched as a process rather than called as a library.
//!
//! Everything else in the suite calls `cargo_gamma_lib::run` directly, which is deliberate: `run`
//! returns an exit code instead of exiting so that every path through the CLI is reachable from an
//! ordinary test. What that leaves untested is the executable boundary that turns a returned code
//! into a process outcome — forwarding the real arguments, and exiting with what came back.
//! Replacing `main` with an empty body, or with one that ignores the code, breaks the installed tool
//! completely while the library tests continue to pass.
//!
//! So these launch the built binary and read what a shell would read: the exit status, and which of
//! the two streams the output arrived on.

use std::process::{Command, Output};

use cargo_gamma_lib::internals::commands::{EXIT_OK, EXIT_USAGE};

/// Runs the built executable with `arguments` and returns everything the process produced.
fn gamma(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-gamma"))
        .args(arguments)
        .output()
        .expect("the built cargo-gamma binary runs")
}

/// The exit status as a code, insisting the process exited rather than dying to a signal.
#[track_caller]
fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or_else(|| {
        panic!(
            "the process was killed rather than exiting: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// `--version` succeeds, and says the version on standard output.
///
/// Two things at once, and neither is incidental. The exit code proves `main` forwards what `run`
/// returned rather than exiting on its own account, and the printed version proves the arguments
/// reached the parser at all — a `main` that dropped them would print help or an error instead.
#[test]
fn the_installed_binary_reports_its_version_and_succeeds() {
    let output = gamma(&["--version"]);
    let printed = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    let expected = format!("cargo-gamma {}", env!("CARGO_PKG_VERSION"));

    assert_eq!(code(&output), EXIT_OK, "{errors}");
    assert_eq!(printed.trim(), expected);
    assert!(output.stderr.is_empty(), "version output reached standard error: {errors}");
}

/// The same, invoked the way cargo invokes it, with the subcommand name in front.
///
/// Cargo runs `cargo-gamma gamma …`, so the second argument is the subcommand's own name and has to
/// be dropped before the parser ever sees it. Nothing in a direct call to `run` notices if
/// that stops happening, because every test that calls it writes the arguments the parser expects.
#[test]
fn the_installed_binary_accepts_the_argument_shape_cargo_passes_it() {
    let output = gamma(&["gamma", "--version"]);
    let printed = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    let expected = format!("cargo-gamma {}", env!("CARGO_PKG_VERSION"));

    assert_eq!(code(&output), EXIT_OK, "{errors}");
    assert_eq!(printed.trim(), expected);
    assert!(output.stderr.is_empty(), "version output reached standard error: {errors}");
}

/// A command that does not exist exits with the documented usage code, on standard error.
///
/// The nonzero half of the contract, which is the half a `main` that ignored `run`'s answer would
/// break: a shell, a script, and a CI job all decide what happened next from this number alone, and
/// a tool that exits zero after refusing to do anything is worse than one that crashes.
///
/// Standard error rather than standard output matters for the same audience: `cargo gamma list`
/// output is meant to be piped, and a diagnostic on that stream corrupts whatever consumes it.
#[test]
fn an_unknown_command_exits_with_the_usage_code_and_explains_itself_on_standard_error() {
    let output = gamma(&["definitely-not-a-command"]);
    let complaint = String::from_utf8_lossy(&output.stderr);

    assert_eq!(code(&output), EXIT_USAGE, "an unknown command must not be reported as success");
    assert!(!complaint.trim().is_empty(), "an unknown command was refused without saying why");
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "a diagnostic reached standard output, where it would corrupt piped report output"
    );
}
