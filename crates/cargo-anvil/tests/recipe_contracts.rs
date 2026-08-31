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
const APRZ: &str = include_str!("../templates/justfiles/anvil/checks/aprz.just");
const MUTANTS_DIFF: &str = include_str!("../templates/justfiles/anvil/checks/mutants-diff.just");
const VERSIONS: &str = include_str!("../templates/justfiles/anvil/versions.just");
const CONTAINER: &str = include_str!("../templates/justfiles/anvil/container.just");
const FAKE_CARGO_PS1: &str = r#"
$joined = $args -join ' '
if ($env:FAKE_CARGO_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_LOG -Value $joined
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
if ($args -contains 'bench-history') {
    # $args inside a function refers to that function's own arguments, so
    # the script's are captured here and passed in explicitly.
    $cbhArgs = $args
    # Writes whatever report the scenario asked for to the path the recipe
    # passed, so the recipe's own parsing and gating are what get exercised.
    function Write-Report([string[]]$all, [string]$flag, [string]$content) {
        $index = [array]::IndexOf($all, $flag)
        if ($index -ge 0 -and $content) {
            $target = $all[$index + 1]
            $parent = Split-Path -Parent $target
            if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
            Set-Content -LiteralPath $target -Value $content -Encoding UTF8
        }
    }
    if ($cbhArgs -contains 'collect') { exit [int]$env:FAKE_CBH_COLLECT_EXIT }
    if ($cbhArgs -contains 'analyze') {
        Write-Report $cbhArgs '--json' $env:FAKE_CBH_FINDINGS
        Write-Report $cbhArgs '--markdown' 'findings'
        Write-Report $cbhArgs '--markdown-summary' 'summary'
        exit [int]$env:FAKE_CBH_ANALYZE_EXIT
    }
    if ($cbhArgs -contains 'list') {
        if ($cbhArgs -contains 'runs') {
            $runs = if ($env:FAKE_CBH_RUNS) {
                $env:FAKE_CBH_RUNS
            } else {
                '{"sets":[{"commits":[{"commit":"8392995a3b94","runs":1,"clean":1,"dirty":0}]}]}'
            }
            Write-Report $cbhArgs '--json' $runs
        } else {
            Write-Report $cbhArgs '--json' $env:FAKE_CBH_BLESSINGS
        }
        exit 0
    }
    if ($cbhArgs -contains 'bless') { exit [int]$env:FAKE_CBH_BLESS_EXIT }
    exit 0
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
    let mut justfile = String::from("set unstable\nset allow-duplicate-recipes\n\n");
    // Focused fixtures need this shared variable, but the real version catalog
    // already defines it and Just rejects duplicate definitions.
    if !imports.iter().any(|(name, _)| *name == "versions.just") {
        justfile.push_str("rust_nightly := \"nightly-test\"\n\n");
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
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );

    let bin = tmp.path().join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    write(&bin.join("cargo.ps1"), FAKE_CARGO_PS1);
    write(&bin.join("git.ps1"), FAKE_GIT);
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
    // A fixture is a scratch workspace, so it must not inherit the impact
    // scoping of whatever invoked the test suite. A CI group job exports
    // `ANVIL_IMPACT=consume` and downloads a cache into the real repository;
    // inherited into a temp directory that has no cache, `anvil-impact` fails
    // hard and takes the recipe under test with it. `ANVIL_INCLUDE_*` is the
    // same hazard one level down: a leg whose scope resolved to `--skip` would
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
            // The other early exit. Impact scoping sets this to `--skip` when a
            // job has no affected packages, and the value is inherited from
            // whatever environment the test runs in -- so on a CI leg that
            // skipped, this test would assert against a recipe that returned
            // before doing anything. Pin it to a scope that runs.
            //
            // The architecture guard is deliberately *not* pinned: Windows
            // re-derives PROCESSOR_ARCHITECTURE for each new process from the
            // process's real architecture, so it cannot be overridden across a
            // spawn. That is why this test returns early on ARM64 above rather
            // than faking its way past the branch.
            ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
            // The recipe depends on `anvil-impact`, which would otherwise
            // invoke cargo-delta against this fixture. The scope this test
            // asserts on is pinned above, so computing an impact set would only
            // add a tool dependency to a contract that does not exercise it.
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
            // Not the architecture -- that is the host's, and real here. This
            // is the *other* early exit, pinned so a skipped impact scope
            // cannot be mistaken for the architecture bail-out.
            ("ANVIL_INCLUDE_AFFECTED", OsStr::new("--package fixture@0.1.0")),
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

const BENCH_HISTORY: &str = include_str!("../templates/justfiles/anvil/checks/bench-history.just");

/// A findings report with one active regression — the state that must gate.
const ACTIVE_REGRESSION: &str = r#"{"notable":true,"findings":[
  {"segments":["emit_alloc","churn"],"kind":"wall_time","direction":"regression",
   "active":true,"relative_delta":0.4966,"confidence":1.0,"commit":"8392995a"}]}"#;

/// The same finding after it recovered: reported, but nothing to act on.
const INACTIVE_REGRESSION: &str = r#"{"notable":true,"findings":[
  {"segments":["emit_alloc","churn"],"kind":"wall_time","direction":"regression",
   "active":false,"relative_delta":0.4966,"confidence":1.0,"commit":"8392995a"}]}"#;

/// An improvement — never a reason to fail.
const IMPROVEMENT: &str = r#"{"notable":true,"findings":[
  {"segments":["emit_alloc","churn"],"kind":"wall_time","direction":"improvement",
   "active":true,"relative_delta":-0.31,"confidence":1.0,"commit":"8392995a"}]}"#;

/// What an empty workspace analyzes to: no runs, no findings.
const NO_FINDINGS: &str = r#"{"notable":false,"findings":[]}"#;

/// The stand-in `git`, covering the commit resolution the blessing
/// reconciliation performs.
const FAKE_GIT: &str = r"
if ($args -contains 'rev-parse') {
    # The recipe resolves a possibly-abbreviated commit before blessing;
    # FAKE_GIT_UNKNOWN_COMMIT makes that resolution fail.
    if ($env:FAKE_GIT_UNKNOWN_COMMIT) { exit 128 }
    Write-Output '8392995a3b94218612437d0b868df2a48029b6ea'
    exit 0
}
exit 0
";

/// Both streams of a recipe run, for assertion messages.
///
/// A recipe that dies before producing output says why on stderr, so a
/// failure message carrying only stdout hides the actual cause.
fn both_streams(output: &Output) -> String {
    format!(
        "\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- status: {:?} ---",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        output.status
    )
}

/// Runs `anvil-bench-history` in a fixture, with the fake cbh returning
/// `findings` and the given extra environment. Gating is CI-only, so the
/// scenarios that assert on the exit code set `ANVIL_BENCH_GATE`.
fn run_bench_history(findings: &str, extra: &[(&str, &OsStr)]) -> (TempDir, Output) {
    let tmp = fixture(
        &[("bench-history.just", BENCH_HISTORY)],
        &[
            "anvil-tool-cargo-bench-history-validate-prereqs",
            "anvil-tool-cargo-bench-history-install installer=\"install\"",
        ],
    );
    let log = tmp.path().join("cargo.log");
    let mut environment: Vec<(&str, &OsStr)> = vec![
        ("FAKE_CBH_FINDINGS", OsStr::new(findings)),
        ("FAKE_CARGO_LOG", log.as_os_str()),
        ("ANVIL_BENCH_GATE", OsStr::new("1")),
    ];
    environment.extend_from_slice(extra);
    let output = run_just(tmp.path(), &["anvil-bench-history"], &environment);
    (tmp, output)
}

fn cargo_calls(root: &Path) -> String {
    std::fs::read_to_string(root.join("cargo.log")).unwrap_or_default()
}

#[test]
fn bench_history_gates_on_active_regressions_only() {
    if !tools_available() {
        return;
    }

    // An active regression is the one state that gates.
    let (tmp, output) = run_bench_history(ACTIVE_REGRESSION, &[]);
    assert_failed(&output, "an active regression");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("emit_alloc/churn"), "names the benchmark:{}", both_streams(&output));
    assert!(text.contains("8392995a"), "names the attributed commit:{}", both_streams(&output));
    let calls = cargo_calls(tmp.path());
    assert!(calls.contains("bench-history collect"), "calls:\n{calls}");
    assert!(calls.contains("bench-history analyze"), "calls:\n{calls}");

    // A recovered regression and an improvement both need no action.
    for (findings, label) in [(INACTIVE_REGRESSION, "inactive"), (IMPROVEMENT, "improvement")] {
        let (_tmp, output) = run_bench_history(findings, &[]);
        assert!(output.status.success(), "{label} finding must not gate:{}", both_streams(&output));
    }

    // A workspace with no benchmarks analyzes to nothing and stays green:
    // adopting the capability must not turn such a repo permanently red.
    let (_tmp, output) = run_bench_history(NO_FINDINGS, &[]);
    assert!(
        output.status.success(),
        "an empty history must be a clean no-op:{}",
        both_streams(&output)
    );
}

#[test]
fn bench_history_reports_without_gating_outside_ci() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("bench-history.just", BENCH_HISTORY)],
        &[
            "anvil-tool-cargo-bench-history-validate-prereqs",
            "anvil-tool-cargo-bench-history-install installer=\"install\"",
        ],
    );
    // No ANVIL_BENCH_GATE, and the CI markers explicitly cleared: a laptop's
    // measurement noise must not fail a pre-release `anvil-full` and invite
    // silencing it with a committed blessing.
    let output = run_just(
        tmp.path(),
        &["anvil-bench-history"],
        &[
            ("FAKE_CBH_FINDINGS", OsStr::new(ACTIVE_REGRESSION)),
            ("CI", OsStr::new("")),
            ("TF_BUILD", OsStr::new("")),
        ],
    );
    assert!(
        output.status.success(),
        "a local run reports but does not gate:{}",
        both_streams(&output)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("emit_alloc/churn"), "still reports the finding:\n{text}");
    assert!(text.contains("ANVIL_BENCH_GATE"), "points at the opt-in:\n{text}");
}

#[test]
fn bench_history_gate_reads_ci_markers_as_boolean_like() {
    if !tools_available() {
        return;
    }
    // PowerShell treats every non-empty string as true, so an exported
    // CI=false would otherwise gate a local run on a shared trend measured
    // on that developer's own hardware.
    let (_tmp, output) = run_bench_history(
        ACTIVE_REGRESSION,
        &[
            ("ANVIL_BENCH_GATE", OsStr::new("")),
            ("CI", OsStr::new("false")),
            ("TF_BUILD", OsStr::new("")),
        ],
    );
    assert!(
        output.status.success(),
        "CI=false must not gate a local run:{}",
        both_streams(&output)
    );

    // A present-but-blank marker is not a marker either.
    let (_tmp, output) = run_bench_history(
        ACTIVE_REGRESSION,
        &[
            ("ANVIL_BENCH_GATE", OsStr::new("")),
            ("CI", OsStr::new("   ")),
            ("TF_BUILD", OsStr::new("")),
        ],
    );
    assert!(
        output.status.success(),
        "a whitespace-only marker must not gate:{}",
        both_streams(&output)
    );

    // Anything else non-empty still gates: the CI side stays fail-closed.
    let (_tmp, output) = run_bench_history(
        ACTIVE_REGRESSION,
        &[
            ("ANVIL_BENCH_GATE", OsStr::new("")),
            ("CI", OsStr::new("true")),
            ("TF_BUILD", OsStr::new("")),
        ],
    );
    assert_failed(&output, "CI=true");
}

#[test]
fn bench_history_propagates_tool_failure() {
    if !tools_available() {
        return;
    }
    // A tool that fails to *run* is not "no regressions".
    let (_tmp, output) = run_bench_history(NO_FINDINGS, &[("FAKE_CBH_COLLECT_EXIT", OsStr::new("3"))]);
    assert_failed(&output, "a failing collect");
}

#[test]
fn bench_history_refuses_a_store_the_wiring_does_not_publish() {
    if !tools_available() {
        return;
    }

    // The wiring announces the one path it restores into and publishes from.
    // A store pointing anywhere else would never persist, so every run would
    // cold-start and analyze to a clean no-op -- reporting green exactly when
    // the history needed to report red has been lost.
    let (_tmp, output) = run_bench_history(
        NO_FINDINGS,
        &[
            ("ANVIL_BENCH_WIRED_STORE", OsStr::new("target/anvil/bench-history")),
            ("ANVIL_BENCH_HISTORY_STORE", OsStr::new("target/somewhere-else")),
        ],
    );
    assert_failed(&output, "a store the wiring does not publish");

    // Agreement is not a desync, however it is spelled: the comparison is on
    // the resolved path, not the literal string.
    let (_tmp, output) = run_bench_history(
        NO_FINDINGS,
        &[
            ("ANVIL_BENCH_WIRED_STORE", OsStr::new("target/anvil/bench-history")),
            ("ANVIL_BENCH_HISTORY_STORE", OsStr::new("target/anvil/../anvil/bench-history")),
        ],
    );
    assert!(
        output.status.success(),
        "the same path spelled differently is not a desync:{}",
        both_streams(&output)
    );

    // Without wiring there is nothing to disagree with: a local run may put
    // its store wherever it likes.
    let (_tmp, output) = run_bench_history(NO_FINDINGS, &[("ANVIL_BENCH_HISTORY_STORE", OsStr::new("target/somewhere-else"))]);
    assert!(
        output.status.success(),
        "an unwired run may choose its own store:{}",
        both_streams(&output)
    );
}

/// Runs the private blessing reconciliation directly, so the prefix-matching
/// boundary is pinned without going through a whole analysis.
fn run_bless(blessings_file: &str, applied: &str) -> (TempDir, Output) {
    let tmp = fixture(
        &[("bench-history.just", BENCH_HISTORY)],
        &[
            "anvil-tool-cargo-bench-history-validate-prereqs",
            "anvil-tool-cargo-bench-history-install installer=\"install\"",
        ],
    );
    write(&tmp.path().join(".config/bench-blessings.toml"), blessings_file);
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["_anvil-bench-history-bless", "store", ".config/bench-blessings.toml"],
        &[("FAKE_CBH_BLESSINGS", OsStr::new(applied)), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    (tmp, output)
}

#[test]
fn bench_history_bless_reconciles_on_exact_prefix_identity() {
    if !tools_available() {
        return;
    }
    let requested = "[[blessing]]\n\
        benchmark = \"emit_alloc\"\n\
        commit = \"8392995a\"\n\
        reason = \"arena allocator tradeoff\"\n";

    // A stored blessing of the *narrower* `emit_alloc/churn` does not cover
    // the requested broader `emit_alloc`. Treating it as already-applied
    // would leave the build red while claiming nothing needed doing.
    let narrower = r#"{"blessings":[{"commit":"8392995a3b94","benchmark":"emit_alloc/churn"}]}"#;
    let (tmp, output) = run_bless(requested, narrower);
    assert!(
        output.status.success(),
        "reconciliation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        cargo_calls(tmp.path()).contains("bench-history bless"),
        "a narrower stored blessing must not satisfy a broader request:{}",
        both_streams(&output)
    );

    // The exact same id already recorded is a no-op, so a scheduled run never
    // re-appends a sidecar that is already in effect. Window-shaped, because
    // `list blessings --all` reports concrete ids and never `prefixes` --
    // fixtures in the HEAD shape would cover a branch that never runs here.
    let exact = r#"{"blessings":[{"commit":"8392995a3b94","benchmark":"emit_alloc"}]}"#;
    let (tmp, output) = run_bless(requested, exact);
    assert!(output.status.success());
    assert!(
        !cargo_calls(tmp.path()).contains("bench-history bless"),
        "an already-applied blessing must not be re-appended:{}",
        both_streams(&output)
    );
}

#[test]
fn bench_history_bless_skips_commits_the_store_has_forgotten() {
    if !tools_available() {
        return;
    }
    let requested = "[[blessing]]\n\
        benchmark = \"emit_alloc\"\n\
        commit = \"8392995a\"\n\
        reason = \"arena allocator tradeoff\"\n";

    // cbh refuses to bless a context commit it holds no run for, and `collect`
    // only ever records the current commit, so a commit lost to a cold start
    // or artifact eviction can never come back. Applying it anyway would fail
    // the recipe before analyze on every run, leaving the group permanently
    // red until a human edited the ledger.
    let tmp = fixture(
        &[("bench-history.just", BENCH_HISTORY)],
        &[
            "anvil-tool-cargo-bench-history-validate-prereqs",
            "anvil-tool-cargo-bench-history-install installer=\"install\"",
        ],
    );
    write(&tmp.path().join(".config/bench-blessings.toml"), requested);
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["_anvil-bench-history-bless", "store", ".config/bench-blessings.toml"],
        &[
            ("FAKE_CBH_BLESSINGS", OsStr::new(r#"{"blessings":[]}"#)),
            ("FAKE_CBH_RUNS", OsStr::new(r#"{"sets":[]}"#)),
            ("FAKE_CARGO_LOG", log.as_os_str()),
        ],
    );
    assert!(
        output.status.success(),
        "a forgotten commit must not fail the recipe:{}",
        both_streams(&output)
    );
    assert!(
        !cargo_calls(tmp.path()).contains("bench-history bless"),
        "nothing to accept at a commit the store has forgotten:{}",
        both_streams(&output)
    );
}

#[test]
fn bench_history_bless_rejects_malformed_entries() {
    if !tools_available() {
        return;
    }
    let applied = r#"{"blessings":[]}"#;

    // A `#` inside a value is content, not a comment: silently truncating a
    // reason citing an issue number would lose exactly what makes the audit
    // trail worth keeping.
    let hashed = "[[blessing]]\n\
        benchmark = \"emit_alloc\"\n\
        commit = \"8392995a\"\n\
        reason = \"accepted in #1234\"\n";
    let (tmp, output) = run_bless(hashed, applied);
    assert!(
        output.status.success(),
        "a # inside a quoted value is content:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("#1234"),
        "the reason must survive intact:{}",
        both_streams(&output)
    );
    assert!(cargo_calls(tmp.path()).contains("bench-history bless"));

    // Anything the subset cannot represent is rejected, not reinterpreted.
    for (body, label) in [
        ("[[blessing]]\nbenchmark = emit_alloc\n", "unquoted value"),
        ("[[blessing]]\nbenchmark = \"a\"\ncommit = \"b\"\n", "missing reason"),
        ("[[other]]\nbenchmark = \"a\"\n", "unexpected table"),
        // A leading `-` would reach cbh as a flag rather than a benchmark id.
        // `--all` in particular has a real meaning there: accept *every*
        // benchmark at the commit, while the log claims one named `--all`.
        (
            "[[blessing]]\nbenchmark = \"--all\"\ncommit = \"8392995a\"\nreason = \"r\"\n",
            "an option-shaped benchmark",
        ),
        (
            "[[blessing]]\nbenchmark = \"emit_alloc\"\ncommit = \"--all\"\nreason = \"r\"\n",
            "an option-shaped commit",
        ),
    ] {
        let (_tmp, output) = run_bless(body, applied);
        assert_failed(&output, label);
    }
}

// ---------------------------------------------------------------------------
// Benchmark-history restore blocks.
//
// These carry the most control flow in the benchmark wiring, and the invariant
// they exist for -- an operational failure must never be mistaken for "no
// history yet" -- is invisible to a `contains` assertion on the emitted YAML.
// Both are therefore extracted from their template and executed against mocked
// transports.
// ---------------------------------------------------------------------------

const GH_BENCH_RESTORE: &str = include_str!("../templates/github/run-group-action.yml");

const ADO_RESTORE: &str = include_str!("../templates/ado/steps/bench-history-restore.yml");

/// Extracts a block scalar (`run: |` / `pwsh: |`) from `yaml`, starting the
/// search at `after` and dedenting the body.
fn block_scalar(yaml: &str, after: &str, key: &str) -> String {
    let start = yaml.find(after).unwrap_or_else(|| panic!("marker '{after}' not found"));
    let rest = &yaml[start..];
    let key_offset = rest.find(key).unwrap_or_else(|| panic!("key '{key}' not found after '{after}'"));
    let key_line_start = rest[..key_offset].rfind('\n').map_or(0, |index| index + 1);
    let indent = key_offset - key_line_start;

    let body = &rest[key_offset + key.len()..];
    let mut lines = Vec::new();
    for line in body.lines().skip(1) {
        let line_indent = line.len() - line.trim_start().len();
        if !line.trim().is_empty() && line_indent <= indent {
            break;
        }
        lines.push(if line.len() > indent + 2 { &line[indent + 2..] } else { "" });
    }
    lines.join("\n")
}

fn git_bash() -> Option<&'static str> {
    let candidate = r"C:\Program Files\Git\bin\bash.exe";
    Path::new(candidate).is_file().then_some(candidate)
}

/// Runs the GitHub restore block with a stubbed `gh`.
///
/// `runs` are the run ids the listing yields, newest first; `artifact_runs`
/// are those the artifacts API reports as carrying the artifact.
fn run_github_restore(
    runs: &str,
    artifact_runs: &str,
    download_exit: &str,
    runs_exit: &str,
    nested: bool,
) -> (TempDir, Output, String, String) {
    let bash = git_bash().expect("git bash checked by caller");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    write(
        &root.join("bin/gh"),
        r#"#!/usr/bin/env bash
if [ "$1" = "run" ] && [ "$2" = "list" ]; then
  if [ -n "$FAKE_GH_RUNS_EXIT" ] && [ "$FAKE_GH_RUNS_EXIT" != "0" ]; then
    echo "gh: HTTP 502 Bad Gateway (api.github.com)" >&2
    exit "$FAKE_GH_RUNS_EXIT"
  fi
  for id in $FAKE_GH_RUNS; do echo "$id"; done
  exit 0
fi
if [ "$1" = "api" ]; then
  for arg in "$@"; do
    case "$arg" in
      */actions/runs/*/artifacts)
        rid="${arg##*/runs/}"
        rid="${rid%%/artifacts}"
        for id in $FAKE_GH_ARTIFACT_RUNS; do
          if [ "$id" = "$rid" ]; then echo "artifact-$id"; exit 0; fi
        done
        ;;
    esac
  done
  exit 0
fi
if [ "$1" = "run" ] && [ "$2" = "download" ]; then
  if [ "${FAKE_GH_DOWNLOAD_EXIT:-0}" != "0" ]; then exit "$FAKE_GH_DOWNLOAD_EXIT"; fi
  # Mimic the payload layout: flat by default (what a single --name gives),
  # nested under the artifact name when the scenario asks for it.
  dest=""
  prev=""
  for arg in "$@"; do
    if [ "$prev" = "--dir" ]; then dest="$arg"; fi
    prev="$arg"
  done
  if [ "$FAKE_GH_NESTED" = "1" ]; then dest="$dest/$ARTIFACT"; fi
  mkdir -p "$dest"
  echo "{}" > "$dest/run.json"
  exit 0
fi
exit 0
"#,
    );

    let script = block_scalar(GH_BENCH_RESTORE, "- name: Restore benchmark history", "run: |");
    write(&root.join("restore.sh"), &script);

    // `gh` is bound as a shell function rather than found on PATH. Git for
    // Windows' bash prepends its own entries to whatever PATH it is handed,
    // so a stub placed on PATH can be shadowed by a real `gh` earlier in the
    // resolution order -- which silently exercises the developer's GitHub CLI
    // instead of the fixture. Function lookup precedes PATH, so this cannot
    // be shadowed.
    write(&root.join("run.sh"), "gh() { bash \"$STUB_GH\" \"$@\"; }\n. ./restore.sh\n");

    let github_env = root.join("env.txt");
    let summary = root.join("summary.md");
    write(&github_env, "");
    write(&summary, "");

    let output = Command::new(bash)
        .arg("run.sh")
        .current_dir(root)
        .env("STUB_GH", root.join("bin/gh").display().to_string().replace('\\', "/"))
        .env("FAKE_GH_RUNS", runs)
        .env("FAKE_GH_ARTIFACT_RUNS", artifact_runs)
        .env("FAKE_GH_DOWNLOAD_EXIT", download_exit)
        .env("FAKE_GH_RUNS_EXIT", runs_exit)
        .env("FAKE_GH_NESTED", if nested { "1" } else { "0" })
        .env("ARTIFACT", "bench-history-linux")
        .env("DEFAULT_BRANCH", "main")
        .env("WORKFLOW", "anvil-scheduled")
        .env("REPO", "owner/repo")
        .env("WINDOW", "30")
        .env("GITHUB_ENV", &github_env)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .expect("bash runs the extracted restore block");

    let env_text = std::fs::read_to_string(&github_env).unwrap_or_default();
    let summary_text = std::fs::read_to_string(&summary).unwrap_or_default();
    (tmp, output, env_text, summary_text)
}

#[test]
fn github_restore_separates_absence_from_failure() {
    if git_bash().is_none() {
        eprintln!("skipping: git bash not installed");
        return;
    }

    // (1) The newest run has no artifact; an older one does. The walk must
    // reach it rather than cold-starting on the first miss.
    let (tmp, output, env, _summary) = run_github_restore("30 20 10", "10", "0", "0", false);
    assert!(
        output.status.success(),
        "walking back should succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(env.contains("ANVIL_BENCH_RESTORE=restored"), "env:\n{env}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("run 10"),
        "should name the run it restored from:{}",
        both_streams(&output)
    );
    // The payload has to land *in* the store, not one level under it. A
    // nested copy loses the history silently: every run then cold-starts
    // and analyzes to a clean no-op.
    let store = tmp.path().join("target/anvil/bench-history/run.json");
    assert!(store.is_file(), "the restored payload must land in the store root");

    // `gh run download` extracts a single `--name` straight into `--dir`,
    // but nests one directory per artifact when several are requested.
    // Tolerate both, so a change in that behaviour cannot silently empty
    // the store.
    let (tmp_nested, output, env, _summary) = run_github_restore("30 20 10", "10", "0", "0", true);
    assert!(output.status.success(), "nested layout:{}", both_streams(&output));
    assert!(env.contains("ANVIL_BENCH_RESTORE=restored"), "env:\n{env}");
    assert!(
        tmp_nested.path().join("target/anvil/bench-history/run.json").is_file(),
        "a nested artifact directory must be lifted into the store root"
    );

    // (2) No run in the window carries it: a genuine cold start, and it must
    // be visible on the summary rather than only in the log.
    let (_tmp, output, env, summary) = run_github_restore("30 20 10", "", "0", "0", false);
    assert!(output.status.success());
    assert!(env.contains("ANVIL_BENCH_RESTORE=cold-start"), "env:\n{env}");
    assert!(summary.contains("cold start"), "summary:\n{summary}");

    // (3) The artifact exists but the download fails. This is the branch whose
    // silent reintroduction re-creates the history-loss bug: it must fail and
    // leave no publishable restore state.
    let (_tmp, output, env, _summary) = run_github_restore("30 20 10", "30", "1", "0", false);
    assert_failed(&output, "a failing download");
    assert!(
        !env.contains("ANVIL_BENCH_RESTORE"),
        "a failed restore must not mark a publishable state:\n{env}"
    );

    // (4) Listing the runs fails outright. `set -e` does not apply to a
    // command substitution consumed as a `for` word list, so this branch
    // would otherwise fall through to a cold start and publish a truncated
    // store over the chain while reporting green.
    let (_tmp, output, env, _summary) = run_github_restore("30 20 10", "10", "0", "1", false);
    assert_failed(&output, "a failing run listing");
    assert!(!env.contains("ANVIL_BENCH_RESTORE"), "a failed listing is not a cold start:\n{env}");
}

/// Runs the ADO restore block with mocked REST/download/extract cmdlets.
///
/// `artifact_builds` are the build ids whose artifact query succeeds; every
/// other build answers 404 (absence). `failure` injects an operational fault:
/// "query" (non-404 status), "download", or "extract".
fn run_ado_restore(builds: &str, artifact_builds: &str, failure: &str, no_default_branch: bool) -> (TempDir, Output, String) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let body = block_scalar(ADO_RESTORE, "steps:", "pwsh: |")
        .replace("${{ parameters.artifact }}", "bench-history-linux")
        .replace("${{ parameters.path }}", "store")
        .replace("${{ parameters.window }}", "30");

    // Function definitions shadow cmdlets of the same name, so the block runs
    // unchanged against these stand-ins.
    let prelude = r#"
# An exception whose Response.StatusCode.value__ the block can read, which is
# how it tells absence (404) from an operational failure (anything else).
class FakeHttpException : System.Exception {
    [object]$Response
    FakeHttpException([int]$status) : base("http $status") {
        $this.Response = [pscustomobject]@{ StatusCode = [pscustomobject]@{ value__ = $status } }
    }
}
function Invoke-RestMethod {
    param([string]$Uri, $Headers)
    if ($Uri -like '*/definitions/*') {
        # The pipeline definition carries the default branch the series is
        # keyed on. FAKE_ADO_NO_DEFAULT_BRANCH withholds it.
        if ($env:FAKE_ADO_NO_DEFAULT_BRANCH) {
            return [pscustomobject]@{ repository = [pscustomobject]@{ defaultBranch = $null } }
        }
        return [pscustomobject]@{ repository = [pscustomobject]@{ defaultBranch = 'refs/heads/main' } }
    }
    if ($Uri -notlike '*/artifacts*') {
        $ids = $env:FAKE_ADO_BUILDS -split ' ' | Where-Object { $_ }
        return [pscustomobject]@{ value = @($ids | ForEach-Object { [pscustomobject]@{ id = $_ } }) }
    }
    $buildId = ($Uri -replace '.*/builds/', '') -replace '/artifacts.*', ''
    $carries = ($env:FAKE_ADO_ARTIFACT_BUILDS -split ' ') -contains $buildId
    if (-not $carries) { throw [FakeHttpException]::new(404) }
    if ($env:FAKE_ADO_FAILURE -eq 'query') { throw [FakeHttpException]::new(500) }
    return [pscustomobject]@{ resource = [pscustomobject]@{ downloadUrl = "https://example/$buildId" } }
}
function Invoke-WebRequest {
    param([string]$Uri, $Headers, [string]$OutFile)
    if ($env:FAKE_ADO_FAILURE -eq 'download') { throw 'download failed' }
    Set-Content -LiteralPath $OutFile -Value 'zip'
}
function Expand-Archive {
    param([string]$LiteralPath, [string]$DestinationPath, [switch]$Force)
    if ($env:FAKE_ADO_FAILURE -eq 'extract') { throw 'corrupt archive' }
    New-Item -ItemType Directory -Force -Path $DestinationPath | Out-Null
    Set-Content -LiteralPath (Join-Path $DestinationPath 'run.json') -Value '{}'
}
"#;

    write(&root.join("restore.ps1"), &format!("{prelude}\n{body}"));

    let output = Command::new("pwsh")
        .args(["-NoProfile", "-File", "restore.ps1"])
        .current_dir(root)
        .env("FAKE_ADO_BUILDS", builds)
        .env("FAKE_ADO_ARTIFACT_BUILDS", artifact_builds)
        .env("FAKE_ADO_FAILURE", failure)
        .env("FAKE_ADO_NO_DEFAULT_BRANCH", if no_default_branch { "1" } else { "" })
        .env("SYSTEM_ACCESSTOKEN", "token")
        .env("SYSTEM_COLLECTIONURI", "https://example/")
        .env("SYSTEM_TEAMPROJECTID", "project")
        .env("SYSTEM_DEFINITIONID", "7")
        .env("BUILD_SOURCEBRANCH", "refs/heads/main")
        .env("BUILD_BUILDID", "999")
        .output()
        .expect("pwsh runs the extracted restore block");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    (tmp, output, stdout)
}

#[test]
fn ado_restore_separates_absence_from_failure() {
    if Command::new("pwsh").arg("--version").output().is_err() {
        eprintln!("skipping: pwsh not installed");
        return;
    }

    // A 404 on the newest build walks back to an older one that has it.
    let (_tmp, output, stdout) = run_ado_restore("30 20 10", "10", "", false);
    assert!(
        output.status.success(),
        "walking back should succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("restoring"), "stdout:\n{stdout}");
    assert!(stdout.contains("ANVIL_BENCH_RESTORE]restored"), "stdout:\n{stdout}");

    // Nothing in the window carries it: a genuine cold start.
    let (_tmp, output, stdout) = run_ado_restore("30 20 10", "", "", false);
    assert!(output.status.success());
    assert!(stdout.contains("ANVIL_BENCH_RESTORE]cold-start"), "stdout:\n{stdout}");

    // Every operational fault must fail without marking a publishable state,
    // which is what the guarded publish depends on to avoid overwriting a
    // good chain with a truncated store.
    for failure in ["query", "download", "extract"] {
        let (_tmp, output, stdout) = run_ado_restore("30 20 10", "30", failure, false);
        assert_failed(&output, failure);
        assert!(
            !stdout.contains("ANVIL_BENCH_RESTORE]"),
            "{failure} must not set a restore state:\n{stdout}"
        );
    }

    // The series is keyed on the default branch. If that cannot be resolved
    // the leg does not know which chain it is extending, and silently keying
    // on the branch this build ran on would fork a one-sample history and
    // report it as clean.
    let (_tmp, output, stdout) = run_ado_restore("30 20 10", "10", "", true);
    assert_failed(&output, "an unresolvable default branch");
    assert!(
        !stdout.contains("ANVIL_BENCH_RESTORE]"),
        "an unidentified series must not set a restore state:\n{stdout}"
    );
}

#[test]
fn bench_history_bless_listings_are_process_scoped() {
    if !tools_available() {
        return;
    }
    // The blessing reconciliation writes `list blessings` output to a temp
    // file before reading it back. A fixed name lets two jobs sharing a
    // machine -- matrix legs on a self-hosted agent, concurrent local runs --
    // read each other's listing. Reverting the process scoping must trip
    // this deterministically rather than by chance under parallel runs.
    let shared_temp = TempDir::new().unwrap();

    // Each invocation asks for a different benchmark and is told a different
    // already-applied set, so consuming the other's listing is observable:
    // each would then think its blessing was already in effect and skip it.
    let cases = [
        ("alpha", r#"{"blessings":[{"commit":"8392995a3b94","prefixes":["beta"]}]}"#),
        ("beta", r#"{"blessings":[{"commit":"8392995a3b94","prefixes":["alpha"]}]}"#),
    ];

    for (benchmark, applied) in cases {
        let file = format!("[[blessing]]\nbenchmark = \"{benchmark}\"\ncommit = \"8392995a\"\nreason = \"deliberate\"\n");
        let tmp = fixture(
            &[("bench-history.just", BENCH_HISTORY)],
            &[
                "anvil-tool-cargo-bench-history-validate-prereqs",
                "anvil-tool-cargo-bench-history-install installer=\"install\"",
            ],
        );
        write(&tmp.path().join(".config/bench-blessings.toml"), &file);
        let log = tmp.path().join("cargo.log");
        let output = run_just(
            tmp.path(),
            &["_anvil-bench-history-bless", "store", ".config/bench-blessings.toml"],
            &[
                ("FAKE_CBH_BLESSINGS", OsStr::new(applied)),
                ("FAKE_CARGO_LOG", log.as_os_str()),
                // Both invocations share one temp directory, which is what a
                // fixed listing filename would collide in.
                ("RUNNER_TEMP", shared_temp.path().as_os_str()),
            ],
        );
        assert!(
            output.status.success(),
            "reconciliation failed for {benchmark}:{}",
            both_streams(&output)
        );
        // Its own listing says a *different* benchmark is blessed, so this
        // one must still be applied.
        assert!(
            cargo_calls(tmp.path()).contains("bench-history bless"),
            "{benchmark} must be blessed from its own listing:{}",
            both_streams(&output)
        );
    }

    // One listing file per process, so the two runs never shared one.
    let listings: Vec<_> = std::fs::read_dir(shared_temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("anvil-bench-blessings"))
        .collect();
    assert_eq!(listings.len(), 2, "each invocation must write its own listing, got: {listings:?}");
}
