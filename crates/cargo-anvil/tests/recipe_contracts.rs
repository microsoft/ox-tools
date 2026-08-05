// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const HELPERS: &str = include_str!("../templates/justfiles/anvil/helpers.just");
#[cfg(target_os = "linux")]
const BOLERO: &str = include_str!("../templates/justfiles/anvil/checks/bolero.just");
const LLVM_COV: &str = include_str!("../templates/justfiles/anvil/checks/llvm-cov.just");
const SEMVER: &str = include_str!("../templates/justfiles/anvil/checks/semver-check.just");
const EXTERNAL_TYPES: &str = include_str!("../templates/justfiles/anvil/checks/external-types.just");
const VERSIONS: &str = include_str!("../templates/justfiles/anvil/versions.just");

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

fn tools_available() -> bool {
    Command::new("just").arg("--version").output().is_ok() && Command::new("pwsh").arg("--version").output().is_ok()
}

fn fixture(imports: &[(&str, &str)], dependency_recipes: &[&str]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let mut justfile = String::from("set unstable\n\nrust_nightly := \"nightly-test\"\n\n");
    for (name, contents) in imports {
        write(&tmp.path().join(name), contents);
        writeln!(justfile, "import '{name}'").unwrap();
    }
    justfile.push('\n');
    for recipe in dependency_recipes {
        justfile.push_str(recipe);
        justfile.push_str(":\n\n");
    }
    write(&tmp.path().join("Justfile"), &justfile);
    write(
        &tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );

    let bin = tmp.path().join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write(
        &bin.join("cargo.ps1"),
        r"
$joined = $args -join ' '
if ($env:FAKE_CARGO_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_LOG -Value $joined
}
if ($args -contains 'metadata') {
    if ($env:FAKE_METADATA_EXIT) { exit [int]$env:FAKE_METADATA_EXIT }
    $root = $env:FAKE_WORKSPACE_ROOT
    $metadata = [pscustomobject]@{
        workspace_root = $root
        workspace_members = @('fixture 0.1.0')
        packages = @(
            [pscustomobject]@{
                name = 'fixture'
                version = '0.1.0'
                id = 'fixture 0.1.0'
                manifest_path = [System.IO.Path]::Combine($root, 'Cargo.toml')
                targets = @([pscustomobject]@{ name = 'fixture'; kind = @('lib') })
                metadata = [pscustomobject]@{
                    'coverage-gate' = [pscustomobject]@{ 'min-lines-percent' = 0 }
                }
            }
        )
    }
    $metadata | ConvertTo-Json -Depth 8 -Compress
    exit 0
}
if ($args -contains 'semver-checks') {
    if ($env:FAKE_SEMVER_OUTPUT) { Write-Output $env:FAKE_SEMVER_OUTPUT }
    exit [int]$env:FAKE_SEMVER_EXIT
}
if ($args -contains 'bolero' -and $args -contains 'list') {
    exit [int]$env:FAKE_BOLERO_LIST_EXIT
}
if ($args -contains 'nextest') {
    if ($env:FAKE_NEXTEST_EXIT -eq '4' -and $args -contains '--no-tests=pass') {
        exit 0
    }
    exit [int]$env:FAKE_NEXTEST_EXIT
}
exit 0
",
    );
    write(&bin.join("git.ps1"), "exit 0\n");
    tmp
}

fn path_with_fake_bin(root: &Path) -> OsString {
    let mut paths = vec![root.join("fake-bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::join_paths(paths).unwrap()
}

fn run_just(root: &Path, arguments: &[&str], environment: &[(&str, &OsStr)]) -> Output {
    let mut command = Command::new("just");
    command.args(["--justfile", "Justfile"]).args(arguments).current_dir(root);
    command.env("PATH", path_with_fake_bin(root));
    command.env("FAKE_WORKSPACE_ROOT", root);
    for &(key, value) in environment {
        command.env(key, value);
    }
    command.output().expect("just is required to verify generated recipe behavior")
}

fn assert_failed(output: &Output, context: &str) {
    assert!(
        !output.status.success(),
        "{context} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn impact_format_fails_for_unknown_packages_and_metadata_errors() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("helpers.just", HELPERS)], &[]);
    write(
        &tmp.path().join("impact.json"),
        r#"{"Modified":[],"Affected":["unknown-package"],"Required":[]}"#,
    );
    let log = tmp.path().join("cargo.log");

    let unknown = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[("FAKE_METADATA_EXIT", OsStr::new("0")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert_failed(&unknown, "unknown cargo-delta package");

    let metadata_error = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[("FAKE_METADATA_EXIT", OsStr::new("23")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert_failed(&metadata_error, "cargo metadata failure");
}

#[cfg(target_os = "linux")]
#[test]
fn bolero_discovery_failure_propagates() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("bolero.just", BOLERO)],
        &[
            "anvil-toolchain-nightly-validate-prereqs",
            "anvil-tool-cargo-bolero-validate-prereqs",
            "anvil-toolchain-nightly-install",
            "anvil-tool-cargo-bolero-install installer",
        ],
    );
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-bolero"],
        &[
            ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
            ("FAKE_BOLERO_LIST_EXIT", OsStr::new("9")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );

    assert_failed(&output, "cargo bolero target discovery failure");
}

#[test]
fn semver_exit_code_contract_is_executed() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("helpers.just", HELPERS), ("semver.just", SEMVER)],
        &[
            "anvil-tool-cargo-semver-checks-validate-prereqs",
            "anvil-tool-cargo-semver-checks-install installer",
        ],
    );
    let log = tmp.path().join("cargo.log");
    let common = [
        ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
        ("BASE_REF", OsStr::new("base")),
        ("FAKE_CARGO_LOG", log.as_os_str()),
    ];

    let findings = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[
            common[0],
            common[1],
            common[2],
            ("FAKE_SEMVER_EXIT", OsStr::new("100")),
            ("FAKE_SEMVER_OUTPUT", OsStr::new("breaking change")),
        ],
    );
    assert!(
        findings.status.success(),
        "exit 100 should be advisory:\n{}",
        String::from_utf8_lossy(&findings.stderr)
    );
    assert!(tmp.path().join("target/anvil/comments/semver.md").is_file());

    let renamed = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[
            common[0],
            common[1],
            common[2],
            ("FAKE_SEMVER_EXIT", OsStr::new("101")),
            ("FAKE_SEMVER_OUTPUT", OsStr::new("package `fixture` not found in the baseline")),
        ],
    );
    assert!(
        renamed.status.success(),
        "accepted exit 101 should succeed:\n{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    assert!(!tmp.path().join("target/anvil/comments/semver.md").exists());

    for output in ["has no lib target", "no library targets found"] {
        let bin_to_lib = run_just(
            tmp.path(),
            &["anvil-semver-check"],
            &[
                common[0],
                common[1],
                common[2],
                ("FAKE_SEMVER_EXIT", OsStr::new("101")),
                ("FAKE_SEMVER_OUTPUT", OsStr::new(output)),
            ],
        );
        assert!(
            bin_to_lib.status.success(),
            "bin-to-lib exit 101 wording '{output}' should succeed:\n{}",
            String::from_utf8_lossy(&bin_to_lib.stderr)
        );
    }

    for (exit, output) in [("101", "operational failure"), ("42", "unexpected failure")] {
        let failed = run_just(
            tmp.path(),
            &["anvil-semver-check"],
            &[
                common[0],
                common[1],
                common[2],
                ("FAKE_SEMVER_EXIT", OsStr::new(exit)),
                ("FAKE_SEMVER_OUTPUT", OsStr::new(output)),
            ],
        );
        assert_failed(&failed, &format!("cargo-semver-checks exit {exit}"));
    }
}

#[test]
fn repository_constants_match_shared_anvil_versions() {
    let constants_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../constants.env");
    if !constants_path.is_file() {
        return;
    }

    let repository_constants = std::fs::read_to_string(constants_path).unwrap();
    let constants = repository_constants.lines().filter_map(|line| {
        let (name, value) = line.split_once('=')?;
        Some((name.to_ascii_lowercase(), value.trim().to_owned()))
    });
    let anvil_versions = VERSIONS.lines().filter_map(|line| {
        let (name, value) = line.split_once(":=")?;
        Some((name.trim().to_owned(), value.trim().trim_matches('"').to_owned()))
    });
    let constants = constants.collect::<std::collections::HashMap<_, _>>();
    let anvil_versions = anvil_versions.collect::<std::collections::HashMap<_, _>>();

    let mismatches = constants
        .iter()
        // The legacy workflow's broad nightly follows rust-toolchain.toml,
        // while Anvil's general-purpose nightly has its own compatibility cadence.
        .filter(|(name, _)| name.as_str() != "rust_nightly")
        .filter_map(|(name, constant)| {
            let anvil = anvil_versions.get(name)?;
            (constant != anvil).then(|| format!("{name}: constants.env={constant}, anvil={anvil}"))
        })
        .collect::<Vec<_>>();

    assert!(
        mismatches.is_empty(),
        "shared repository and Anvil versions must match:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn public_api_checks_fail_when_metadata_discovery_fails() {
    if !tools_available() {
        return;
    }
    for (recipe_file, contents, recipe, dependencies) in [
        (
            "semver.just",
            SEMVER,
            "anvil-semver-check",
            &[
                "anvil-tool-cargo-semver-checks-validate-prereqs",
                "anvil-tool-cargo-semver-checks-install installer",
            ][..],
        ),
        (
            "external-types.just",
            EXTERNAL_TYPES,
            "anvil-external-types",
            &[
                "anvil-tool-cargo-check-external-types-validate-prereqs",
                "anvil-toolchain-external-types-validate-prereqs",
                "anvil-tool-cargo-check-external-types-install installer",
                "anvil-toolchain-external-types-install",
            ][..],
        ),
    ] {
        let tmp = fixture(&[(recipe_file, contents)], dependencies);
        let output = run_just(
            tmp.path(),
            &[recipe],
            &[
                ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
                ("FAKE_METADATA_EXIT", OsStr::new("23")),
            ],
        );
        assert_failed(&output, &format!("{recipe} cargo metadata failure"));
    }
}

#[test]
fn all_coverage_opted_out_packages_run_both_test_configurations() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("llvm-cov.just", LLVM_COV)],
        &[
            "anvil-component-nightly-llvm-tools-validate-prereqs",
            "anvil-tool-cargo-llvm-cov-validate-prereqs",
            "anvil-tool-cargo-nextest-validate-prereqs",
            "anvil-tool-cargo-coverage-gate-validate-prereqs",
            "anvil-component-nightly-llvm-tools-install",
            "anvil-tool-cargo-llvm-cov-install installer",
            "anvil-tool-cargo-nextest-install installer",
            "anvil-tool-cargo-coverage-gate-install installer",
        ],
    );
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-llvm-cov"],
        &[
            ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
            ("FAKE_NEXTEST_EXIT", OsStr::new("0")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "all-opted-out coverage path should succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("nextest run").count(), 2, "calls:\n{calls}");
    assert!(calls.contains("--all-features"), "calls:\n{calls}");
    assert!(calls.contains("--no-default-features"), "calls:\n{calls}");
    assert_eq!(calls.matches("--no-tests=pass").count(), 2, "calls:\n{calls}");
    assert!(!calls.contains("llvm-cov"), "coverage commands must not run:\n{calls}");
    assert!(!calls.contains("coverage-gate"), "the coverage gate must not run:\n{calls}");

    let no_tests = run_just(
        tmp.path(),
        &["anvil-llvm-cov"],
        &[
            ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
            ("FAKE_NEXTEST_EXIT", OsStr::new("4")),
        ],
    );
    assert!(
        no_tests.status.success(),
        "opted-out packages with no runnable tests should succeed:\n{}",
        String::from_utf8_lossy(&no_tests.stderr)
    );

    let failed = run_just(
        tmp.path(),
        &["anvil-llvm-cov"],
        &[
            ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
            ("FAKE_NEXTEST_EXIT", OsStr::new("7")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );
    assert_failed(&failed, "plain nextest failure for an opted-out package");
}
