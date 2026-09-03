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
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const HELPERS: &str = include_str!("../templates/justfiles/anvil/helpers.just");
const IMPACT: &str = include_str!("../templates/justfiles/anvil/impact.just");
const BOLERO: &str = include_str!("../templates/justfiles/anvil/checks/bolero.just");
const FMT: &str = include_str!("../templates/justfiles/anvil/checks/fmt.just");
const DOC_TEST: &str = include_str!("../templates/justfiles/anvil/checks/doc-test.just");
const LLVM_COV: &str = include_str!("../templates/justfiles/anvil/checks/llvm-cov.just");
const LOOM: &str = include_str!("../templates/justfiles/anvil/checks/loom.just");
const MSRV_TEST: &str = include_str!("../templates/justfiles/anvil/checks/msrv-test.just");
const SEMVER: &str = include_str!("../templates/justfiles/anvil/checks/semver-check.just");
const EXTERNAL_TYPES: &str = include_str!("../templates/justfiles/anvil/checks/external-types.just");
const TOOLS: &str = include_str!("../templates/justfiles/anvil/tools.just");
const APRZ: &str = include_str!("../templates/justfiles/anvil/checks/aprz.just");
const MUTANTS_DIFF: &str = include_str!("../templates/justfiles/anvil/checks/mutants-diff.just");
const VERSIONS: &str = include_str!("../templates/justfiles/anvil/versions.just");
const REGENERATE_WORKFLOW: &str = include_str!("../../../.github/workflows/regenerate-check.yml");
const CONTAINER: &str = include_str!("../templates/justfiles/anvil/container.just");
// Any nonzero value works; naming it prevents tests from implying an external exit-code contract.
const ARBITRARY_FAILURE_EXIT: &str = "23";

#[test]
fn regeneration_check_runs_on_every_pull_request() {
    assert!(REGENERATE_WORKFLOW.contains("pull_request: {}"));
    assert!(
        !REGENERATE_WORKFLOW.contains("\n    paths:"),
        "the dogfood drift gate must not be limited by changed paths"
    );
}

#[test]
fn loom_does_not_globally_limit_exploration() {
    assert!(
        !LOOM.contains("LOOM_MAX_PREEMPTIONS"),
        "loom models must own their exploration scope rather than inheriting a global preemption cap"
    );
}

#[test]
fn bolero_uses_its_supported_release_profile_option() {
    assert!(
        BOLERO.contains("bolero test --profile release"),
        "bolero execution must select the release profile with cargo-bolero's supported option"
    );
    assert!(
        !BOLERO.contains("bolero test --release"),
        "cargo-bolero 0.13.4 does not accept Cargo's --release shorthand"
    );
}

const FAKE_CARGO_PS1: &str = r#"
$effectiveArgs = @()
foreach ($argument in $args) {
    if ($argument -is [Array]) {
        $effectiveArgs += @($argument)
    } else {
        $effectiveArgs += $argument
    }
}
$args = $effectiveArgs
$joined = $args -join ' '
if ($env:FAKE_CARGO_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_LOG -Value $joined
}
if ($env:FAKE_CARGO_CWD_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_CWD_LOG -Value (Get-Location).Path
}
if ($env:FAKE_CARGO_TOOLCHAIN_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_TOOLCHAIN_LOG -Value $env:RUSTUP_TOOLCHAIN
}
if ($args -contains 'each') {
    exit [int]$env:FAKE_EACH_EXIT
}
if ($args -contains 'metadata') {
    if ($env:FAKE_METADATA_EXIT) { exit [int]$env:FAKE_METADATA_EXIT }
    if ($env:FAKE_METADATA_INVALID) {
        Write-Output '{invalid metadata'
        exit 0
    }
    $root = $env:FAKE_WORKSPACE_ROOT
    $packageName = if ($env:FAKE_PACKAGE_NAME) { $env:FAKE_PACKAGE_NAME } else { 'fixture' }
    $libName = if ($env:FAKE_LIB_NAME) { $env:FAKE_LIB_NAME } else { 'fixture' }
    $manifestPath = if ($env:FAKE_PACKAGE_DIR_LEAF) {
        [System.IO.Path]::Combine($root, $env:FAKE_PACKAGE_DIR_LEAF, 'Cargo.toml')
    } else {
        [System.IO.Path]::Combine($root, 'Cargo.toml')
    }
    $packages = @(
        [pscustomobject]@{
            name = $packageName
            version = '0.1.0'
            id = "$packageName 0.1.0"
            manifest_path = $manifestPath
            targets = @([pscustomobject]@{ name = $libName; kind = @('lib') })
            publish = if ($env:FAKE_PUBLISH_FALSE) {
                # Preserve the empty array through expression output so JSON emits [] rather than null.
                Write-Output -NoEnumerate @()
            } else {
                $null
            }
            metadata = [pscustomobject]@{
                'coverage-gate' = [pscustomobject]@{ 'min-lines-percent' = 0 }
            }
        }
    )
    if ($env:FAKE_SECOND_PACKAGE_NAME) {
        $secondDirLeaf = if ($env:FAKE_SECOND_PACKAGE_DIR_LEAF) {
            $env:FAKE_SECOND_PACKAGE_DIR_LEAF
        } else {
            $env:FAKE_PACKAGE_DIR_LEAF
        }
        $packages += [pscustomobject]@{
            name = $env:FAKE_SECOND_PACKAGE_NAME
            version = '0.1.0'
            id = "$($env:FAKE_SECOND_PACKAGE_NAME) 0.1.0"
            manifest_path = [System.IO.Path]::Combine($root, 'nested', $secondDirLeaf, 'Cargo.toml')
            targets = @([pscustomobject]@{
                name = $env:FAKE_SECOND_PACKAGE_NAME
                kind = if ($env:FAKE_SECOND_BIN_ONLY) { @('bin') } else { @('lib') }
            })
            publish = $null
            metadata = [pscustomobject]@{}
        }
    }
    if ($env:FAKE_THIRD_PACKAGE_NAME) {
        $packages += [pscustomobject]@{
            name = $env:FAKE_THIRD_PACKAGE_NAME
            version = '0.1.0'
            id = "$($env:FAKE_THIRD_PACKAGE_NAME) 0.1.0"
            manifest_path = [System.IO.Path]::Combine(
                $root,
                'nested',
                $env:FAKE_THIRD_PACKAGE_NAME,
                'Cargo.toml'
            )
            targets = @([pscustomobject]@{
                name = $env:FAKE_THIRD_PACKAGE_NAME
                kind = if ($env:FAKE_THIRD_PROC_MACRO) { @('proc-macro') } else { @('lib') }
            })
            publish = @('private-registry')
            metadata = [pscustomobject]@{}
        }
    }
    $metadata = [pscustomobject]@{
        workspace_root = $root
        workspace_members = @($packages | ForEach-Object { $_.id })
        packages = $packages
    }
    if ($env:FAKE_NON_MEMBER_PACKAGE_NAME) {
        # A package present in `packages` but absent from `workspace_members`
        # (a path/registry dependency). Recipes that enumerate the workspace
        # must filter these out; the object is added AFTER workspace_members is
        # computed so it is never listed as a member.
        $metadata.packages += [pscustomobject]@{
            name = $env:FAKE_NON_MEMBER_PACKAGE_NAME
            version = '0.1.0'
            id = "$($env:FAKE_NON_MEMBER_PACKAGE_NAME) 0.1.0"
            manifest_path = [System.IO.Path]::Combine($root, 'external', 'Cargo.toml')
            targets = @([pscustomobject]@{ name = $env:FAKE_NON_MEMBER_PACKAGE_NAME; kind = @('lib') })
            metadata = [pscustomobject]@{}
        }
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
if ($args -contains 'binstall') {
    exit [int]$env:FAKE_BINSTALL_EXIT
}
if ($args -contains 'install' -and $args -contains '--version') {
    exit [int]$env:FAKE_INSTALL_EXIT
}
exit 0
"#;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Seed the impact cache that scoped check recipes read via
/// `_anvil-impact-include`, standing in for a completed `anvil-impact` run.
/// Without a cache file the recipes fall back to their tier default
/// (`--workspace` for the affected tier), so tests that exercise a scoped run
/// must plant the include file the recipe consumes.
fn seed_include(root: &Path, tier: &str, spec: &str) {
    write(&root.join(format!("target/anvil/impact/include_{tier}.txt")), spec);
}

fn tools_available() -> bool {
    Command::new("just").arg("--version").output().is_ok() && Command::new("pwsh").arg("--version").output().is_ok()
}

fn fixture(imports: &[(&str, &str)], dependency_recipes: &[&str]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let mut justfile =
        String::from("set unstable\nset allow-duplicate-recipes\nset windows-shell := [\"pwsh\", \"-NoProfile\", \"-Command\"]\n\n");
    // Focused fixtures need this shared variable, but the real version catalog
    // already defines it and Just rejects duplicate definitions.
    if !imports.iter().any(|(name, _)| *name == "versions.just") {
        justfile.push_str("rust_nightly := \"nightly-test\"\n\n");
        justfile.push_str("rust_nightly_external_types := \"nightly-test\"\n\n");
        // A visibly synthetic placeholder used only for recipe interpolation.
        justfile.push_str("cargo_check_external_types_version := \"0.0.0-test\"\n\n");
        justfile.push_str("_anvil_stable_toolchain_args := \"@()\"\n\n");
    }
    justfile.push_str("anvil-tool-cargo-each-validate-prereqs:\n\n");
    justfile.push_str("anvil-tool-cargo-each-install installer=\"install\":\n\n");
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
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nrust-version = \"1.97\"\n",
    );

    let bin = tmp.path().join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    write(&bin.join("cargo.ps1"), FAKE_CARGO_PS1);
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
    command
        .arg("--justfile")
        .arg(root.join("Justfile"))
        .args(arguments)
        .current_dir(root);
    command.env("PATH", path_with_fake_bin(root));
    command.env("FAKE_WORKSPACE_ROOT", root);
    // A fixture is a scratch workspace, so it must not inherit the impact
    // scoping of whatever invoked the test suite. A CI group job exports
    // `ANVIL_IMPACT=consume` and downloads a cache into the real repository;
    // inherited into a temp directory that has no cache, `anvil-impact` fails
    // hard and takes the recipe under test with it. `ANVIL_INCLUDE_*` is the
    // same hazard one level down: a leg whose scope resolved to `--none` would
    // silently short-circuit the recipe before it did anything. A test that
    // cares about either value passes it explicitly below.
    command.env_remove("ANVIL_IMPACT");
    for key in std::env::vars_os().map(|(key, _)| key) {
        if key.to_string_lossy().starts_with("ANVIL_INCLUDE_") {
            command.env_remove(key);
        }
    }
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
fn stable_command_leaves_environment_toolchain_selection_native() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("versions.just", VERSIONS), ("tools.just", TOOLS)], &[]);
    let justfile_path = tmp.path().join("Justfile");
    let mut justfile = fs::read_to_string(&justfile_path).unwrap();
    justfile.push_str(
        "\n[script(\"pwsh\", \"-NoProfile\")]\n\
         _anvil-test-stable-command:\n\
         \x20   & cargo {{_anvil_stable_toolchain_args}} doc2readme --check\n\
         \x20   if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n",
    );
    write(&justfile_path, &justfile);
    let args_log = tmp.path().join("cargo-args.log");
    let cwd_log = tmp.path().join("cargo-cwd.log");
    let toolchain_log = tmp.path().join("cargo-toolchain.log");

    let output = run_just(
        tmp.path(),
        &["_anvil-test-stable-command"],
        &[
            ("FAKE_CARGO_LOG", args_log.as_os_str()),
            ("FAKE_CARGO_CWD_LOG", cwd_log.as_os_str()),
            ("FAKE_CARGO_TOOLCHAIN_LOG", toolchain_log.as_os_str()),
            ("RUSTUP_TOOLCHAIN", OsStr::new("test-stable")),
        ],
    );

    assert!(
        output.status.success(),
        "direct stable command should preserve arguments:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(args_log).unwrap().trim(), "doc2readme --check");
    assert_eq!(
        fs::canonicalize(fs::read_to_string(cwd_log).unwrap().trim()).unwrap(),
        fs::canonicalize(tmp.path()).unwrap(),
        "the command must run directly in its recipe process"
    );
    assert_eq!(fs::read_to_string(toolchain_log).unwrap().trim(), "test-stable");
}

#[test]
fn msrv_test_propagates_nested_just_failures() {
    if !tools_available() {
        return;
    }
    let helper_recipes = "\n[script(\"pwsh\", \"-NoProfile\")]\n\
         _anvil-impact-include tier:\n\
         \x20   if ($env:FAKE_IMPACT_HELPER_OUTPUT) { Write-Output $env:FAKE_IMPACT_HELPER_OUTPUT }\n\
         \x20   exit [int]$env:FAKE_IMPACT_HELPER_EXIT\n\
         \n[script(\"pwsh\", \"-NoProfile\")]\n\
         _anvil-resolve-stable action:\n\
         \x20   if ($env:FAKE_RESOLVER_OUTPUT) { Write-Output $env:FAKE_RESOLVER_OUTPUT }\n\
         \x20   exit [int]$env:FAKE_RESOLVER_EXIT\n";

    let validation_tmp = fixture(&[("msrv-test.just", MSRV_TEST)], &["anvil-impact"]);
    let validation_justfile = validation_tmp.path().join("Justfile");
    let mut validation_body = fs::read_to_string(&validation_justfile).unwrap();
    validation_body.push_str(helper_recipes);
    write(&validation_justfile, &validation_body);
    let validation_failure = run_just(
        validation_tmp.path(),
        &["anvil-msrv-test-validate-prereqs"],
        &[("FAKE_RESOLVER_EXIT", OsStr::new("30"))],
    );
    assert_failed(&validation_failure, "failed prerequisite MSRV resolver");

    let tmp = fixture(
        &[("msrv-test.just", MSRV_TEST)],
        &["anvil-msrv-test-validate-prereqs", "anvil-impact"],
    );
    let justfile_path = tmp.path().join("Justfile");
    let mut justfile = fs::read_to_string(&justfile_path).unwrap();
    justfile.push_str(helper_recipes);
    write(&justfile_path, &justfile);
    let cargo_log = tmp.path().join("cargo.log");

    let impact_failure = run_just(
        tmp.path(),
        &["anvil-msrv-test"],
        &[
            ("FAKE_IMPACT_HELPER_EXIT", OsStr::new("31")),
            ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
        ],
    );
    assert_failed(&impact_failure, "failed impact helper");
    assert!(
        fs::read_to_string(&cargo_log).unwrap_or_default().is_empty(),
        "Cargo must not run after impact helper failure"
    );

    let resolver_failure = run_just(
        tmp.path(),
        &["anvil-msrv-test"],
        &[
            ("FAKE_IMPACT_HELPER_EXIT", OsStr::new("0")),
            ("FAKE_RESOLVER_EXIT", OsStr::new("32")),
            ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
        ],
    );
    assert_failed(&resolver_failure, "failed MSRV resolver");
    assert!(
        fs::read_to_string(cargo_log).unwrap_or_default().is_empty(),
        "Cargo must not run after resolver failure"
    );
}

#[test]
fn impact_format_resolves_directory_aliases_and_fails_hard() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("impact.just", IMPACT)], &[]);
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
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("unknown package"),
        "unknown package should be diagnosed directly:\n{}",
        String::from_utf8_lossy(&unknown.stderr)
    );

    write(
        &tmp.path().join("impact.json"),
        r#"{"Modified":[],"Affected":["workspace-leaf"],"Required":[]}"#,
    );
    let directory_alias = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[
            ("FAKE_PACKAGE_NAME", OsStr::new("fixture-package")),
            ("FAKE_LIB_NAME", OsStr::new("fixture_lib")),
            ("FAKE_PACKAGE_DIR_LEAF", OsStr::new("workspace-leaf")),
        ],
    );
    assert!(
        directory_alias.status.success(),
        "unique manifest directory alias should resolve:\n{}",
        String::from_utf8_lossy(&directory_alias.stderr)
    );
    assert!(
        String::from_utf8_lossy(&directory_alias.stdout)
            .lines()
            .eq(["--package", "fixture-package"])
    );

    let ambiguous_alias = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[
            ("FAKE_PACKAGE_NAME", OsStr::new("fixture-package")),
            ("FAKE_LIB_NAME", OsStr::new("fixture_lib")),
            ("FAKE_PACKAGE_DIR_LEAF", OsStr::new("workspace-leaf")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other-package")),
        ],
    );
    assert_failed(&ambiguous_alias, "ambiguous cargo-delta directory alias");
    assert!(
        String::from_utf8_lossy(&ambiguous_alias.stderr).contains("ambiguous package identifier"),
        "ambiguous alias should be diagnosed directly:\n{}",
        String::from_utf8_lossy(&ambiguous_alias.stderr)
    );

    write(
        &tmp.path().join("impact.json"),
        r#"{"Modified":[],"Affected":["workspace-leaf"],"Required":[]}"#,
    );
    let cross_namespace_alias = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[
            ("FAKE_PACKAGE_NAME", OsStr::new("workspace-leaf")),
            ("FAKE_LIB_NAME", OsStr::new("fixture_lib")),
            ("FAKE_PACKAGE_DIR_LEAF", OsStr::new("first-package")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other-package")),
            ("FAKE_SECOND_PACKAGE_DIR_LEAF", OsStr::new("workspace-leaf")),
        ],
    );
    assert_failed(
        &cross_namespace_alias,
        "cargo-delta alias that collides across identifier namespaces",
    );

    let metadata_error = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[
            ("FAKE_METADATA_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT)),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );
    assert_failed(&metadata_error, "cargo metadata failure");

    let malformed_metadata = run_just(
        tmp.path(),
        &["_anvil-impact-format", "affected", "impact.json"],
        &[("FAKE_METADATA_INVALID", OsStr::new("1"))],
    );
    assert_failed(&malformed_metadata, "malformed cargo metadata");
    assert!(
        String::from_utf8_lossy(&malformed_metadata.stderr).contains("could not parse cargo metadata output"),
        "malformed metadata should be diagnosed directly:\n{}",
        String::from_utf8_lossy(&malformed_metadata.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bolero_discovery_failure_propagates() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("bolero.just", BOLERO), ("impact.just", IMPACT)],
        &[
            "anvil-toolchain-nightly-validate-prereqs",
            "anvil-tool-cargo-bolero-validate-prereqs",
            "anvil-toolchain-nightly-install",
            "anvil-tool-cargo-bolero-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    seed_include(tmp.path(), "affected", "--package\nfixture");
    let output = run_just(
        tmp.path(),
        &["anvil-bolero"],
        &[("FAKE_BOLERO_LIST_EXIT", OsStr::new("9")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );

    assert_failed(&output, "cargo bolero target discovery failure");
}

#[cfg(target_os = "linux")]
#[test]
fn bolero_full_workspace_discovery_enumerates_every_member() {
    if !tools_available() {
        return;
    }
    // When the affected tier is the whole workspace (`--workspace`, or an
    // adopter running `anvil-bolero` directly with no impact cache), the recipe
    // cannot read package specs from the include string -- it must enumerate
    // the workspace via `cargo metadata` and run target discovery for EVERY
    // member. A regression that only handled the scoped `--package` form, or
    // that dropped members, would silently fuzz nothing. Drive the metadata
    // shim so the workspace has two members and assert both are discovered.
    let tmp = fixture(
        &[("bolero.just", BOLERO), ("impact.just", IMPACT)],
        &[
            "anvil-toolchain-nightly-validate-prereqs",
            "anvil-tool-cargo-bolero-validate-prereqs",
            "anvil-toolchain-nightly-install",
            "anvil-tool-cargo-bolero-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    // A whole-workspace affected tier forces the metadata-enumeration branch
    // (the scoped `--package` branch never calls `cargo metadata`).
    seed_include(tmp.path(), "affected", "--workspace");
    let output = run_just(
        tmp.path(),
        &["anvil-bolero"],
        &[
            // `bolero list` succeeds but reports no targets, so the recipe
            // no-ops after discovery -- exactly the path we want to observe.
            ("FAKE_BOLERO_LIST_EXIT", OsStr::new("0")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other-package")),
            // A package present in metadata but NOT a workspace member must be
            // skipped -- discovery is over members, not every known package.
            ("FAKE_NON_MEMBER_PACKAGE_NAME", OsStr::new("external-dep")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );

    assert!(
        output.status.success(),
        "full-workspace bolero discovery must succeed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains("metadata"),
        "full-workspace discovery must enumerate members via `cargo metadata`, got:\n{calls}"
    );
    assert!(
        calls.contains("bolero list --profile release --package fixture"),
        "the first workspace member must be discovered, got:\n{calls}"
    );
    assert!(
        calls.contains("bolero list --profile release --package other-package"),
        "every workspace member must be discovered, not just the first, got:\n{calls}"
    );
    assert!(
        !calls.contains("external-dep"),
        "a non-workspace-member package must NOT be discovered, got:\n{calls}"
    );
}

#[test]
fn semver_exit_code_contract_is_executed() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("helpers.just", HELPERS), ("semver.just", SEMVER), ("impact.just", IMPACT)],
        &[
            "anvil-tool-cargo-semver-checks-validate-prereqs",
            "anvil-tool-cargo-semver-checks-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    seed_include(tmp.path(), "affected", "--package\nfixture");
    let common = [("BASE_REF", OsStr::new("base")), ("FAKE_CARGO_LOG", log.as_os_str())];

    let findings = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[
            common[0],
            common[1],
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
    let findings_comment = fs::read_to_string(tmp.path().join("target/anvil/comments/semver.md")).unwrap();
    assert!(findings_comment.contains("Potential breaking changes"));
    assert!(findings_comment.contains("breaking change"));

    let renamed = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[
            common[0],
            common[1],
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

    for output in [
        "has no lib target",
        "no library targets found",
        "version 1.0.0 is yanked: target/semver-checks/git-origin_main/crates/fixture",
    ] {
        let bin_to_lib = run_just(
            tmp.path(),
            &["anvil-semver-check"],
            &[
                common[0],
                common[1],
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
        let inconclusive = run_just(
            tmp.path(),
            &["anvil-semver-check"],
            &[
                common[0],
                common[1],
                ("FAKE_SEMVER_EXIT", OsStr::new(exit)),
                ("FAKE_SEMVER_OUTPUT", OsStr::new(output)),
            ],
        );
        assert!(
            inconclusive.status.success(),
            "cargo-semver-checks exit {exit} should be advisory:\n{}",
            String::from_utf8_lossy(&inconclusive.stderr)
        );
        let comment = fs::read_to_string(tmp.path().join("target/anvil/comments/semver.md")).unwrap();
        assert!(comment.contains("Inconclusive comparisons"));
        assert!(comment.contains(&format!("exit {exit}")));
        assert!(comment.contains(output));
    }
}

#[test]
fn install_tool_controls_source_fallback_and_prerequisite_ordering() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("versions.just", VERSIONS), ("tools.just", TOOLS)], &[]);
    let justfile_path = tmp.path().join("Justfile");
    let mut justfile = fs::read_to_string(&justfile_path).unwrap();
    justfile.push_str(
        r#"
[script("pwsh", "-NoProfile")]
source-prereq:
    Add-Content -LiteralPath $env:FAKE_CARGO_LOG -Value 'source-prereq'
    exit [int]$env:FAKE_PREREQ_EXIT
"#,
    );
    write(&justfile_path, &justfile);
    let log = tmp.path().join("cargo.log");

    let fallback = run_just(
        tmp.path(),
        &["_install-tool", "cargo-spellcheck", "0.15.7", "binstall", "source-prereq"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_BINSTALL_EXIT", OsStr::new("7")),
            ("FAKE_PREREQ_EXIT", OsStr::new("0")),
            ("FAKE_INSTALL_EXIT", OsStr::new("0")),
        ],
    );
    assert!(
        fallback.status.success(),
        "controlled source fallback should succeed:\n{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    let log_contents = fs::read_to_string(&log).unwrap();
    let lines = log_contents.lines().collect::<Vec<_>>();
    let binstall = lines
        .iter()
        .position(|line| line.contains("binstall --no-confirm --locked --disable-strategies compile"))
        .expect("source-prerequisite tools must disable binstall compilation");
    let prerequisite = lines
        .iter()
        .position(|line| *line == "source-prereq")
        .expect("source prerequisite must run after binary installation fails");
    let source_install = lines
        .iter()
        .position(|line| line.contains("install --locked cargo-spellcheck --version =0.15.7"))
        .expect("Anvil must perform the controlled source install at the exact pin");
    assert!(binstall < prerequisite && prerequisite < source_install);

    fs::remove_file(&log).unwrap();
    let prerequisite_failure = run_just(
        tmp.path(),
        &["_install-tool", "cargo-spellcheck", "0.15.7", "binstall", "source-prereq"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_BINSTALL_EXIT", OsStr::new("7")),
            ("FAKE_PREREQ_EXIT", OsStr::new("9")),
        ],
    );
    assert_failed(&prerequisite_failure, "source prerequisite failure");
    let failed_log = fs::read_to_string(&log).unwrap();
    assert!(failed_log.contains("source-prereq"));
    assert!(
        !failed_log.contains("install --locked cargo-spellcheck --version =0.15.7"),
        "source installation must not run after prerequisite failure"
    );

    fs::remove_file(&log).unwrap();
    let ordinary_tool = run_just(
        tmp.path(),
        &["_install-tool", "cargo-other", "1.2.3", "binstall", ""],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_BINSTALL_EXIT", OsStr::new("7")),
            ("FAKE_INSTALL_EXIT", OsStr::new("0")),
        ],
    );
    assert!(ordinary_tool.status.success());
    let ordinary_log = fs::read_to_string(&log).unwrap();
    let ordinary_binstall = ordinary_log
        .lines()
        .find(|line| line.contains("binstall --no-confirm --locked"))
        .expect("ordinary tool must attempt binstall");
    assert!(
        !ordinary_binstall.contains("--disable-strategies compile"),
        "tools without source prerequisites retain binstall's compile strategy"
    );
}

#[test]
fn repository_constants_match_shared_anvil_versions() {
    let constants_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../constants.env");
    if !constants_path.is_file() {
        return;
    }

    let repository_constants = fs::read_to_string(constants_path).unwrap();
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

    let shared_keys = [
        "rust_nightly_external_types",
        "cargo_careful_version",
        "cargo_check_external_types_version",
        "cargo_deny_version",
        "cargo_doc2readme_version",
        "cargo_ensure_no_cyclic_deps_version",
        "cargo_ensure_no_default_features_version",
        "cargo_hack_version",
        "cargo_llvm_cov_version",
        "cargo_mutants_version",
        "cargo_nextest_version",
        "cargo_semver_checks_version",
        "cargo_sort_version",
        "cargo_spellcheck_version",
        "cargo_udeps_version",
    ];
    let intentionally_unshared_keys = [
        "rust_msrv",
        "rust_latest",
        // The legacy workflow's broad nightly follows rust-toolchain.toml,
        // while Anvil's general-purpose nightly has its own compatibility cadence.
        "rust_nightly",
        "just_version",
        "sccache_version",
    ];
    let mut mismatches = shared_keys
        .iter()
        .filter_map(|name| match (constants.get(*name), anvil_versions.get(*name)) {
            (Some(constant), Some(anvil)) if constant == anvil => None,
            (Some(constant), Some(anvil)) => Some(format!("{name}: constants.env={constant}, anvil={anvil}")),
            (None, Some(_)) => Some(format!("{name}: missing from constants.env")),
            (Some(_), None) => Some(format!("{name}: missing from versions.just")),
            (None, None) => Some(format!("{name}: missing from constants.env and versions.just")),
        })
        .collect::<Vec<_>>();
    let known_constants = shared_keys
        .iter()
        .chain(intentionally_unshared_keys.iter())
        .copied()
        .collect::<std::collections::HashSet<_>>();
    mismatches.extend(
        constants
            .keys()
            .filter(|name| !known_constants.contains(name.as_str()))
            .map(|name| format!("{name}: constants.env key is not classified as shared or intentionally unshared")),
    );

    assert!(
        mismatches.is_empty(),
        "the explicit shared repository and Anvil version set must be present and match:\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn semver_check_fails_when_metadata_discovery_fails() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("semver.just", SEMVER), ("impact.just", IMPACT)],
        &[
            "anvil-tool-cargo-semver-checks-validate-prereqs",
            "anvil-tool-cargo-semver-checks-install installer",
            "anvil-impact",
        ],
    );
    seed_include(tmp.path(), "affected", "--package\nfixture");
    let output = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[("FAKE_METADATA_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT))],
    );
    assert_failed(&output, "anvil-semver-check cargo metadata failure");

    let malformed = run_just(tmp.path(), &["anvil-semver-check"], &[("FAKE_METADATA_INVALID", OsStr::new("1"))]);
    assert_failed(&malformed, "anvil-semver-check malformed cargo metadata");
}

#[test]
fn fmt_delegates_workspace_iteration_to_cargo_each() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("fmt.just", FMT), ("impact.just", IMPACT)],
        &[
            "anvil-component-nightly-rustfmt-validate-prereqs",
            "anvil-component-nightly-rustfmt-install",
            "anvil-tool-cargo-each-validate-prereqs",
            "anvil-tool-cargo-each-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    let output = run_just(tmp.path(), &["anvil-fmt"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
    assert!(
        output.status.success(),
        "per-package formatting failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(&log).unwrap();
    assert!(
        commands.contains("each --workspace --keep-going -- cargo +nightly-test fmt --manifest-path {manifest} --check"),
        "unexpected cargo invocation: {commands}"
    );
    assert!(!commands.contains("fmt --all"));
}

#[test]
fn fmt_propagates_cargo_each_failure() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("fmt.just", FMT), ("impact.just", IMPACT)],
        &[
            "anvil-component-nightly-rustfmt-validate-prereqs",
            "anvil-component-nightly-rustfmt-install",
            "anvil-tool-cargo-each-validate-prereqs",
            "anvil-tool-cargo-each-install installer",
            "anvil-impact",
        ],
    );
    let output = run_just(
        tmp.path(),
        &["anvil-fmt"],
        &[("FAKE_EACH_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT))],
    );
    assert_failed(&output, "anvil-fmt cargo-each failure");
}

#[test]
fn doc_test_selects_libraries_and_proc_macros_but_not_bin_only_packages() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("doc-test.just", DOC_TEST), ("impact.just", IMPACT)],
        &[
            "anvil-tool-rustc-validate-prereqs",
            "anvil-toolchain-stable-install",
            "anvil-impact",
        ],
    );
    seed_include(tmp.path(), "affected", "--workspace");
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-doc-test"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("bin-only")),
            ("FAKE_SECOND_BIN_ONLY", OsStr::new("1")),
            ("FAKE_THIRD_PACKAGE_NAME", OsStr::new("macro-package")),
            ("FAKE_THIRD_PROC_MACRO", OsStr::new("1")),
        ],
    );
    assert!(
        output.status.success(),
        "doc-test selection failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(log).unwrap();
    assert_eq!(commands.matches(" test --doc ").count(), 2, "commands:\n{commands}");
    assert!(commands.contains("--package fixture"), "commands:\n{commands}");
    assert!(commands.contains("--package macro-package"), "commands:\n{commands}");
    assert!(!commands.contains("--package bin-only"), "commands:\n{commands}");
}

#[test]
fn external_types_delegates_library_selection_and_iteration_to_cargo_each() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("external-types.just", EXTERNAL_TYPES), ("impact.just", IMPACT)],
        &[
            "anvil-tool-cargo-check-external-types-validate-prereqs",
            "anvil-toolchain-nightly-external-types-validate-prereqs",
            "anvil-tool-cargo-check-external-types-install installer",
            "anvil-toolchain-nightly-external-types-install",
            "anvil-impact",
        ],
    );
    seed_include(tmp.path(), "affected", "--workspace");
    let log = tmp.path().join("cargo.log");
    let output = run_just(tmp.path(), &["anvil-external-types"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
    assert!(
        output.status.success(),
        "library selection failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("each --workspace --filter lib --keep-going"));
    assert!(commands.contains("check-external-types --manifest-path {manifest}"));
    assert!(
        !commands.contains("metadata --no-deps"),
        "the recipe must not duplicate cargo-each metadata filtering"
    );
}

#[test]
fn semver_skips_non_publishable_libraries() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("helpers.just", HELPERS), ("semver.just", SEMVER), ("impact.just", IMPACT)],
        &[
            "anvil-tool-cargo-semver-checks-validate-prereqs",
            "anvil-tool-cargo-semver-checks-install installer",
            "anvil-impact",
        ],
    );
    seed_include(tmp.path(), "affected", "--package\nfixture");
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[("FAKE_CARGO_LOG", log.as_os_str()), ("FAKE_PUBLISH_FALSE", OsStr::new("1"))],
    );
    assert!(
        output.status.success(),
        "non-publishable semver filtering failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fs::read_to_string(log).unwrap().contains("semver-checks"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("no affected publishable library crates"));
}

#[test]
fn semver_includes_libraries_restricted_to_named_registries() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("helpers.just", HELPERS), ("semver.just", SEMVER), ("impact.just", IMPACT)],
        &[
            "anvil-tool-cargo-semver-checks-validate-prereqs",
            "anvil-tool-cargo-semver-checks-install installer",
            "anvil-impact",
        ],
    );
    seed_include(tmp.path(), "affected", "--package\nnamed-registry");
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-semver-check"],
        &[
            ("BASE_REF", OsStr::new("base")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_THIRD_PACKAGE_NAME", OsStr::new("named-registry")),
        ],
    );

    assert!(
        output.status.success(),
        "named-registry SemVer selection failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("semver-checks --package named-registry --baseline-rev base")
    );
}

#[test]
fn all_coverage_opted_out_packages_run_both_test_configurations() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("llvm-cov.just", LLVM_COV), ("impact.just", IMPACT)],
        &[
            "anvil-component-nightly-llvm-tools-validate-prereqs",
            "anvil-tool-cargo-llvm-cov-validate-prereqs",
            "anvil-tool-cargo-nextest-validate-prereqs",
            "anvil-tool-cargo-coverage-gate-validate-prereqs",
            "anvil-component-nightly-llvm-tools-install",
            "anvil-tool-cargo-llvm-cov-install installer",
            "anvil-tool-cargo-nextest-install installer",
            "anvil-tool-cargo-coverage-gate-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    seed_include(tmp.path(), "affected", "--package\nfixture");
    let output = run_just(
        tmp.path(),
        &["anvil-llvm-cov"],
        &[("FAKE_NEXTEST_EXIT", OsStr::new("0")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert!(
        output.status.success(),
        "all-opted-out coverage path should succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("nextest run").count(), 2, "calls:\n{calls}");
    assert!(calls.contains("--all-features"), "calls:\n{calls}");
    assert!(calls.contains("--no-default-features"), "calls:\n{calls}");
    assert_eq!(calls.matches("--no-tests=pass").count(), 2, "calls:\n{calls}");
    assert!(!calls.contains("llvm-cov"), "coverage commands must not run:\n{calls}");
    assert!(!calls.contains("coverage-gate"), "the coverage gate must not run:\n{calls}");

    let no_tests = run_just(tmp.path(), &["anvil-llvm-cov"], &[("FAKE_NEXTEST_EXIT", OsStr::new("4"))]);
    assert!(
        no_tests.status.success(),
        "opted-out packages with no runnable tests should succeed:\n{}",
        String::from_utf8_lossy(&no_tests.stderr)
    );

    let failed = run_just(
        tmp.path(),
        &["anvil-llvm-cov"],
        &[("FAKE_NEXTEST_EXIT", OsStr::new("7")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert_failed(&failed, "plain nextest failure for an opted-out package");
}

#[cfg(windows)]
#[test]
fn windows_arm64_fallback_accepts_empty_nextest_sets_in_both_configurations() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("llvm-cov.just", LLVM_COV), ("impact.just", IMPACT)],
        &[
            "anvil-component-nightly-llvm-tools-validate-prereqs",
            "anvil-tool-cargo-llvm-cov-validate-prereqs",
            "anvil-tool-cargo-nextest-validate-prereqs",
            "anvil-tool-cargo-coverage-gate-validate-prereqs",
            "anvil-component-nightly-llvm-tools-install",
            "anvil-tool-cargo-llvm-cov-install installer",
            "anvil-tool-cargo-nextest-install installer",
            "anvil-tool-cargo-coverage-gate-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    seed_include(tmp.path(), "affected", "--package\nfixture");
    let output = run_just(
        tmp.path(),
        &["anvil-llvm-cov"],
        &[
            ("PROCESSOR_ARCHITECTURE", OsStr::new("ARM64")),
            ("FAKE_NEXTEST_EXIT", OsStr::new("4")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "Windows ARM64 fallback should accept empty nextest sets:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("nextest run").count(), 2, "calls:\n{calls}");
    assert!(calls.contains("--all-features"), "calls:\n{calls}");
    assert!(calls.contains("--no-default-features"), "calls:\n{calls}");
    assert_eq!(calls.matches("--no-tests=pass").count(), 2, "calls:\n{calls}");
    assert!(!calls.contains("llvm-cov"), "coverage commands must not run:\n{calls}");
}

// --- container-specific behaviour ------------------------------------------

/// `anvil-aprz` warns and proceeds when it cannot obtain a token, rather than
/// throwing. That change exists so a containerized tier is not aborted by a
/// missing credential, and nothing else covers it: the dogfood run normally has
/// a host token, and the tokenless container E2E case runs a custom echo recipe.
#[test]
fn aprz_without_a_token_warns_and_still_runs() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("aprz.just", APRZ)],
        &[
            "anvil-tool-cargo-aprz-validate-prereqs",
            "anvil-tool-cargo-aprz-install installer=\"install\"",
        ],
    );
    // A gh that yields no token: the recipe must fall through to the warnings
    // rather than treating a failed lookup as fatal.
    //
    // Three stubs because command lookup differs by platform and the fallback
    // is the developer's real, signed-in `gh`: on Windows only `.cmd` is in
    // PATHEXT, so a `.ps1` stub is skipped; on Unix a bare `gh` must exist and
    // be executable. Getting this wrong does not fail the test -- it makes it
    // pass while exercising the authenticated path, which is the opposite of
    // what the name claims.
    write(&tmp.path().join("fake-bin/gh.cmd"), "@exit /b 1\r\n");
    write(&tmp.path().join("fake-bin/gh.ps1"), "exit 1\n");
    let unix_stub = tmp.path().join("fake-bin/gh");
    write(&unix_stub, "#!/bin/sh\nexit 1\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&unix_stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = tmp.path().join("cargo.log");

    let output = run_just(
        tmp.path(),
        &["anvil-aprz"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("GITHUB_TOKEN", OsStr::new("")),
            ("ANVIL_IN_CONTAINER", OsStr::new("1")),
        ],
    );

    assert!(
        output.status.success(),
        "a missing token must not fail the check\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // PowerShell's warning stream surfaces on stdout once `just` has run the
    // script, so assert on what the developer actually sees rather than on a
    // particular stream.
    let seen = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        seen.contains("GITHUB_TOKEN is not set"),
        "the warning must name the variable:\n{seen}"
    );
    assert!(seen.contains("gh auth login"), "the warning must say how to fix it:\n{seen}");

    // The point of warning rather than throwing: the check still runs.
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(calls.contains("aprz deps"), "cargo aprz must still be invoked:\n{calls}");
}

/// `anvil-mutants-diff` diffs the base against the WORKING TREE, not against
/// HEAD. cargo-mutants validates every diff line against the file on disk and
/// aborts when they disagree, so a commit-to-commit diff fails as soon as
/// anything is uncommitted -- the normal local state, and the one CI never
/// exercises because its tree is clean.
#[test]
fn mutants_diff_covers_uncommitted_work() {
    if !tools_available() || Command::new("git").arg("--version").output().is_err() {
        return;
    }
    // On aarch64-pc-windows-msvc the recipe bails out before doing any of this,
    // because cargo-mutants does not build there -- so there is no `--in-diff`
    // behavior to assert. The architecture cannot be faked past: Windows
    // re-derives PROCESSOR_ARCHITECTURE for every new process from its real
    // architecture, so an override does not survive the spawn. The skip itself
    // is covered by `mutants_diff_skips_on_arm64_windows`, and this contract is
    // exercised on the other three legs.
    if cfg!(windows) && cfg!(target_arch = "aarch64") {
        return;
    }
    let tmp = fixture(
        &[
            ("helpers.just", HELPERS),
            ("impact.just", IMPACT),
            ("mutants-diff.just", MUTANTS_DIFF),
        ],
        &[
            "anvil-tool-cargo-mutants-validate-prereqs",
            "anvil-tool-cargo-mutants-install installer=\"install\"",
        ],
    );
    let root = tmp.path();
    // Real git: the stub the fixture installs would make `git diff` a no-op.
    std::fs::remove_file(root.join("fake-bin/git.ps1")).unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(
            status.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "test"]);
    // The host's global config decides line-ending rewriting and commit
    // signing, and either will stop this fixture: a machine set to autocrlf
    // rejects the add outright ("LF would be replaced by CRLF"), and one with
    // commit.gpgsign and no usable key or TTY fails the commit before the
    // behaviour under test runs. Pin both so the test means the same thing on
    // every developer's box.
    git(&["config", "core.autocrlf", "false"]);
    git(&["config", "core.safecrlf", "false"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "tag.gpgsign", "false"]);
    write(&root.join("src/lib.rs"), "pub fn base() {}\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    let base = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    // One change committed after the base, and one left uncommitted. A
    // `base..HEAD` diff sees only the first.
    write(&root.join("src/lib.rs"), "pub fn base() {}\npub fn committed() {}\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "committed change"]);
    write(
        &root.join("src/lib.rs"),
        "pub fn base() {}\npub fn committed() {}\npub fn uncommitted() {}\n",
    );

    let log = root.join("cargo.log");
    let output = run_just(
        root,
        &["anvil-mutants-diff"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("BASE_REF", OsStr::new(&base)),
            ("RUNNER_TEMP", root.as_os_str()),
            // The architecture guard is deliberately *not* pinned: Windows
            // re-derives PROCESSOR_ARCHITECTURE for each new process from the
            // process's real architecture, so it cannot be overridden across a
            // spawn. That is why this test returns early on ARM64 above rather
            // than faking its way past the branch.
            // The recipe depends on `anvil-impact`, which would otherwise
            // invoke cargo-delta against this fixture. This contract exercises
            // diff construction, not impact computation.
            ("ANVIL_IMPACT", OsStr::new("off")),
        ],
    );
    assert!(
        output.status.success(),
        "the recipe must succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains("--in-diff"),
        "cargo mutants must be given a diff file.\ncargo log:\n{calls}\nrecipe stdout:\n{}\nrecipe stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let diff = std::fs::read_to_string(root.join("anvil-mutants-diff.diff")).unwrap();
    assert!(diff.contains("committed"), "the committed change must be in the diff:\n{diff}");
    assert!(
        diff.contains("uncommitted"),
        "the uncommitted change must be in the diff -- a base..HEAD diff would omit it:\n{diff}"
    );
}

/// The ARM64 Windows bail-out is a documented behavior, not an accident:
/// cargo-mutants does not build for `aarch64-pc-windows-msvc`, so the recipe
/// exits cleanly rather than failing the merged `pr-slow` group on that leg.
///
/// This runs only on a real ARM64 Windows host, which CI has. Faking the
/// architecture is not an option: `PROCESSOR_ARCHITECTURE` is load-bearing for
/// the Windows loader, and setting it to ARM64 on an x64 host makes spawning
/// `just` fail outright rather than exercise the branch.
///
/// Asserting it here is what keeps the sibling test above honest. That one pins
/// the architecture to AMD64 so it exercises the real path; without this test
/// the skip branch would be exercised by nothing.
#[test]
fn mutants_diff_skips_on_arm64_windows() {
    if !tools_available() {
        return;
    }
    if !(cfg!(windows) && cfg!(target_arch = "aarch64")) {
        return;
    }
    let tmp = fixture(
        &[
            ("helpers.just", HELPERS),
            ("impact.just", IMPACT),
            ("mutants-diff.just", MUTANTS_DIFF),
        ],
        &[
            "anvil-tool-cargo-mutants-validate-prereqs",
            "anvil-tool-cargo-mutants-install installer=\"install\"",
        ],
    );
    let root = tmp.path();

    let log = root.join("cargo.log");
    let output = run_just(
        root,
        &["anvil-mutants-diff"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("RUNNER_TEMP", root.as_os_str()),
            // Same reason as the sibling contract: `anvil-mutants-diff` depends
            // on `anvil-impact`, and this test is about the architecture
            // bail-out, not about computing an impact set.
            ("ANVIL_IMPACT", OsStr::new("off")),
        ],
    );

    assert!(
        output.status.success(),
        "the recipe must skip cleanly, not fail, on aarch64-pc-windows-msvc\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cargo-mutants does not build here"),
        "the skip must say why, or a silent no-op looks like a passing run:\n{stdout}"
    );
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "cargo must not be invoked at all on the skipped leg"
    );
}

/// The wrapper that disables impact scoping for the full-workspace tiers is
/// only correct if the export happens *before* the wrapped recipe's
/// dependencies run: `just` evaluates dependencies in their own processes, and
/// every impact-scoped check reads `ANVIL_IMPACT` as a dependency of the tier,
/// not in the tier's own body. A fixture dependency that fails unless the
/// variable is already set pins that ordering; invoking the wrapped recipe
/// directly is the negative control that proves the fixture can fail.
#[test]
fn unscoped_wrapper_exports_impact_off_before_dependencies_run() {
    const PROBE: &str = "[private]\n_anvil-probe: probe-dep\n\n\
        [private]\n[script(\"pwsh\", \"-NoProfile\")]\nprobe-dep:\n    \
        if ($env:ANVIL_IMPACT -ne 'off') { exit 9 }\n    exit 0\n";

    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("helpers.just", HELPERS), ("probe.just", PROBE)], &[]);
    let root = tmp.path();

    let wrapped = run_just(root, &["_anvil-unscoped", "probe"], &[]);
    assert!(
        wrapped.status.success(),
        "the dependency must observe ANVIL_IMPACT=off\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&wrapped.stdout),
        String::from_utf8_lossy(&wrapped.stderr)
    );

    let direct = run_just(root, &["_anvil-probe"], &[]);
    assert_eq!(
        direct.status.code(),
        Some(9),
        "without the wrapper the dependency must see no setting, or this test proves nothing\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&direct.stdout),
        String::from_utf8_lossy(&direct.stderr)
    );
}

/// `just --dry-run` reports the bodies just runs itself, not the body of a
/// recipe that one of them launches as a child process. The unscoped wrapper
/// launches its tier that way, so a plan of the public tier name reveals the
/// wrapper alone.
///
/// The container driver decides whether to mint a GitHub token by matching the
/// plan for `GITHUB_TOKEN`, so this is why it has to follow each nested target
/// rather than reading one plan. If this test ever fails because a plan now
/// reaches through the child process, that expansion can be deleted.
#[test]
fn a_wrapped_tier_hides_its_checks_from_a_plan() {
    const PROBE: &str = "[private]\n[script(\"pwsh\", \"-NoProfile\")]\n_anvil-probe:\n    \
        if (-not $env:GITHUB_TOKEN) { exit 1 }\n\n\
        probe: (_anvil-unscoped \"probe\")\n";

    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("helpers.just", HELPERS), ("probe.just", PROBE)], &[]);
    let root = tmp.path();

    let wrapped = run_just(root, &["--dry-run", "probe"], &[]);
    let wrapped_plan = format!(
        "{}{}",
        String::from_utf8_lossy(&wrapped.stdout),
        String::from_utf8_lossy(&wrapped.stderr)
    );
    assert!(
        !wrapped_plan.contains("GITHUB_TOKEN"),
        "a wrapped tier's plan must not reach the recipe it launches, or the driver's expansion is dead code\n{wrapped_plan}"
    );
    assert!(
        wrapped_plan.contains("_anvil-probe"),
        "the wrapper must still name the recipe it launches, which is what the driver follows\n{wrapped_plan}"
    );

    let direct = run_just(root, &["--dry-run", "_anvil-probe"], &[]);
    let direct_plan = format!(
        "{}{}",
        String::from_utf8_lossy(&direct.stdout),
        String::from_utf8_lossy(&direct.stderr)
    );
    assert!(
        direct_plan.contains("GITHUB_TOKEN"),
        "planning the launched recipe directly must reveal the variable, or this test proves nothing\n{direct_plan}"
    );
}

/// The engine derives the ignore file's name from the Dockerfile's, and anvil
/// maintains that artifact at a fixed canonical path, so the two names have to
/// agree. A case variant is refused rather than accommodated: building from
/// `dockerfile` would find no `dockerfile.dockerignore`, silently stream the
/// whole worktree into the build context, and admit inputs the tag does not
/// cover.
#[test]
fn a_case_variant_dockerfile_is_refused_rather_than_built_from() {
    if !tools_available() {
        return;
    }

    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let root = tmp.path();
    write(&root.join(".anvil/container/Dockerfile"), "FROM scratch\n");
    let canonical = run_just(root, &["_anvil-container-dockerfile"], &[]);
    assert!(
        canonical.status.success(),
        "the canonical name must resolve\nstderr:\n{}",
        String::from_utf8_lossy(&canonical.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&canonical.stdout).trim(), ".anvil/container/Dockerfile");

    // Only a case-sensitive filesystem can hold a variant that is a different
    // file, which is exactly where the ignore-file lookup breaks.
    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let root = tmp.path();
    write(&root.join(".anvil/container/dockerfile"), "FROM scratch\n");
    let holds_variant = !root.join(".anvil/container/Dockerfile").exists();
    if holds_variant {
        let variant = run_just(root, &["_anvil-container-dockerfile"], &[]);
        assert_failed(&variant, "resolving a case-variant Dockerfile");
        let stderr = String::from_utf8_lossy(&variant.stderr);
        assert!(
            stderr.contains("must be named exactly") && stderr.contains("dockerignore"),
            "the refusal must name the rule and the reason\nstderr:\n{stderr}"
        );
    }

    // Absent, the tag would hash a directory that contributes nothing for it
    // and hand back a confident reference to an image that cannot be built.
    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let missing = run_just(tmp.path(), &["_anvil-container-dockerfile"], &[]);
    assert_failed(&missing, "resolving an absent Dockerfile");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("container image input is missing"),
        "the failure must name the missing input\nstderr:\n{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}
/// `COPY` carries a file's executable bit into the image, so a `chmod +x` with
/// no content change still changes what the image contains. The tag has to
/// follow it, or the changed image keeps a reference that already resolves and
/// the stale one is reused.
///
/// The bit is read from git's index rather than the filesystem, because Windows
/// has no such bit and two checkouts of one commit must agree on the tag. The
/// fixture's stub git is what makes that observable from either platform.
#[test]
fn the_image_tag_follows_the_executable_bit() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let root = tmp.path();
    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    write(&root.join(".anvil/container/Dockerfile"), "FROM scratch\n");
    write(&root.join(".anvil/container/Dockerfile.dockerignore"), "*\n!justfiles\n");
    write(&root.join("justfiles/anvil/setup.sh"), "echo hello\n");
    write(
        &root.join("fake-bin/git.ps1"),
        "if ($args -contains 'ls-files' -and $env:FAKE_UNTRACKED -ne '1') {\n    \
         $mode = if ($env:FAKE_EXECUTABLE -eq '1') { '100755' } else { '100644' }\n    \
         Write-Output \"$mode 0000000000000000000000000000000000000000 0`tjustfiles/anvil/setup.sh\"\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`trust-toolchain.toml\"\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`t.anvil/container/Dockerfile\"\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`t.anvil/container/Dockerfile.dockerignore\"\n}\nexit 0\n",
    );

    let tag = |executable: &str| {
        let output = run_just(root, &["anvil-container-tag"], &[("FAKE_EXECUTABLE", OsStr::new(executable))]);
        assert!(
            output.status.success(),
            "computing the tag failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    // A freshly generated repository has staged nothing, so the first container
    // run meets a tree git knows nothing about. It must still compute a tag:
    // an untracked file is in no commit, so no other checkout reproduces it and
    // no published image depends on it.
    let unstaged = run_just(root, &["anvil-container-tag"], &[("FAKE_UNTRACKED", OsStr::new("1"))]);
    assert!(
        unstaged.status.success(),
        "an untracked input must not block the tag\nstderr:\n{}",
        String::from_utf8_lossy(&unstaged.stderr)
    );

    // The ignore file is what narrows the build context to `justfiles/anvil`.
    // Absent, the build still succeeds but copies files the digest never
    // hashes, so the tag stops covering what the image contains. The walk
    // cannot notice an absent file, so it is named as a required input.
    std::fs::remove_file(root.join(".anvil/container/Dockerfile.dockerignore")).unwrap();
    let missing = run_just(root, &["anvil-container-tag"], &[("FAKE_EXECUTABLE", OsStr::new("0"))]);
    assert_failed(&missing, "computing a tag without the ignore file");
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing"),
        "the failure must name the missing input\nstderr:\n{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    write(&root.join(".anvil/container/Dockerfile.dockerignore"), "*\n!justfiles\n");

    let plain = tag("0");
    let executable = tag("1");
    assert_ne!(
        plain, executable,
        "the executable bit must reach the digest, or a chmod leaves the image unnamed"
    );
    assert_eq!(plain, tag("0"), "the tag must depend on the inputs alone");
}

/// The tag is computed from the index while the build copies the working tree,
/// so the two have to agree about the executable bit. Where they do not, the
/// reference names an image the build does not produce, and the run stops
/// rather than absorbing it.
///
/// `git diff --raw` has three shapes here and only one of them is drift, so
/// each is pinned: an ordinary modification, a deletion (absent from both the
/// context and the digest), and an intent-to-add entry, whose raw index mode is
/// zero even though `ls-files --stage` reports a real placeholder mode.
#[test]
fn a_working_tree_mode_the_tag_did_not_frame_stops_the_run() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let root = tmp.path();
    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    write(&root.join(".anvil/container/Dockerfile"), "FROM scratch\n");
    write(&root.join(".anvil/container/Dockerfile.dockerignore"), "*\n!justfiles\n");
    write(&root.join("justfiles/anvil/setup.sh"), "echo hello\n");
    // The digest frames this path from `ls-files --stage`, which reports
    // 100644 in every case below -- including the intent-to-add ones, where
    // the raw index mode is zero but the placeholder is a real mode.
    write(
        &root.join("fake-bin/git.ps1"),
        "if ($args -contains 'ls-files' -and $env:FAKE_UNTRACKED -ne '1') {\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`tjustfiles/anvil/setup.sh\"\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`trust-toolchain.toml\"\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`t.anvil/container/Dockerfile\"\n    \
         Write-Output \"100644 0000000000000000000000000000000000000000 0`t.anvil/container/Dockerfile.dockerignore\"\n}\n\
         if ($args -contains 'diff' -and $env:FAKE_RAW) {\n    Write-Output $env:FAKE_RAW\n}\nexit 0\n",
    );

    let tag = |raw: &str| run_just(root, &["anvil-container-tag"], &[("FAKE_RAW", OsStr::new(raw))]);

    for (kind, raw) in [
        ("no working-tree change at all", ""),
        ("an unstaged deletion", ":100644 000000 0000000 0000000 D\tjustfiles/anvil/setup.sh"),
        (
            "an intent-to-add entry that is not executable",
            ":000000 100644 0000000 0000000 A\tjustfiles/anvil/setup.sh",
        ),
        (
            "an edit that leaves the mode alone",
            ":100644 100644 0000000 0000000 M\tjustfiles/anvil/setup.sh",
        ),
    ] {
        let output = tag(raw);
        assert!(
            output.status.success(),
            "{kind} must not be reported as drift\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    for (kind, raw) in [
        ("an unstaged chmod +x", ":100644 100755 0000000 0000000 M\tjustfiles/anvil/setup.sh"),
        (
            "an intent-to-add entry that is executable",
            ":000000 100755 0000000 0000000 A\tjustfiles/anvil/setup.sh",
        ),
        (
            "a regular file replaced by a symlink",
            ":100644 120000 0000000 0000000 T\tjustfiles/anvil/setup.sh",
        ),
    ] {
        let output = tag(raw);
        assert_failed(&output, kind);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // PowerShell wraps an error record and decorates each continuation, so
        // a multi-word phrase is not a substring of what reaches stderr. It
        // wraps at spaces though, so individual words survive intact.
        assert!(
            stderr.contains("working") && stderr.contains("Stage"),
            "{kind} must name the drift and the recovery\nstderr:\n{stderr}"
        );
    }
}

/// The engine copies a link as a link, while any read of one here follows it,
/// so a retarget changes the image without changing a byte the walk can see --
/// and a link to a directory is not enumerated by the walk at all. Framing the
/// link text instead would have to work on Windows, where git materializes a
/// symlink as an ordinary file unless the checkout was privileged, so the same
/// commit would digest differently per platform. Anvil creates no link under
/// these trees, so the whole class is refused.
#[test]
fn a_link_among_the_image_inputs_is_refused() {
    if !tools_available() {
        return;
    }

    // A walk only ever reports descendants, so a link that *is* a declared
    // input or a walk root is followed and never appears in its own output.
    // Both positions are covered.
    for (kind, name, links_a_directory) in [
        ("a file link below a walk root", "justfiles/anvil/linked.just", false),
        ("a directory link below a walk root", "justfiles/anvil/linked", true),
        ("a linked declared input", "rust-toolchain.toml", false),
        ("a linked recipe walk root", "justfiles/anvil", true),
        ("a linked container walk root", ".anvil/container", true),
    ] {
        let tmp = fixture(&[("container.just", CONTAINER)], &[]);
        let root = tmp.path();
        write(&root.join("elsewhere/target.just"), "# shared\n");
        write(&root.join("elsewhere/Dockerfile"), "FROM scratch\n");
        // Everything the tag needs, except whatever this case replaces with a
        // link. The link stands in for it, so writing it first would defeat the
        // case for a walk root and leave nothing to link at all.
        for (path, body) in [
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n"),
            (".anvil/container/Dockerfile", "FROM scratch\n"),
            (".anvil/container/Dockerfile.dockerignore", "*\n!justfiles\n"),
            ("justfiles/anvil/mod.just", "# recipes\n"),
        ] {
            if !path.starts_with(name) {
                write(&root.join(path), body);
            }
        }

        let link = root.join(name);
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let created = if links_a_directory {
            symlink_dir(&root.join("elsewhere"), &link)
        } else {
            symlink_file(&root.join("elsewhere/target.just"), &link)
        };
        // Creating a link needs a privilege that not every environment grants.
        // Where it is refused there is nothing to assert about.
        if created.is_err() {
            continue;
        }

        let output = run_just(root, &["anvil-container-tag"], &[]);
        assert_failed(&output, &format!("computing a tag with {kind} among the inputs"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("regular") && stderr.contains(name.rsplit('/').next().unwrap()),
            "{kind} must be named in the refusal\nstderr:\n{stderr}"
        );
    }
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
