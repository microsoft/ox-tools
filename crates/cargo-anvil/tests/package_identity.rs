// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(not(miri))]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::expect_used, clippy::unwrap_used, reason = "panic-on-failure test assertions")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture exercises all package selector adapters against the same ambiguous graph"
)]
fn generated_commands_disambiguate_same_name_and_version_on_the_consumer() {
    let temp = tempfile::Builder::new()
        .prefix("package identity ")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    let root = temp.path();
    write(root, "Cargo.toml", "[workspace]\nmembers = ['local', 'consumer']\nresolver = '2'\n");
    write(
        root,
        "local/Cargo.toml",
        "[package]\nname = 'itoa'\nversion = '1.0.18'\nedition = '2021'\n",
    );
    write(root, "local/src/lib.rs", "pub fn local() {}\n");
    write(
        root,
        "consumer/Cargo.toml",
        "[package]\nname = 'consumer'\nversion = '0.1.0'\nedition = '2021'\n\
         [dependencies]\npublished = { package = 'itoa', version = '=1.0.18' }\n",
    );
    write(
        root,
        "consumer/src/lib.rs",
        "pub fn published() { let _ = published::Buffer::new(); }\n",
    );
    success(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .output()
            .unwrap(),
    );
    let ambiguous = Command::new("cargo")
        .args(["pkgid", "--locked", "--offline", "-p", "itoa@1.0.18"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!ambiguous.status.success(), "the regression fixture must actually be ambiguous");
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"));

    write(root, ".anvil/packages.ps1", include_str!("../templates/anvil/packages.ps1"));
    write(root, "build.just", include_str!("../templates/justfiles/anvil/dev/build.just"));
    write(
        root,
        "Justfile",
        "set unstable\n_anvil_stable_toolchain_args := \"@()\"\nimport 'build.just'\n\
         anvil-tool-rustc-validate-prereqs:\n\nanvil-toolchain-stable-install:\n",
    );
    success(
        Command::new("just")
            .args(["anvil-build", "--package", "itoa"])
            .current_dir(root)
            .env("TEMP", root)
            .env("TMP", root)
            .output()
            .unwrap(),
    );
    // The same portable artifact can be downloaded into any checkout: there is
    // no producer URL, drive letter, or path to transplant.
    write(root, "include_affected.txt", "--package itoa@1.0.18 --package consumer@0.1.0");
    write(
        root,
        "verify.ps1",
        r"$ErrorActionPreference = 'Stop'
. ./.anvil/packages.ps1
$scope = Get-Content -Raw ./include_affected.txt
$packages = @(Get-AnvilPackages $scope)
if ($packages.Count -ne 2) { throw 'wrong workspace selection' }
$args = @(Get-AnvilPackageArguments $packages)
& cargo check --locked --offline @args
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$llvm = @(Get-AnvilPackageArguments $packages -Adapter llvm-cov)
$each = @(Get-AnvilPackageArguments $packages -Adapter each)
$names = @(Get-AnvilPackageArguments $packages -Adapter names)
if ($llvm[1] -cne $packages[0].id) { throw 'llvm-cov requires the literal metadata ID' }
if ($each[1] -cne 'itoa@1.0.18') { throw 'cargo-each filters workspace specs' }
if ($names[1] -cne 'itoa') { throw 'coverage-gate filters workspace names' }
if ($args[1] -notlike 'file:*#itoa@1.0.18') { throw 'Cargo requires a source-qualified spec' }
$manifest = @(Resolve-AnvilPackageArguments '--package itoa')
if ($manifest[0] -ne '--manifest-path' -or $manifest[1] -cne $packages[0].manifest_path) {
    throw 'single-package commands must use the metadata manifest path'
}

$failed = $false
try { Get-AnvilPackages '--package missing@0.1.0' | Out-Null } catch { $failed = $true }
if (-not $failed) { throw 'invalid cache entries must fail closed' }
& cargo hack --version 2>$null | Out-Null
if ($LASTEXITCODE -eq 0) {
    & cargo hack --package itoa@1.0.18 check --locked --offline
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
& cargo each --version 2>$null | Out-Null
if ($LASTEXITCODE -eq 0) {
    & cargo each @each --keep-going -- cargo check --manifest-path '{manifest}' --package '{spec}' --locked --offline
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
exit 0
",
    );
    success(
        Command::new("pwsh")
            .args(["-NoProfile", "-File", "verify.ps1"])
            .current_dir(root)
            .env("TEMP", root)
            .env("TMP", root)
            .output()
            .unwrap(),
    );
    // A root manifest alone selects default-members, not necessarily its package.
    write(
        root,
        "Cargo.toml",
        "[workspace]\nmembers = ['local', 'consumer']\ndefault-members = ['consumer']\nresolver = '2'\n\
         [package]\nname = 'root-only'\nversion = '0.1.0'\nedition = '2021'\n",
    );
    write(root, "src/lib.rs", "pub fn root_only() {}\n");
    write(
        root,
        "consumer/src/lib.rs",
        "compile_error!(\"default member must not be selected\");\n",
    );
    success(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(root)
            .output()
            .unwrap(),
    );
    success(
        Command::new("just")
            .args(["anvil-build", "--package", "root-only"])
            .current_dir(root)
            .env("TEMP", root)
            .env("TMP", root)
            .output()
            .unwrap(),
    );
}

#[test]
fn portable_scope_uses_consumer_metadata_without_splitting_paths() {
    let script = format!(
        "{}\n{}",
        include_str!("../templates/anvil/packages.ps1"),
        r#"
$ErrorActionPreference = 'Stop'
foreach ($case in @(
    @{ manifest = "/runner's checkout/local crate/Cargo.toml"; id = 'path+file:///runner%27s%20checkout/local%20crate#ohno@0.5.2' },
    @{ manifest = 'C:\runner checkout\local crate\Cargo.toml'; id = 'path+file:///C:/runner%20checkout/local%20crate#ohno@0.5.2' }
)) {
    $script:fixture = @{
        workspace_root = 'unused'
        workspace_members = @($case.id)
        packages = @(
            @{ name = 'ohno'; version = '0.5.2'; id = $case.id; manifest_path = $case.manifest },
            @{ name = 'ohno'; version = '0.5.2'; id = 'registry+https://github.com/rust-lang/crates.io-index#ohno@0.5.2'; manifest_path = '/registry/ohno/Cargo.toml' }
        )
    }
    function cargo {
        $global:LASTEXITCODE = 0
        $script:fixture | ConvertTo-Json -Depth 8 -Compress
    }
    $selected = @(Get-AnvilPackages '--package ohno@0.5.2')
    if ($selected.Count -ne 1 -or $selected[0].id -cne $case.id) { throw 'registry package selected' }
    $manifest = @(Get-AnvilPackageArguments $selected -Adapter manifest)
    if ($manifest.Count -ne 2 -or $manifest[1] -cne $case.manifest) { throw 'manifest argument was split or rewritten' }
    $literal = @(Get-AnvilPackageArguments $selected -Adapter llvm-cov)
    if ($literal[1] -cne $case.id) { throw 'llvm-cov ID was rewritten' }
    $source = @(Get-AnvilPackageArguments $selected)
    if ($source[1] -cne $case.id.Substring(5)) { throw 'wrong Cargo source spec' }
    $explicit = @(Get-AnvilPackages '--package ohno')
    if ($explicit[0].id -cne $case.id) { throw 'explicit override selected registry package' }
    $semver = @(Get-AnvilPackageArguments $selected -Adapter semver)
    if ($semver.Count -ne 4 -or $semver[1] -cne $case.manifest -or $semver[3] -cne 'ohno') { throw 'semver requires manifest and bare name' }
    $selected[0].anvil_workspace_root = Split-Path -Parent $case.manifest
    $rootSemver = @(Get-AnvilPackageArguments $selected -Adapter semver)
    if ($rootSemver.Count -ne 4 -or $rootSemver[2] -cne '--package' -or $rootSemver[3] -cne 'ohno') {
        throw 'root semver must use a bare package name, not the Cargo source selector'
    }
    if (@(Get-AnvilPackages '--skip').Count -ne 0) { throw 'empty scope widened' }
}
"#
    );
    success(Command::new("pwsh").args(["-NoProfile", "-Command", &script]).output().unwrap());
}
