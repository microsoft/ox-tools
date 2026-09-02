// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]

//! Pins the exact diagnostic each exported macro reports for a malformed argument, by compiling a
//! fixture against the real `gamma` proc-macro artifact rather than a stand-in.
//!
//! A `compile_fail` doctest only proves that *some* error occurred; it accepts a diagnostic from
//! any cause, including an unrelated one a regression introduced by accident. Real compilation,
//! checked against a substring every macro's diagnostic carries — its own `#[gamma::<name>]:`
//! prefix — is what actually distinguishes "the tool objected" from "the tool objected under its
//! own name, for the reason it states".

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn rustc() -> String {
    env::var("RUSTC").unwrap_or_else(|_missing| "rustc".to_owned())
}

/// The directory holding every artifact `cargo test` built for this run.
///
/// The running test binary's own path is used to find it, rather than a guessed `target/...`
/// layout: it is correct regardless of profile, target triple, or a customized target directory,
/// because it is where this very process was actually loaded from.
fn deps_dir() -> PathBuf {
    let exe = env::current_exe().expect("a running test binary knows its own path");

    exe.parent().expect("a test binary is always inside some directory").to_path_buf()
}

/// Finds the most recently built `gamma` proc-macro artifact.
///
/// Named by prefix rather than a fixed path, because the exact filename carries a per-build hash
/// this test cannot predict. An explicit `--target` puts this test under the target triple while
/// proc macros remain host artifacts, so both the test's dependency directory and the corresponding
/// host dependency directory are searched.
///
/// Panics rather than reporting absence, because absence is not a host limitation: cargo builds
/// the dependency before it links this binary, so a missing artifact means the lookup is wrong and
/// every diagnostic below would otherwise be skipped while reporting success.
#[track_caller]
fn gamma_artifact() -> PathBuf {
    let directory = deps_dir();
    let profile = directory
        .parent()
        .expect("a dependency directory is always inside a profile directory");
    let mut directories = vec![directory.clone()];

    if let (Some(profile_name), Some(target_root)) = (profile.file_name(), profile.parent().and_then(|parent| parent.parent())) {
        let host = target_root.join(profile_name).join("deps");

        if host != directory && host.is_dir() {
            directories.push(host);
        }
    }

    let mut candidates: Vec<PathBuf> = directories
        .iter()
        .flat_map(|directory| {
            std::fs::read_dir(directory)
                .unwrap_or_else(|error| panic!("artifact directory {} must be readable: {error}", directory.display()))
        })
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            let is_gamma = name.starts_with("libgamma-") || name.starts_with("gamma-");
            let is_dynamic = matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("so" | "dylib" | "dll")
            );

            is_gamma && is_dynamic
        })
        .collect();

    candidates.sort_by_key(|path| std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok());

    candidates.pop().unwrap_or_else(|| {
        panic!(
            "no `gamma` proc-macro artifact in {}, which cargo must have built before running this test",
            directories
                .iter()
                .map(|directory| directory.display().to_string())
                .collect::<Vec<_>>()
                .join(" or ")
        )
    })
}

/// Compiles `source` against the real `gamma` crate and returns what `rustc` said about it.
///
/// Every failure is reported as a failure. A skip here would be indistinguishable from a passing
/// diagnostic check, so a lookup that stopped finding the artifact, or a host without a usable
/// `rustc`, would retire every assertion below while the suite stayed green — which is the one
/// outcome a test pinning diagnostics must not produce. Panics if the fixture unexpectedly
/// compiles too: every fixture this file hands to it is deliberately malformed, and a clean
/// compile means the validation this test exists to pin has stopped rejecting it.
#[track_caller]
fn diagnostic_for(name: &str, source: &str) -> String {
    let artifact = gamma_artifact();
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("diagnostics-{}", std::process::id()));

    std::fs::create_dir_all(&directory).expect("the scratch directory must be creatable");

    let path = directory.join(format!("{name}.rs"));

    std::fs::write(&path, source).expect("the fixture source must be writable");

    let compiler = rustc();
    let output = Command::new(&compiler)
        .args(["--edition", "2024", "--crate-type", "lib", "--emit", "metadata"])
        .arg("--extern")
        .arg(format!("gamma={}", artifact.display()))
        .arg("-o")
        .arg(directory.join(format!("{name}.rmeta")))
        .arg(&path)
        .output()
        .unwrap_or_else(|error| panic!("`{compiler}` must be runnable to pin what it reports for {name}: {error}"));

    assert!(
        !output.status.success(),
        "a deliberately malformed fixture compiled cleanly\n--- source ---\n{source}"
    );

    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn skip_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::skip(reason = performance)]\nfn scaled(a: i64) -> i64 { a * 2 }\n";
    let reported = diagnostic_for("skip_malformed", source);

    assert!(reported.contains("#[gamma::skip]:"), "{reported}");
    assert!(reported.contains("`reason` must be a string literal"), "{reported}");
}

#[test]
fn expect_survived_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::expect_survived(tag = 7)]\nfn describe(n: usize) -> usize { n }\n";
    let reported = diagnostic_for("expect_survived_malformed", source);

    assert!(reported.contains("#[gamma::expect_survived]:"), "{reported}");
    assert!(reported.contains("`tag` must be a string literal"), "{reported}");
}

#[test]
fn expect_killed_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::expect_killed(reason = 5)]\nfn checksum(bytes: &[u8]) -> usize { bytes.len() }\n";
    let reported = diagnostic_for("expect_killed_malformed", source);

    assert!(reported.contains("#[gamma::expect_killed]:"), "{reported}");
    assert!(reported.contains("`reason` must be a string literal"), "{reported}");
}

#[test]
fn value_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::value()]\nfn budget() -> u32 { 512 }\n";
    let reported = diagnostic_for("value_malformed", source);

    assert!(reported.contains("#[gamma::value]:"), "{reported}");
    assert!(reported.contains("expected one expression"), "{reported}");
}

#[test]
fn test_timeout_multiplier_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::test_timeout_multiplier(\"fast\")]\nfn heavy(data: &[u8]) -> usize { data.len() }\n";
    let reported = diagnostic_for("test_timeout_multiplier_malformed", source);

    assert!(reported.contains("#[gamma::test_timeout_multiplier]:"), "{reported}");
    assert!(reported.contains("timeout multiplier must be a positive number"), "{reported}");
}

#[test]
fn timeout_multiplier_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::timeout_multiplier(\"fast\")]\nfn heavy(data: &[u8]) -> usize { data.len() }\n";
    let reported = diagnostic_for("timeout_multiplier_malformed", source);

    assert!(reported.contains("#[gamma::timeout_multiplier]:"), "{reported}");
    assert!(reported.contains("timeout multiplier must be a positive number"), "{reported}");
}

#[test]
fn gamma_reports_its_own_name_in_a_malformed_diagnostic() {
    let source = "#[gamma::gamma(\"fast\")]\nfn heavy(data: &[u8]) -> usize { data.len() }\n";
    let reported = diagnostic_for("gamma_malformed", source);

    assert!(reported.contains("#[gamma::gamma]:"), "{reported}");
    assert!(reported.contains("timeout multiplier must be a positive number"), "{reported}");
}
