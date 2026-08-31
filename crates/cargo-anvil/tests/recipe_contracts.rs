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
#[cfg(target_os = "linux")]
const BOLERO: &str = include_str!("../templates/justfiles/anvil/checks/bolero.just");
const LLVM_COV: &str = include_str!("../templates/justfiles/anvil/checks/llvm-cov.just");
const SEMVER: &str = include_str!("../templates/justfiles/anvil/checks/semver-check.just");
const EXTERNAL_TYPES: &str = include_str!("../templates/justfiles/anvil/checks/external-types.just");
const TOOLS: &str = include_str!("../templates/justfiles/anvil/tools.just");
const VERSIONS: &str = include_str!("../templates/justfiles/anvil/versions.just");
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
            targets = @([pscustomobject]@{ name = $env:FAKE_SECOND_PACKAGE_NAME; kind = @('lib') })
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
        justfile.push_str("rust_nightly := \"nightly-test\"\n_anvil_stable_toolchain_args := \"@()\"\n\n");
    }
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
    run_just_from(root, root, arguments, environment)
}

fn run_just_from(root: &Path, current_dir: &Path, arguments: &[&str], environment: &[(&str, &OsStr)]) -> Output {
    let mut command = Command::new("just");
    command
        .arg("--justfile")
        .arg(root.join("Justfile"))
        .args(arguments)
        .current_dir(current_dir);
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

    let output = run_just(
        tmp.path(),
        &["_anvil-test-stable-command"],
        &[
            ("FAKE_CARGO_LOG", args_log.as_os_str()),
            ("FAKE_CARGO_CWD_LOG", cwd_log.as_os_str()),
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
    assert_eq!(
        String::from_utf8_lossy(&directory_alias.stdout).trim(),
        "--package fixture-package@0.1.0"
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
        &[("FAKE_METADATA_EXIT", OsStr::new("23")), ("FAKE_CARGO_LOG", log.as_os_str())],
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
    seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
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
    seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
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

    for output in ["has no lib target", "no library targets found"] {
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
        // These bootstrap/repository-only tools are not managed by Anvil.
        "cargo_workspaces_version",
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
                "anvil-impact",
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
                "anvil-impact",
            ][..],
        ),
    ] {
        let tmp = fixture(&[(recipe_file, contents), ("impact.just", IMPACT)], dependencies);
        seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
        let output = run_just(tmp.path(), &[recipe], &[("FAKE_METADATA_EXIT", OsStr::new("23"))]);
        assert_failed(&output, &format!("{recipe} cargo metadata failure"));

        let malformed = run_just(tmp.path(), &[recipe], &[("FAKE_METADATA_INVALID", OsStr::new("1"))]);
        assert_failed(&malformed, &format!("{recipe} malformed cargo metadata"));
    }
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
    seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
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
    seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
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
