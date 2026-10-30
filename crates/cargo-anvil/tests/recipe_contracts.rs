// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panic-on-failure idioms are appropriate in tests"
)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use cargo_anvil::test_support::{Cli, run_update};
use serde_json::from_str;
use tempfile::TempDir;

const HELPERS: &str = include_str!("../templates/justfiles/anvil/helpers.just");
const IMPACT: &str = include_str!("../templates/justfiles/anvil/impact.just");
const BUILD: &str = include_str!("../templates/justfiles/anvil/dev/build.just");
const BOLERO: &str = include_str!("../templates/justfiles/anvil/checks/bolero.just");
const CHECK_ALL_TARGETS: &str = include_str!("../templates/justfiles/anvil/checks/check-all-targets.just");
const DOC_BUILD: &str = include_str!("../templates/justfiles/anvil/checks/doc-build.just");
const DOC_TEST: &str = include_str!("../templates/justfiles/anvil/checks/doc-test.just");
const EXAMPLES: &str = include_str!("../templates/justfiles/anvil/checks/examples.just");
const FMT: &str = include_str!("../templates/justfiles/anvil/checks/fmt.just");
const LLVM_COV: &str = include_str!("../templates/justfiles/anvil/checks/llvm-cov.just");
const LOOM: &str = include_str!("../templates/justfiles/anvil/checks/loom.just");
const MIRI: &str = include_str!("../templates/justfiles/anvil/checks/miri.just");
const MIRI_RACE_COVERAGE: &str = include_str!("../templates/justfiles/anvil/checks/miri-race-coverage.just");
const MIRI_STRICT_PROVENANCE: &str = include_str!("../templates/justfiles/anvil/checks/miri-strict-provenance.just");
const MIRI_TREE_BORROWS: &str = include_str!("../templates/justfiles/anvil/checks/miri-tree-borrows.just");
const MSRV_TEST: &str = include_str!("../templates/justfiles/anvil/checks/msrv-test.just");
const README: &str = include_str!("../templates/justfiles/anvil/checks/readme-check.just");
const SEMVER: &str = include_str!("../templates/justfiles/anvil/checks/semver-check.just");
const EXTERNAL_TYPES: &str = include_str!("../templates/justfiles/anvil/checks/external-types.just");
const TOOLS: &str = include_str!("../templates/justfiles/anvil/tools.just");
const APRZ: &str = include_str!("../templates/justfiles/anvil/checks/aprz.just");
const MUTANTS_DIFF: &str = include_str!("../templates/justfiles/anvil/checks/mutants-diff.just");
const MUTANTS_FULL: &str = include_str!("../templates/justfiles/anvil/checks/mutants-full.just");
const VERSIONS: &str = include_str!("../templates/justfiles/anvil/versions.just");
const REGENERATE_WORKFLOW: &str = include_str!("../../../.github/workflows/regenerate-check.yml");
const CONTAINER: &str = include_str!("../templates/justfiles/anvil/container.just");
const CONTAINER_SETUP_REGION: &str = include_str!("../templates/anvil/container/Dockerfile.setup.region");
const CONTAINER_DOCKERIGNORE: &str = include_str!("../templates/anvil/container/Dockerfile.dockerignore");
// Any nonzero value works; naming it prevents tests from implying an external exit-code contract.
const ARBITRARY_FAILURE_EXIT: &str = "23";

/// Serializes PowerShell interpreter startup within this libtest process.
///
/// PowerShell#26940 can corrupt assembly-name parsing when constrained runners
/// start many interpreters concurrently. Individual recipes may still exercise
/// their own intentional child-process parallelism while this guard prevents
/// unrelated tests from starting competing PowerShell hosts.
static POWERSHELL_PROCESS_LOCK: Mutex<()> = Mutex::new(());

fn powershell_process_lock() -> MutexGuard<'static, ()> {
    POWERSHELL_PROCESS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// The Miri fixture mirrors the production protocol rather than executing Rust:
// fake Cargo supplies workspace metadata and compiler-artifact JSON, fake rustc
// identifies the pinned toolchain sysroot, and cargo-miri under that sysroot
// records the environment and working directory restored by the direct runner.
#[test]
fn regeneration_check_runs_on_every_pull_request() {
    assert!(REGENERATE_WORKFLOW.contains("pull_request: {}"));
    assert!(
        !REGENERATE_WORKFLOW.contains("\n    paths:"),
        "the dogfood drift gate must not be limited by changed paths"
    );
}

#[test]
fn resolver_hook_executes_with_legacy_and_engine_context_signatures() {
    if !tools_available() {
        return;
    }
    let _powershell = powershell_process_lock();
    let start = CONTAINER
        .find("                    $resolveArgs = @{}")
        .expect("resolver context block");
    let end = CONTAINER[start..]
        .find("                    $resolved = @(Anvil-ResolveImage $image @resolveArgs")
        .map(|offset| start + offset)
        .expect("resolver invocation");
    let end = CONTAINER[end..].find('\n').map_or(CONTAINER.len(), |offset| end + offset);
    let invocation = &CONTAINER[start..end];

    for (hook, expected) in [
        (
            "function Anvil-ResolveImage { param([string]$Image) \"$Image|legacy\" }",
            "input:tag|legacy",
        ),
        (
            "function Anvil-ResolveImage { param([string]$Image, [string]$Engine) \"$Image|$($PSBoundParameters.ContainsKey('Engine'))\" }",
            "input:tag|False",
        ),
        (
            "function Anvil-ResolveImage { param([string]$Image, [string[]]$EnginePrefix) \"$Image|$($PSBoundParameters.ContainsKey('EnginePrefix'))\" }",
            "input:tag|False",
        ),
        (
            "function Anvil-ResolveImage { param([string]$Image, [string]$Engine, [string[]]$EnginePrefix) \"$Image|$Engine|$($EnginePrefix -join ',')\" }",
            "input:tag|wsl.exe|--exec,docker",
        ),
    ] {
        let script = format!(
            "$ErrorActionPreference = 'Stop'\n\
             $image = 'input:tag'\n\
             $engineExe = 'wsl.exe'\n\
             $enginePrefix = @('--exec', 'docker')\n\
             {hook}\n\
             {invocation}\n\
             Write-Output $resolved\n"
        );
        let output = Command::new("pwsh")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .expect("pwsh was checked by tools_available");
        assert!(
            output.status.success(),
            "resolver fixture failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    }
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

#[test]
fn developer_options_are_explicit_and_cloud_defaults_stay_non_interactive() {
    assert!(BUILD.contains("[arg(\"package\", long"));
    assert!(BUILD.contains("anvil-build package=\"\" profile=\"\""));
    assert!(BUILD.contains("--all-features --all-targets --locked"));
    assert!(DOC_BUILD.contains("[arg(\"open\", long, value=\"true\")]"));
    assert!(EXAMPLES.contains(".'no-run'"));
    assert!(FMT.contains("[arg(\"fix\", long, value=\"true\")]"));
    assert!(MIRI.contains("[arg(\"test\", long"));
    assert!(MIRI.contains("[arg(\"example\", long"));
    assert!(README.contains("[arg(\"fix\", long, value=\"true\")]"));
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
if ($env:FAKE_CARGO_AUTO_INSTALL_LOG) {
    Add-Content -LiteralPath $env:FAKE_CARGO_AUTO_INSTALL_LOG -Value $env:RUSTUP_AUTO_INSTALL
}
if ($args -contains 'each') {
    $featureExit = if ($args -contains '--no-default-features') {
        $env:FAKE_EACH_NO_DEFAULT_EXIT
    } else {
        $env:FAKE_EACH_DEFAULT_EXIT
    }
    if ($featureExit) { exit [int]$featureExit }
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
    $packageId = if ($env:FAKE_PACKAGE_ID) { $env:FAKE_PACKAGE_ID } else { "$packageName 0.1.0" }
    $libName = if ($env:FAKE_LIB_NAME) { $env:FAKE_LIB_NAME } else { 'fixture' }
    $manifestPath = if ($env:FAKE_PACKAGE_DIR_LEAF) {
        [System.IO.Path]::Combine($root, $env:FAKE_PACKAGE_DIR_LEAF, 'Cargo.toml')
    } else {
        [System.IO.Path]::Combine($root, 'Cargo.toml')
    }
    $packageMetadata = [pscustomobject]@{
        'coverage-gate' = [pscustomobject]@{ 'min-lines-percent' = 0 }
    }
    if ($env:FAKE_MIRI_EXCLUDE) {
        $packageMetadata | Add-Member -NotePropertyName anvil -NotePropertyValue (
            [pscustomobject]@{ miri = [pscustomobject]@{ exclude = $true } }
        )
    }
    $packages = @(
        [pscustomobject]@{
            name = $packageName
            version = '0.1.0'
            id = $packageId
            manifest_path = $manifestPath
            targets = @([pscustomobject]@{
                name = $libName
                kind = if ($env:FAKE_FIRST_RLIB) { @('rlib') } else { @('lib') }
                doctest = -not [bool]$env:FAKE_FIRST_DOCTEST_FALSE
            })
            publish = if ($env:FAKE_PUBLISH_FALSE) {
                # Preserve the empty array through expression output so JSON emits [] rather than null.
                Write-Output -NoEnumerate @()
            } else {
                $null
            }
            metadata = $packageMetadata
        }
    )
    if ($env:FAKE_SECOND_PACKAGE_NAME) {
        $secondDirLeaf = if ($env:FAKE_SECOND_PACKAGE_DIR_LEAF) {
            $env:FAKE_SECOND_PACKAGE_DIR_LEAF
        } else {
            $env:FAKE_PACKAGE_DIR_LEAF
        }
        $secondMetadata = [pscustomobject]@{}
        if ($env:FAKE_SECOND_MIRI_EXCLUDE) {
            $secondMetadata | Add-Member -NotePropertyName anvil -NotePropertyValue (
                [pscustomobject]@{ miri = [pscustomobject]@{ exclude = $true } }
            )
        }
        $secondPackageId = if ($env:FAKE_SECOND_PACKAGE_ID) {
            $env:FAKE_SECOND_PACKAGE_ID
        } else {
            "$($env:FAKE_SECOND_PACKAGE_NAME) 0.1.0"
        }
        $packages += [pscustomobject]@{
            name = $env:FAKE_SECOND_PACKAGE_NAME
            version = '0.1.0'
            id = $secondPackageId
            manifest_path = [System.IO.Path]::Combine($root, 'nested', $secondDirLeaf, 'Cargo.toml')
            targets = @([pscustomobject]@{
                name = $env:FAKE_SECOND_PACKAGE_NAME
                kind = if ($env:FAKE_SECOND_BIN_ONLY) { @('bin') } else { @('lib') }
                doctest = -not [bool]$env:FAKE_SECOND_DOCTEST_FALSE
            })
            publish = $null
            metadata = $secondMetadata
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
                doctest = -not [bool]$env:FAKE_THIRD_DOCTEST_FALSE
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
            targets = @([pscustomobject]@{
                name = $env:FAKE_NON_MEMBER_PACKAGE_NAME
                kind = @('lib')
                doctest = -not [bool]$env:FAKE_NON_MEMBER_DOCTEST_FALSE
            })
            metadata = [pscustomobject]@{}
        }
    }
    $metadata | ConvertTo-Json -Depth 8 -Compress
    exit 0
}
if ($args -contains 'miri') {
    if ($args -contains 'setup') {
        Write-Output ([System.IO.Path]::Combine($env:FAKE_WORKSPACE_ROOT, 'fake-miri-sysroot'))
        exit [int]$env:FAKE_MIRI_SETUP_EXIT
    }
    if ($args -contains '--no-run') {
        if ($env:FAKE_MIRI_INVALID_JSON) {
            Write-Output '{invalid cargo json'
            exit 0
        }
        $artifactRoot = [System.IO.Path]::Combine($env:FAKE_WORKSPACE_ROOT, 'fake artifacts')
        [System.IO.Directory]::CreateDirectory($artifactRoot) | Out-Null
        $definitions = @()
        if ($env:FAKE_MIRI_ARTIFACTS) {
            $definitions = @($env:FAKE_MIRI_ARTIFACTS | ConvertFrom-Json)
        }
        foreach ($definition in $definitions) {
            $artifactDirectory = if ($definition.artifact_dir) {
                [System.IO.Path]::Combine($artifactRoot, [string]$definition.artifact_dir)
            } else {
                $artifactRoot
            }
            [System.IO.Directory]::CreateDirectory($artifactDirectory) | Out-Null
            $path = [System.IO.Path]::Combine($artifactDirectory, [string]$definition.name)
            Set-Content -LiteralPath $path -Value '{}'
            [pscustomobject]@{
                reason = 'compiler-artifact'
                package_id = [string]$definition.package_id
                executable = $path
                profile = [pscustomobject]@{ test = [bool]$definition.test }
                target = [pscustomobject]@{
                    name = if ($definition.target_name) {
                        [string]$definition.target_name
                    } else {
                        [string]$definition.name
                    }
                    kind = @(if ($definition.target_kind) {
                        [string]$definition.target_kind
                    } else {
                        'test'
                    })
                }
            } | ConvertTo-Json -Depth 4 -Compress
            if ($definition.duplicate) {
                [pscustomobject]@{
                    reason = 'compiler-artifact'
                    package_id = [string]$definition.package_id
                    executable = $path
                    profile = [pscustomobject]@{ test = [bool]$definition.test }
                    target = [pscustomobject]@{
                        name = if ($definition.target_name) {
                            [string]$definition.target_name
                        } else {
                            [string]$definition.name
                        }
                        kind = @(if ($definition.target_kind) {
                            [string]$definition.target_kind
                        } else {
                            'test'
                        })
                    }
                } | ConvertTo-Json -Depth 4 -Compress
            }
        }
        [pscustomobject]@{
            reason = 'build-finished'
            success = ([int]$env:FAKE_MIRI_BUILD_EXIT -eq 0)
        } | ConvertTo-Json -Compress
        exit [int]$env:FAKE_MIRI_BUILD_EXIT
    }
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
if ($args -contains '--in-diff' -and $env:FAKE_MUTANTS_DIFF_CAPTURE) {
    $diffPath = $args[([array]::IndexOf($args, '--in-diff') + 1)]
    Copy-Item -LiteralPath $diffPath -Destination $env:FAKE_MUTANTS_DIFF_CAPTURE
}
if ($args -contains 'install' -and $args -contains '--list') {
    if ($env:FAKE_INSTALL_LIST_OUTPUT) { Write-Output $env:FAKE_INSTALL_LIST_OUTPUT }
    exit [int]$env:FAKE_INSTALL_LIST_EXIT
}
if ($args -contains 'install' -and $args -contains '--version') {
    exit [int]$env:FAKE_INSTALL_EXIT
}
exit [int]$env:FAKE_CARGO_DEFAULT_EXIT
"#;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Seed the impact cache that scoped check recipes read via
/// `_anvil-impact-include`, standing in for a completed `anvil-impact` run.
/// Outside consume mode, a missing cache falls back to the tier default
/// (`--workspace` for the affected tier), so tests that exercise a scoped run
/// must plant the include file the recipe consumes.
fn seed_include(root: &Path, tier: &str, spec: &str) {
    write(&root.join(format!("target/anvil/impact/include_{tier}.txt")), spec);
}

fn seed_doctest_packages(root: &Path, specs: &[&str]) {
    write(&root.join("target/anvil/impact/doctest_packages.txt"), &specs.join("\n"));
}

fn assert_exact_doc_test_commands(commands: &str, package_args: &str) {
    let actual = commands
        .lines()
        .filter(|line| line.contains("test --doc"))
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        format!("test --doc {package_args} --all-features --locked"),
        format!("test --doc {package_args} --locked"),
    ];
    assert_eq!(
        actual,
        expected.iter().map(String::as_str).collect(),
        "doctest commands must be exactly the locked all-features/default-features pair:\n{commands}"
    );
}

fn tools_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let _powershell = powershell_process_lock();
        Command::new("just").arg("--version").output().is_ok() && Command::new("pwsh").arg("--version").output().is_ok()
    })
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
        justfile.push_str("cargo_delta_version := \"0.0.0-test\"\n\n");
        justfile.push_str("_anvil_stable_toolchain_args := \"@()\"\n\n");
    }
    for (name, contents) in imports {
        write(&tmp.path().join(name), contents);
        writeln!(justfile, "import '{name}'").unwrap();
    }
    justfile.push('\n');
    if imports.iter().any(|(name, _)| *name == "miri.just") {
        justfile.push_str(
            r#"
[script("pwsh", "-NoProfile")]
_anvil-impact-exclude tier:
    Write-Output $(if ($env:FAKE_INCLUDE) { $env:FAKE_INCLUDE } else { '--workspace' })

"#,
        );
    }
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
    write(
        &bin.join("rustc.ps1"),
        r"
if ($args -contains 'sysroot') {
    if ($env:FAKE_RUSTC_PREAMBLE) {
        Write-Output $env:FAKE_RUSTC_PREAMBLE
    }
    Write-Output ([System.IO.Path]::Combine($env:FAKE_WORKSPACE_ROOT, 'fake-toolchain'))
    exit 0
}
exit 1
",
    );
    let fake_toolchain_bin = tmp.path().join("fake-toolchain/bin");
    fs::create_dir_all(&fake_toolchain_bin).unwrap();
    write(
        &fake_toolchain_bin.join("cargo-miri.ps1"),
        r#"
$runnerArgs = @($args)
if ($runnerArgs.Count -lt 2 -or $runnerArgs[0] -ne 'runner') {
    Write-Error "unexpected cargo-miri invocation: $($runnerArgs -join ' ')"
    exit 97
}
$artifact = [System.IO.Path]::GetFileName([string]$runnerArgs[1])
$binaryArgs = @($runnerArgs | Select-Object -Skip 2)
$prefix = "$($env:FAKE_MIRI_RUN_LOG).$artifact"
($binaryArgs -join "`n") | Set-Content -LiteralPath "$prefix.args"
(Get-Location).Path | Set-Content -LiteralPath "$prefix.cwd"
$env:MIRI_SYSROOT | Set-Content -LiteralPath "$prefix.sysroot"
$env:MIRI_BE_RUSTC | Set-Content -LiteralPath "$prefix.miri-be-rustc"
$env:MIRIFLAGS | Set-Content -LiteralPath "$prefix.miriflags"
$env:RUSTFLAGS | Set-Content -LiteralPath "$prefix.rustflags"
if ($env:FAKE_MIRI_BARRIER_DIR) {
    $ready = Join-Path $env:FAKE_MIRI_BARRIER_DIR "$artifact.ready"
    $release = Join-Path $env:FAKE_MIRI_BARRIER_DIR 'release'
    Set-Content -LiteralPath $ready -Value 'ready'

    $watchdog = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath $release)) {
        if ($watchdog.Elapsed.TotalSeconds -ge 60) {
            Write-Error "timed out at Miri concurrency barrier: $artifact"
            exit 98
        }
        Start-Sleep -Milliseconds 10
    }
}
Write-Output "miri output: $artifact"
if ($artifact -like '*fail*') { exit 9 }
exit 0
"#,
    );
    tmp
}

struct RecipeDeclaration<'a> {
    visibility: Vec<&'a str>,
    signature: String,
}

fn declared_anvil_recipes(catalog: &cargo_anvil::Catalog) -> BTreeMap<&str, RecipeDeclaration<'_>> {
    let mut recipes = BTreeMap::new();
    for artifact in catalog.artifacts() {
        let cargo_anvil::Artifact::OwnedFile(spec) = artifact else {
            continue;
        };
        if !spec.path.starts_with("justfiles/anvil/") {
            continue;
        }
        let lines: Vec<_> = spec.body.lines().collect();
        for (index, line) in lines.iter().enumerate().filter(|(_, line)| line.starts_with("anvil-")) {
            let name = line.split([' ', ':']).next().unwrap();
            let visibility = lines[..index]
                .iter()
                .rev()
                .take_while(|line| line.starts_with('['))
                .copied()
                .filter(|line| *line == "[private]" || line.starts_with("[group("))
                .collect();
            let mut end = index;
            while lines[end].ends_with('\\') {
                end += 1;
            }
            let signature = lines[index..=end].join("\n");
            assert!(
                recipes.insert(name, RecipeDeclaration { visibility, signature }).is_none(),
                "duplicate recipe declaration: {name}"
            );
        }
    }
    recipes
}

#[test]
fn generated_recipe_visibility_exposes_only_grouped_public_entry_points() {
    let catalog = cargo_anvil::Catalog::anvil();
    let recipes = declared_anvil_recipes(&catalog);
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for (name, recipe) in &recipes {
        if !recipe.visibility.contains(&"[private]") {
            groups.entry(&recipe.visibility).or_default().push(*name);
        }
    }
    let surface = groups
        .iter()
        .map(|(attributes, names)| format!("{}\n{}", attributes.join("\n"), names.join("\n")))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        surface.lines().collect::<Vec<_>>(),
        include_str!("fixtures/recipe-public-surface.txt").lines().collect::<Vec<_>>()
    );

    let aliases: Vec<_> = catalog
        .artifacts()
        .iter()
        .filter_map(|artifact| match artifact {
            cargo_anvil::Artifact::OwnedFile(spec) if spec.path.starts_with("justfiles/anvil/") => Some(&spec.body),
            _ => None,
        })
        .flat_map(|body| body.lines().filter(|line| line.starts_with("alias ")))
        .collect();
    assert_eq!(aliases, ["alias anvil := anvil-pr"]);
    assert_eq!(recipes["anvil-pr"].visibility, ["[group(\"anvil\")]"]);
}

#[test]
fn private_prerequisite_recipes_preserve_their_definitions() {
    let catalog = cargo_anvil::Catalog::anvil();
    let recipes = declared_anvil_recipes(&catalog);
    let mut signatures = Vec::new();
    for recipe in [
        "anvil-build-setup",
        "anvil-clippy-setup",
        "anvil-pr-fast-setup",
        "anvil-pr-setup",
        "anvil-tool-cargo-audit-install",
        "anvil-component-default-clippy-install",
        "anvil-toolchain-stable-install",
        "anvil-clippy-validate-prereqs",
        "anvil-pr-fast-validate-prereqs",
        "anvil-pr-validate-prereqs",
        "anvil-tool-cargo-audit-validate-prereqs",
    ] {
        let declaration = &recipes[recipe];
        assert_eq!(declaration.visibility, ["[private]"], "{recipe}");
        signatures.push(declaration.signature.as_str());
    }
    assert_eq!(
        signatures.join("\n\n").lines().collect::<Vec<_>>(),
        include_str!("fixtures/recipe-private-definitions.txt").lines().collect::<Vec<_>>()
    );
}

/// `anvil-container-tag` resolves the declared root MSRV through
/// `_anvil-resolve-stable`, which the container fixtures do not import. Stubbing
/// it keeps them focused on the digest, and lets a case vary the value the tag
/// frames without standing up a manifest.
///
/// The stub rejects any other action, so a caller that asks for the wrong one --
/// `msrv`, say, which answers with the *mapped* toolchain rather than the
/// declared version -- fails here instead of silently digesting a value the
/// image never installs.
fn stub_msrv_resolver(root: &Path) {
    let justfile_path = root.join("Justfile");
    let mut justfile = fs::read_to_string(&justfile_path).unwrap();
    justfile.push_str(
        "\n[script(\"pwsh\", \"-NoProfile\")]\n\
         _anvil-resolve-stable action:\n\
         \x20   if ('{{action}}' -ne 'root-msrv') { Write-Error \"stub: expected action 'root-msrv', got '{{action}}'\"; exit 2 }\n\
         \x20   if ($env:FAKE_ROOT_MSRV) { Write-Output $env:FAKE_ROOT_MSRV } else { Write-Output 'none' }\n",
    );
    write(&justfile_path, &justfile);
}

fn path_with_fake_bin(root: &Path) -> OsString {
    let mut paths = vec![root.join("fake-bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::join_paths(paths).unwrap()
}

fn just_command(root: &Path, arguments: &[&str], environment: &[(&str, &OsStr)]) -> Command {
    let mut command = Command::new("just");
    command
        .arg("--justfile")
        .arg(root.join("Justfile"))
        .args(arguments)
        .current_dir(root);
    command.env("PATH", path_with_fake_bin(root));
    command.env("FAKE_WORKSPACE_ROOT", root);
    // A fixture is a scratch workspace, so it must not inherit impact scoping
    // or output-backend markers from the process running the test suite. Tests
    // that exercise those contracts pass the relevant values explicitly.
    command.env_remove("ANVIL_IMPACT");
    command.env_remove("ANVIL_IMPACT_INPUT_DIR");
    command.env_remove("ANVIL_IN_CONTAINER");
    command.env_remove("ANVIL_MIRI_JOBS");
    command.env_remove("GITHUB_ACTIONS");
    command.env_remove("TF_BUILD");
    for key in std::env::vars_os().map(|(key, _)| key) {
        if key.to_string_lossy().starts_with("ANVIL_INCLUDE_") {
            command.env_remove(key);
        }
    }
    for &(key, value) in environment {
        command.env(key, value);
    }
    command
}

fn run_just(root: &Path, arguments: &[&str], environment: &[(&str, &OsStr)]) -> Output {
    let _powershell = powershell_process_lock();
    just_command(root, arguments, environment)
        .output()
        .expect("just is required to verify generated recipe behavior")
}

fn run_just_with_real_cargo(root: &Path, arguments: &[&str]) -> Output {
    run_just_with_real_cargo_env(root, arguments, &[])
}

fn run_just_with_real_cargo_env(root: &Path, arguments: &[&str], environment: &[(&str, &OsStr)]) -> Output {
    let _powershell = powershell_process_lock();
    let mut command = Command::new("just");
    command.args(["--justfile", "Justfile"]).args(arguments).current_dir(root);
    command.env_remove("ANVIL_IMPACT");
    command.env_remove("ANVIL_IMPACT_INPUT_DIR");
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
fn scoped_check_propagates_missing_consumed_impact_cache() {
    if !tools_available() {
        return;
    }
    for (name, template, recipe, cache) in [
        ("fmt.just", FMT, "anvil-fmt", "include_modified.txt"),
        (
            "check-all-targets.just",
            CHECK_ALL_TARGETS,
            "anvil-check-all-targets",
            "include_affected.txt",
        ),
    ] {
        let tmp = fixture(
            &[(name, template), ("impact.just", IMPACT)],
            &[
                "anvil-component-nightly-rustfmt-validate-prereqs",
                "anvil-component-nightly-rustfmt-install",
                "anvil-tool-rustc-validate-prereqs",
                "anvil-toolchain-stable-install",
                "anvil-tool-cargo-each-validate-prereqs",
                "anvil-tool-cargo-each-install installer",
                "anvil-impact",
            ],
        );
        let log = tmp.path().join("cargo.log");
        let output = run_just(
            tmp.path(),
            &[recipe],
            &[("ANVIL_IMPACT", OsStr::new("consume")), ("FAKE_CARGO_LOG", log.as_os_str())],
        );

        assert_failed(&output, "missing consumed impact cache");
        // The impact dependency is stubbed, so this error proves the recipe
        // body propagated scope resolution failure before invoking Cargo.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(cache),
            "{recipe} must surface the resolver's own cache-missing error\nstderr:\n{stderr}"
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(!log.exists(), "the scoped command must not run after impact scope resolution fails");
    }
}

#[mutants::skip]
fn all_targets_fixture() -> TempDir {
    let tmp = fixture(
        &[("check-all-targets.just", CHECK_ALL_TARGETS), ("impact.just", IMPACT)],
        &[
            "anvil-tool-rustc-validate-prereqs",
            "anvil-toolchain-stable-install",
            "anvil-tool-cargo-each-validate-prereqs",
            "anvil-tool-cargo-each-install installer",
            "anvil-impact",
        ],
    );
    let justfile_path = tmp.path().join("Justfile");
    let justfile = fs::read_to_string(&justfile_path).unwrap().replace(
        "_anvil_stable_toolchain_args := \"@()\"",
        "_anvil_stable_toolchain_args := \"'+stable-test'\"",
    );
    write(&justfile_path, &justfile);
    tmp
}

#[test]
fn all_targets_checks_packages_in_both_configurations_and_preserves_exit_codes() {
    if !tools_available() {
        return;
    }
    let tmp = all_targets_fixture();
    let log = tmp.path().join("cargo.log");

    for selection in ["-p first@1.2.3 -p second@4.5.6", "--workspace"] {
        seed_include(tmp.path(), "affected", selection);
        let output = run_just(tmp.path(), &["anvil-check-all-targets"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
        assert!(
            output.status.success(),
            "isolated checks failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&log).unwrap().lines().collect::<Vec<_>>(),
            [
                format!(
                    "+stable-test each {selection} --keep-going -- cargo +stable-test check --package {{cargo-spec}} --all-targets --locked"
                ),
                format!(
                    "+stable-test each {selection} --keep-going -- cargo +stable-test check --package {{cargo-spec}} --all-targets --no-default-features --locked"
                ),
            ],
            "both configurations must check packages independently, with both Cargo processes selecting stable"
        );
        fs::remove_file(&log).unwrap();
    }

    for failure in ["FAKE_EACH_EXIT", "FAKE_EACH_DEFAULT_EXIT", "FAKE_EACH_NO_DEFAULT_EXIT"] {
        let failed = run_just(
            tmp.path(),
            &["anvil-check-all-targets"],
            &[("FAKE_CARGO_LOG", log.as_os_str()), (failure, OsStr::new(ARBITRARY_FAILURE_EXIT))],
        );
        assert_failed(&failed, "isolated check cargo-each failure");
        assert_eq!(failed.status.code(), Some(ARBITRARY_FAILURE_EXIT.parse().unwrap()));
        assert_eq!(
            fs::read_to_string(&log).unwrap().lines().count(),
            if failure == "FAKE_EACH_NO_DEFAULT_EXIT" { 2 } else { 1 },
            "a failed configuration must stop the check before the next configuration"
        );
        fs::remove_file(&log).unwrap();
    }

    let justfile_path = tmp.path().join("Justfile");
    let mut justfile = fs::read_to_string(&justfile_path).unwrap();
    justfile.push_str("\n[script(\"pwsh\", \"-NoProfile\")]\n_anvil-impact-include tier:\n    exit 0\n");
    write(&justfile_path, &justfile);
    let fallback = run_just(tmp.path(), &["anvil-check-all-targets"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
    assert!(
        fallback.status.success(),
        "empty impact output must fall back to the workspace:\n{}",
        String::from_utf8_lossy(&fallback.stderr)
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().collect::<Vec<_>>(),
        [
            "+stable-test each --workspace --keep-going -- cargo +stable-test check --package {cargo-spec} --all-targets --locked",
            "+stable-test each --workspace --keep-going -- cargo +stable-test check --package {cargo-spec} --all-targets --no-default-features --locked",
        ]
    );
}

#[test]
fn all_targets_skips_empty_affected_selection() {
    if !tools_available() {
        return;
    }
    let tmp = all_targets_fixture();
    seed_include(tmp.path(), "affected", "--skip");
    let log = tmp.path().join("cargo.log");
    let output = run_just(tmp.path(), &["anvil-check-all-targets"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
    assert!(
        output.status.success(),
        "an empty affected selection must succeed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!log.exists(), "Cargo must not run for an empty affected selection");
    assert!(String::from_utf8_lossy(&output.stdout).contains("no affected packages"));
}

#[test]
fn all_targets_isolation_exposes_missing_dev_dependency_features() {
    let tmp = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
    write(
        &tmp.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"provider\", \"consumer\", \"sibling\"]\n",
    );
    write(
        &tmp.path().join("provider").join("Cargo.toml"),
        "[package]\nname = \"provider\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [features]\ndefault = [\"std\"]\nstd = []\n",
    );
    write(
        &tmp.path().join("provider").join("src").join("lib.rs"),
        "#[cfg(feature = \"std\")]\npub fn std_api() {}\n",
    );
    write(
        &tmp.path().join("consumer").join("Cargo.toml"),
        "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nprovider = { path = \"../provider\", default-features = false }\n",
    );
    write(&tmp.path().join("consumer").join("src").join("lib.rs"), "");
    write(
        &tmp.path().join("consumer").join("tests").join("feature_contract.rs"),
        "#[test]\nfn uses_std_api() { provider::std_api(); }\n",
    );
    write(
        &tmp.path().join("sibling").join("Cargo.toml"),
        "[package]\nname = \"sibling\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nprovider = { path = \"../provider\", default-features = false, features = [\"std\"] }\n",
    );
    write(&tmp.path().join("sibling").join("src").join("lib.rs"), "");

    let lockfile = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(tmp.path())
        .output()
        .expect("Cargo is available to execute this integration test");
    assert!(
        lockfile.status.success(),
        "path-only fixture lockfile generation failed:\n{}",
        String::from_utf8_lossy(&lockfile.stderr)
    );

    for feature_args in [&[][..], &["--no-default-features"][..]] {
        for (selection, expected_success) in [(&["--workspace"][..], true), (&["--package", "consumer"][..], false)] {
            let output = Command::new("cargo")
                .args(["check", "--all-targets", "--locked", "--offline"])
                .args(feature_args)
                .args(selection)
                .current_dir(tmp.path())
                .output()
                .expect("Cargo is available to execute this integration test");
            assert_eq!(
                output.status.success(),
                expected_success,
                "workspace feature unification must mask the consumer's missing dev-dependency feature activation, but isolation must expose it:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if !expected_success {
                assert!(String::from_utf8_lossy(&output.stderr).contains("std_api"));
            }
        }
    }

    let consumer_manifest = tmp.path().join("consumer").join("Cargo.toml");
    let mut manifest = fs::read_to_string(&consumer_manifest).unwrap();
    manifest.push_str("\n[dev-dependencies]\nprovider = { path = \"../provider\", default-features = false, features = [\"std\"] }\n");
    write(&consumer_manifest, &manifest);
    for feature_args in [&[][..], &["--no-default-features"][..]] {
        let repaired = Command::new("cargo")
            .args(["check", "--package", "consumer", "--all-targets", "--locked", "--offline"])
            .args(feature_args)
            .current_dir(tmp.path())
            .output()
            .expect("Cargo is available to execute this integration test");
        assert!(
            repaired.status.success(),
            "declaring the required dev-dependency feature must make the isolated check pass:\n{}",
            String::from_utf8_lossy(&repaired.stderr)
        );
    }
}

fn assert_miri_cargo_calls(cargo_calls: &str) {
    assert!(
        cargo_calls.contains("+nightly-test metadata --no-deps --format-version 1"),
        "metadata and compiler-artifact IDs must use the same pinned nightly Cargo:\n{cargo_calls}"
    );
    assert!(
        cargo_calls.contains(
            "miri test --all-features --tests --no-run --message-format=json-render-diagnostics --workspace --exclude other-package"
        ),
        "workspace Miri compilation must exclude metadata-opted-out packages:\n{cargo_calls}"
    );
}

#[test]
fn miri_runner_filters_artifacts_and_runs_in_parallel() {
    if !tools_available() {
        return;
    }
    let _powershell = powershell_process_lock();
    let tmp = fixture(
        &[("miri.just", MIRI)],
        &[
            "anvil-component-nightly-miri-validate-prereqs",
            "anvil-component-nightly-rust-src-validate-prereqs",
            "anvil-component-nightly-miri-install",
            "anvil-component-nightly-rust-src-install",
            "anvil-impact",
        ],
    );
    let nested = tmp.path().join("nested/other-package");
    fs::create_dir_all(&nested).unwrap();
    write(
        &nested.join("Cargo.toml"),
        "[package]\nname = \"other-package\"\nversion = \"0.1.0\"\n",
    );
    let cargo_log = tmp.path().join("cargo.log");
    let run_log = tmp.path().join("miri-run");
    let barrier = tmp.path().join("miri-barrier");
    fs::create_dir(&barrier).unwrap();
    let artifacts = r#"[
        {"name":"zeta-test","package_id":"fixture 0.1.0","target_name":"zeta","target_kind":"test","test":true},
        {"name":"alpha-test","package_id":"fixture 0.1.0","target_name":"alpha","target_kind":"test","test":true,"duplicate":true},
        {"name":"ordinary-bin","package_id":"fixture 0.1.0","test":false},
        {"name":"excluded-test","package_id":"other-package 0.1.0","test":true}
    ]"#;
    let mut command = just_command(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[
            ("ANVIL_MIRI_JOBS", OsStr::new("2")),
            ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
            ("FAKE_MIRI_ARTIFACTS", OsStr::new(artifacts)),
            ("FAKE_MIRI_BARRIER_DIR", barrier.as_os_str()),
            ("FAKE_MIRI_RUN_LOG", run_log.as_os_str()),
            ("FAKE_RUSTC_PREAMBLE", OsStr::new("rustc diagnostic preamble")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other-package")),
            ("FAKE_SECOND_PACKAGE_DIR_LEAF", OsStr::new("other-package")),
            ("FAKE_SECOND_MIRI_EXCLUDE", OsStr::new("1")),
        ],
    );
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().expect("just is required to verify generated recipe behavior");

    let alpha_ready = barrier.join("alpha-test.ready");
    let zeta_ready = barrier.join("zeta-test.ready");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(alpha_ready.is_file() && zeta_ready.is_file()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let both_workers_admitted = alpha_ready.is_file() && zeta_ready.is_file();
    write(&barrier.join("release"), "");
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "parallel Miri runner should succeed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let cargo_log = fs::read_to_string(cargo_log).unwrap();
    assert_miri_cargo_calls(&cargo_log);
    assert!(
        both_workers_admitted,
        "both Miri workers must reach the barrier before either is released"
    );
    assert!(!barrier.join("ordinary-bin.ready").exists());
    assert!(!barrier.join("excluded-test.ready").exists());

    let recorded_cwd = fs::read_to_string(run_log.with_extension("alpha-test.cwd")).unwrap();
    let expected_cwd = fs::canonicalize(tmp.path()).unwrap();
    assert_eq!(fs::canonicalize(recorded_cwd.trim()).unwrap(), expected_cwd);
    assert!(
        fs::read_to_string(run_log.with_extension("alpha-test.args"))
            .unwrap()
            .trim()
            .is_empty(),
        "an unfiltered Miri run must not receive binary arguments"
    );
    assert_eq!(
        fs::read_to_string(run_log.with_extension("alpha-test.sysroot")).unwrap().trim(),
        tmp.path().join("fake-miri-sysroot").to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(run_log.with_extension("alpha-test.miri-be-rustc"))
            .unwrap()
            .trim(),
        "host"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let alpha_group = stdout.find("=== Miri executable fixture 0.1.0 :: test alpha").unwrap();
    let zeta_group = stdout.find("=== Miri executable fixture 0.1.0 :: test zeta").unwrap();
    assert!(alpha_group < zeta_group, "artifact logs must replay deterministically");
}

#[test]
fn miri_runner_labels_same_named_artifacts_with_package_identity() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("miri.just", MIRI)],
        &[
            "anvil-component-nightly-miri-validate-prereqs",
            "anvil-component-nightly-rust-src-validate-prereqs",
            "anvil-component-nightly-miri-install",
            "anvil-component-nightly-rust-src-install",
            "anvil-impact",
        ],
    );
    let other_package = tmp.path().join("nested/other-package");
    fs::create_dir_all(&other_package).unwrap();
    write(
        &other_package.join("Cargo.toml"),
        "[package]\nname = \"other-package\"\nversion = \"0.1.0\"\n",
    );
    let run_log = tmp.path().join("same-name");
    let fixture_id = "path+file:///workspace/fixture#fixture@0.1.0";
    let other_id = "path+file:///workspace/other#other-package@0.1.0";
    let artifacts = r#"[
        {"name":"shared-test","artifact_dir":"fixture","package_id":"path+file:///workspace/fixture#fixture@0.1.0","target_name":"shared","target_kind":"test","test":true},
        {"name":"shared-test","artifact_dir":"other","package_id":"path+file:///workspace/other#other-package@0.1.0","target_name":"shared","target_kind":"test","test":true}
    ]"#;
    let output = run_just(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[
            ("FAKE_MIRI_ARTIFACTS", OsStr::new(artifacts)),
            ("FAKE_MIRI_RUN_LOG", run_log.as_os_str()),
            ("ANVIL_MIRI_JOBS", OsStr::new("1")),
            ("FAKE_PACKAGE_ID", OsStr::new(fixture_id)),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other-package")),
            ("FAKE_SECOND_PACKAGE_ID", OsStr::new(other_id)),
            ("FAKE_SECOND_PACKAGE_DIR_LEAF", OsStr::new("other-package")),
        ],
    );

    assert!(
        output.status.success(),
        "same-named Miri executables should both run:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Miri executable fixture 0.1.0 :: test shared (shared-test)"));
    assert!(stdout.contains("Miri executable other-package 0.1.0 :: test shared (shared-test)"));
    assert!(
        !stdout.contains("path+file:///"),
        "display labels must not expose Cargo package IDs"
    );
}

#[test]
fn miri_runner_preserves_impact_filtering() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("miri.just", MIRI), ("miri-tree-borrows.just", MIRI_TREE_BORROWS)],
        &[
            "anvil-component-nightly-miri-validate-prereqs",
            "anvil-component-nightly-rust-src-validate-prereqs",
            "anvil-component-nightly-miri-install",
            "anvil-component-nightly-rust-src-install",
            "anvil-impact",
        ],
    );
    let run_log = tmp.path().join("miri-profile");
    let cargo_log = tmp.path().join("miri-profile-cargo.log");
    let artifacts = r#"[{"name":"profile-test","package_id":"fixture 0.1.0","test":true}]"#;
    let output = run_just(
        tmp.path(),
        &["anvil-miri-tree-borrows"],
        &[
            ("FAKE_INCLUDE", OsStr::new("--workspace --exclude other")),
            ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
            ("FAKE_MIRI_ARTIFACTS", OsStr::new(artifacts)),
            ("FAKE_MIRI_RUN_LOG", run_log.as_os_str()),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("excluded")),
            ("FAKE_SECOND_MIRI_EXCLUDE", OsStr::new("1")),
            ("FAKE_THIRD_PACKAGE_NAME", OsStr::new("other")),
        ],
    );

    assert!(
        output.status.success(),
        "Tree Borrows profile should delegate to the shared runner:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let cargo_log = fs::read_to_string(cargo_log).unwrap();
    let build_call = cargo_log
        .lines()
        .find(|call| call.contains("miri test"))
        .expect("the profile must compile its selected Miri targets");
    assert!(
        build_call.ends_with("--workspace --exclude other --exclude excluded"),
        "impact exclusions must be forwarded and opted-out packages added:\n{build_call}"
    );
}

#[test]
fn miri_profiles_set_expected_flags() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[
            ("miri.just", MIRI),
            ("miri-race-coverage.just", MIRI_RACE_COVERAGE),
            ("miri-strict-provenance.just", MIRI_STRICT_PROVENANCE),
            ("miri-tree-borrows.just", MIRI_TREE_BORROWS),
        ],
        &[
            "anvil-component-nightly-miri-validate-prereqs",
            "anvil-component-nightly-rust-src-validate-prereqs",
            "anvil-component-nightly-miri-install",
            "anvil-component-nightly-rust-src-install",
            "anvil-impact",
        ],
    );
    let artifacts = r#"[{"name":"profile-test","package_id":"fixture 0.1.0","test":true}]"#;

    for (recipe, expected_miri_flags, expected_rust_flags) in [
        ("anvil-miri-tree-borrows", "-Zmiri-tree-borrows", "--cfg miri_tree_borrows"),
        (
            "anvil-miri-strict-provenance",
            "-Zmiri-strict-provenance",
            "--cfg miri_strict_provenance",
        ),
        ("anvil-miri-race-coverage", "-Zmiri-many-seeds=", "--cfg miri_race_coverage"),
    ] {
        let profile_run_log = tmp.path().join(recipe);
        let output = run_just(
            tmp.path(),
            &[recipe],
            &[
                ("FAKE_INCLUDE", OsStr::new("--package fixture@0.1.0")),
                ("FAKE_MIRI_ARTIFACTS", OsStr::new(artifacts)),
                ("FAKE_MIRI_RUN_LOG", profile_run_log.as_os_str()),
            ],
        );
        assert!(
            output.status.success(),
            "{recipe} should inherit the shared runner:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            fs::read_to_string(profile_run_log.with_extension("profile-test.miriflags"))
                .unwrap()
                .contains(expected_miri_flags)
        );
        assert!(
            fs::read_to_string(profile_run_log.with_extension("profile-test.rustflags"))
                .unwrap()
                .contains(expected_rust_flags)
        );
    }
}

#[test]
fn miri_runner_handles_no_work_and_aggregates_failures() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("miri.just", MIRI)],
        &[
            "anvil-component-nightly-miri-validate-prereqs",
            "anvil-component-nightly-rust-src-validate-prereqs",
            "anvil-component-nightly-miri-install",
            "anvil-component-nightly-rust-src-install",
            "anvil-impact",
        ],
    );
    let no_artifacts = run_just(tmp.path(), &["_anvil-miri-test", "standard"], &[]);
    assert!(
        no_artifacts.status.success(),
        "a selected package set with no test executables should succeed:\n{}",
        String::from_utf8_lossy(&no_artifacts.stderr)
    );
    assert!(String::from_utf8_lossy(&no_artifacts.stdout).contains("no runnable test executables"));

    let all_excluded = run_just(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[
            ("FAKE_INCLUDE", OsStr::new("--workspace --exclude other")),
            ("FAKE_MIRI_EXCLUDE", OsStr::new("1")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other")),
        ],
    );
    assert!(all_excluded.status.success());
    assert!(String::from_utf8_lossy(&all_excluded.stdout).contains("all selected packages are excluded"));

    let workspace_cargo_log = tmp.path().join("all-excluded-workspace-cargo.log");
    let all_workspace_excluded = run_just(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[
            ("FAKE_CARGO_LOG", workspace_cargo_log.as_os_str()),
            ("FAKE_MIRI_EXCLUDE", OsStr::new("1")),
        ],
    );
    assert!(all_workspace_excluded.status.success());
    assert!(String::from_utf8_lossy(&all_workspace_excluded.stdout).contains("all selected packages are excluded"));
    assert!(
        !fs::read_to_string(workspace_cargo_log).unwrap().contains("miri test"),
        "an all-excluded workspace must stop before asking Cargo to select no packages"
    );

    let run_log = tmp.path().join("miri-failure");
    let artifacts = r#"[
        {"name":"fail-test","package_id":"fixture 0.1.0","test":true},
        {"name":"pass-test","package_id":"fixture 0.1.0","test":true}
    ]"#;
    let failed = run_just(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[
            ("FAKE_MIRI_ARTIFACTS", OsStr::new(artifacts)),
            ("FAKE_MIRI_RUN_LOG", run_log.as_os_str()),
            ("ANVIL_MIRI_JOBS", OsStr::new("2")),
            ("TF_BUILD", OsStr::new("True")),
        ],
    );
    assert_failed(&failed, "one failed Miri executable");
    let stdout = String::from_utf8_lossy(&failed.stdout);
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stdout.contains("miri output: fail-test"));
    assert!(stdout.contains("miri output: pass-test"));
    assert!(stdout.contains("##[group]Miri executable fixture 0.1.0 :: test fail-test (fail-test)"));
    assert!(stdout.contains("##[endgroup]"));
    assert!(stderr.contains("failed executables: fixture 0.1.0 :: test fail-test (fail-test)"));

    let invalid_jobs = run_just(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[
            (
                "FAKE_MIRI_ARTIFACTS",
                OsStr::new(r#"[{"name":"jobs-test","package_id":"fixture 0.1.0","test":true}]"#),
            ),
            ("FAKE_MIRI_RUN_LOG", run_log.as_os_str()),
            ("ANVIL_MIRI_JOBS", OsStr::new("0")),
        ],
    );
    assert_failed(&invalid_jobs, "invalid ANVIL_MIRI_JOBS");
    assert!(String::from_utf8_lossy(&invalid_jobs.stderr).contains("anvil miri: ANVIL_MIRI_JOBS must be a positive integer"));

    let invalid_json = run_just(
        tmp.path(),
        &["_anvil-miri-test", "standard"],
        &[("FAKE_MIRI_INVALID_JSON", OsStr::new("1"))],
    );
    assert_failed(&invalid_json, "malformed Cargo JSON");
    assert!(String::from_utf8_lossy(&invalid_json.stderr).contains("could not parse Cargo JSON output"));
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
         _anvil-impact-exclude tier:\n\
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
            ("FAKE_RESOLVER_OUTPUT", OsStr::new("test-msrv")),
            ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
        ],
    );
    assert_failed(&impact_failure, "failed impact helper");
    assert_eq!(
        impact_failure.status.code(),
        Some(31),
        "the impact helper's exit code must propagate"
    );
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
    assert_eq!(
        resolver_failure.status.code(),
        Some(32),
        "the MSRV resolver's exit code must propagate"
    );
    assert!(
        fs::read_to_string(&cargo_log).unwrap_or_default().is_empty(),
        "Cargo must not run after resolver failure"
    );

    let no_msrv = run_just(
        tmp.path(),
        &["anvil-msrv-test"],
        &[
            ("FAKE_IMPACT_HELPER_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT)),
            ("FAKE_METADATA_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT)),
            ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
        ],
    );
    assert!(
        no_msrv.status.success(),
        "missing MSRV must skip even when package discovery is unavailable:\n{}",
        String::from_utf8_lossy(&no_msrv.stderr)
    );
    assert!(String::from_utf8_lossy(&no_msrv.stdout).contains("no root MSRV declared; skipping"));
    assert!(
        fs::read_to_string(cargo_log).unwrap_or_default().is_empty(),
        "missing MSRV must skip before any Cargo invocation"
    );
}

// The MSRV is the one version anvil installs that is declared in `Cargo.toml`
// rather than pinned in `versions.just`. The manifest is admitted to the build
// context so the resolver reads it there as it does anywhere else.
#[test]
fn root_msrv_reports_the_declared_version() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("versions.just", VERSIONS), ("tools.just", TOOLS)], &[]);

    let output = run_just(tmp.path(), &["_anvil-resolve-stable", "root-msrv"], &[]);

    assert!(
        output.status.success(),
        "root-msrv should resolve from the manifest:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1.97");
}

#[test]
fn root_msrv_reads_workspace_package_ahead_of_package() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("versions.just", VERSIONS), ("tools.just", TOOLS)], &[]);
    // The shape a real workspace root has, and the one the container image
    // resolves against: both tables present, and `workspace.package` winning.
    // The fixture default declares only `[package]`, so without this the
    // precedence half of the scanner is never exercised.
    write(
        &tmp.path().join("Cargo.toml"),
        "[workspace.package]\nrust-version = \"1.93\"\n\n\
         [package]\nname = \"fixture\"\nversion = \"0.1.0\"\nrust-version = \"1.97\"\n",
    );

    let output = run_just(tmp.path(), &["_anvil-resolve-stable", "root-msrv"], &[]);

    assert!(
        output.status.success(),
        "root-msrv should resolve from a workspace root:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "1.93",
        "the workspace declaration is what cargo resolves members against, so it is what the \
         image must install"
    );
}

#[test]
fn root_msrv_reports_none_when_the_repository_declares_no_msrv() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("versions.just", VERSIONS), ("tools.just", TOOLS)], &[]);
    write(
        &tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    );

    let output = run_just(tmp.path(), &["_anvil-resolve-stable", "root-msrv"], &[]);

    assert!(
        output.status.success(),
        "root-msrv must answer for a repository with no MSRV:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "none",
        "the answer must be total, because the caller hashes it into the container image tag"
    );
}

#[test]
fn container_build_carries_the_manifest_that_declares_the_msrv() {
    assert!(
        CONTAINER_DOCKERIGNORE.contains("!Cargo.toml"),
        "the build context must admit the root manifest, or the setup cannot resolve the MSRV"
    );
    assert!(
        CONTAINER_SETUP_REGION.contains("COPY . ./"),
        "the setup region must copy the scoped context to the root the recipes resolve against"
    );
    // The tag hashes the declared MSRV, not the file, so an unrelated dependency
    // edit computes the same tag. A manifest left in the image would make that
    // tag name two different filesystems.
    assert!(
        CONTAINER_SETUP_REGION.contains("rm -f Cargo.toml"),
        "the setup region must delete the manifest once read, or the tag describes an image that \
         can differ from it"
    );
    // The members it names are a checkout, and the image is not one.
    assert!(
        !CONTAINER_DOCKERIGNORE.contains("!sources") && !CONTAINER_DOCKERIGNORE.contains("!crates"),
        "the context must stay a recipe tree plus declarations, not a checkout"
    );
    // The manifest is in the context but must not be in the identity: every
    // dependency edit touches it while `rust-version` moves perhaps once.
    assert!(
        CONTAINER.contains("'msrv ' + $msrvBytes.Length"),
        "the image tag must hash the declared MSRV value, because the image installs that toolchain"
    );
    assert!(
        !CONTAINER.contains("ANVIL_ROOT_MSRV"),
        "the value travels in the context as a file, not as a build argument a replaced setup \
         region can silently drop"
    );
    assert!(
        !CONTAINER_SETUP_REGION.contains("ANVIL_ROOT_MSRV"),
        "the setup region must not reintroduce the build argument"
    );
}

#[test]
fn the_container_build_does_not_require_a_root_toolchain_file() {
    assert!(
        !CONTAINER_SETUP_REGION.contains("COPY rust-toolchain"),
        "naming the toolchain file makes the image unbuildable in exactly the repositories that \
         have nothing to pin"
    );
    assert!(
        CONTAINER_DOCKERIGNORE.contains("!rust-toolchain.toml") && CONTAINER_DOCKERIGNORE.contains("!rust-toolchain\n"),
        "the context must admit a root toolchain file in either spelling"
    );
}

/// A repository with no root toolchain file has nothing but the image's own
/// default to select a compiler, and `rustup-init` ran with
/// `--default-toolchain none`. Rustup does set the default from the first
/// install that finds none set, so this holds today by accident of the order
/// `anvil-setup` reaches the install recipes in; a reordering that installed a
/// nightly first would silently make it the compiler `cargo` runs in the
/// container.
#[test]
fn the_image_names_its_default_toolchain() {
    assert!(
        CONTAINER_SETUP_REGION.contains("rustup default"),
        "the setup region must name the image's default toolchain rather than inherit whichever \
         one the setup graph installed first"
    );
    assert!(
        CONTAINER_SETUP_REGION.contains("_anvil-resolve-stable root-msrv"),
        "the default must be the declared MSRV, the version the setup installs for a repository \
         that pins nothing"
    );
    let default_at = CONTAINER_SETUP_REGION.find("rustup default").unwrap();
    let removal_at = CONTAINER_SETUP_REGION.find("rm -f Cargo.toml").unwrap();
    assert!(
        default_at < removal_at,
        "the MSRV is read from the root manifest, so the default must be set before the setup \
         deletes it"
    );
}

fn container_impact_script() -> String {
    let recipe = CONTAINER.split_once("anvil-container *command:\n").unwrap().1;
    let mut recipe = recipe
        .lines()
        .take_while(|line| line.is_empty() || line.starts_with("    "))
        .map(|line| line.strip_prefix("    ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    for (expression, value) in [
        ("{{ replace(command, \"'\", \"''\") }}", "echo ok"),
        ("{{ replace(just_executable(), \"'\", \"''\") }}", "Invoke-Just"),
        ("{{ replace(justfile_directory(), \"'\", \"''\") }}", "."),
        ("{{ replace(invocation_directory_native(), \"'\", \"''\") }}", "."),
        ("{{anvil_container_workdir}}", "/workspace"),
        ("{{anvil_container_name}}", "fixture"),
    ] {
        recipe = recipe.replace(expression, value);
    }
    assert!(!recipe.contains("{{"), "all recipe interpolation inputs must be controlled");

    // Execute the complete recipe, with host/process collaborators scripted in
    // the same interpreter. No nested just/pwsh startup or mutable disk fixture
    // participates in the refusal or forwarding verdict.
    format!(
        "{}\n{recipe}",
        r#"
if ($PSVersionTable.PSVersion -lt [version]'7.6') { throw 'PowerShell 7.6 or newer is required' }
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
function Invoke-Just {
    $global:LASTEXITCODE = 0
    switch ($args[0]) {
        '_anvil-container-engine' { 'Fake-Engine' }
        '_anvil-container-image' { 'example.invalid/anvil:test' }
        '_anvil-container-path' { $args[1] }
        default { throw "unexpected just call: $args" }
    }
}
function Get-Command {
    param($Name, $ErrorAction, $CommandType)
    if ($Name -ne 'git') { throw "unexpected command lookup: $Name" }
}
function Test-Path {
    param([string]$LiteralPath, $PathType)
    if ($LiteralPath.StartsWith('Env:')) {
        $name = $LiteralPath.Substring(4)
        if ($name -notin @('PR_TITLE', 'BASE_REF', 'GITHUB_BASE_REF', 'SYSTEM_PULLREQUEST_TARGETBRANCH',
                          'ANVIL_IMPACT', 'ANVIL_MIRI_JOBS', 'ANVIL_IMPACT_INPUT_DIR')) {
            throw "unexpected environment lookup: $name"
        }
        return $null -ne [Environment]::GetEnvironmentVariable($name)
    }
    if ($LiteralPath.Replace('\', '/').EndsWith('.anvil/container/hooks.ps1')) { return $false }
    throw "unexpected path lookup: $LiteralPath"
}
function id {
    if ($args.Count -ne 1 -or $args[0] -notin @('-u', '-g')) { throw "unexpected id call: $args" }
    $global:LASTEXITCODE = 0
    '1000'
}
function Fake-Engine {
    ConvertTo-Json -InputObject @($args) -Compress
    $global:LASTEXITCODE = 0
}
"#,
    )
}

#[test]
fn container_rejects_impact_input_override_only_in_consume_mode() {
    if !tools_available() {
        return;
    }
    let _powershell = powershell_process_lock();
    let script = container_impact_script();
    let run = |mode: Option<&str>, input_override: &str| {
        let mut command = Command::new("pwsh");
        command.args(["-NoProfile", "-Command", &script]);
        for name in [
            "ANVIL_IMPACT",
            "ANVIL_IN_CONTAINER",
            "ANVIL_MIRI_JOBS",
            "PR_TITLE",
            "BASE_REF",
            "GITHUB_BASE_REF",
            "SYSTEM_PULLREQUEST_TARGETBRANCH",
        ] {
            command.env_remove(name);
        }
        command.env("ANVIL_IMPACT_INPUT_DIR", input_override);
        if let Some(mode) = mode {
            command.env("ANVIL_IMPACT", mode);
        }
        command.output().expect("pwsh was checked by tools_available")
    };

    let consume = run(Some("consume"), "injected-impact-cache");
    assert_failed(&consume, "running a container with an injected consume cache");
    let consume_stderr = String::from_utf8_lossy(&consume.stderr);
    assert!(
        consume_stderr.contains("ANVIL_IMPACT=consume")
            && consume_stderr.contains("ANVIL_IMPACT_INPUT_DIR")
            && consume_stderr.contains("cannot honor"),
        "the refusal must identify the unsupported consume override\nstderr:\n{consume_stderr}"
    );
    assert!(
        consume.stdout.is_empty(),
        "consume mode must reject the override before invoking the container engine"
    );

    for (mode, input_override) in [
        (Some("off"), "injected-impact-cache"),
        (None, "injected-impact-cache"),
        (Some("consume"), ""),
    ] {
        let output = run(mode, input_override);
        assert!(
            output.status.success(),
            "an ignored or empty impact-cache override must not block container execution in mode {mode:?} with input {input_override:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let arguments = String::from_utf8(output.stdout).unwrap();
        let args: Vec<String> = from_str(&arguments).unwrap();
        let forwards_impact = args.windows(2).any(|pair| pair == ["-e", "ANVIL_IMPACT"]);
        assert_eq!(
            forwards_impact,
            mode.is_some(),
            "unexpected impact-mode forwarding for {mode:?}:\n{arguments}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("ANVIL_IMPACT_INPUT_DIR")),
            "the host-only cache path must never be forwarded to the container engine:\n{arguments}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("injected-impact-cache")),
            "the impact-cache host path must never be forwarded to the container engine:\n{arguments}"
        );
        assert_eq!(&args[args.len() - 3..], ["example.invalid/anvil:test", "echo", "ok"]);
    }
}

/// `anvil-setup` must not reach workspace MSRV validation: the image runs it
/// against a context carrying the root manifest and none of the members that
/// validation resolves through `cargo metadata`. The fixture here is an
/// ordinary workspace, because what is under test is the recipe graph rather
/// than the container's filesystem.
///
/// Nothing else declares this. An edge added later, or a recipe body that
/// shells out to one, would surface as a cargo path error inside an image
/// build, naming a manifest instead of the edge that reached it. The whole
/// emitted tree is planned, because the edge could be added in any tier, group
/// or check file. `anvil-setup` does reach `anvil-tool-pwsh-validate-prereqs`,
/// so the assertion is about the one validator that resolves members, not about
/// the family.
#[test]
fn setup_never_reaches_workspace_msrv_validation() {
    if !tools_available() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n[workspace.package]\nrust-version = \"1.90\"\n",
    );
    write(
        &root.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    write(&root.join("crates/alpha/src/lib.rs"), "");
    run_update(
        &cargo_anvil::Catalog::anvil(),
        &Cli {
            backends: vec![],
            no_backends: true,
            dry_run: false,
            force: false,
        },
        root,
    )
    .unwrap();
    write(&root.join("Justfile"), "import 'justfiles/anvil/mod.just'\n");

    let output = run_just(root, &["--dry-run", "anvil-setup", "binstall"], &[]);
    assert!(
        output.status.success(),
        "planning the image's setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // The needle is the invocation, not the bare action name, which the
    // resolver's own body lists among the actions it accepts.
    let plan = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        plan.contains("_anvil-resolve-stable install-msrv"),
        "the plan must reach the resolver at all, or this test proves nothing\nplan:\n{plan}"
    );
    assert!(
        !plan.contains("_anvil-resolve-stable validate-workspace-msrv"),
        "`anvil-setup` reached workspace MSRV validation, which the image's context cannot answer: \
         it carries the root manifest and none of its members"
    );
}

#[test]
fn validation_disables_auto_install_and_preserves_cargo_failures() {
    if !tools_available() {
        return;
    }

    let helper_recipes = "\n[script(\"pwsh\", \"-NoProfile\")]\n\
         _anvil-resolve-stable action:\n\
         \x20   Write-Output '1.97'\n";
    let msrv = fixture(&[("msrv-test.just", MSRV_TEST)], &["anvil-impact"]);
    let justfile_path = msrv.path().join("Justfile");
    let mut justfile = fs::read_to_string(&justfile_path).unwrap();
    justfile.push_str(helper_recipes);
    write(&justfile_path, &justfile);
    let auto_install_log = msrv.path().join("auto-install.log");
    let output = run_just(
        msrv.path(),
        &["anvil-msrv-test-validate-prereqs"],
        &[("FAKE_CARGO_AUTO_INSTALL_LOG", auto_install_log.as_os_str())],
    );
    assert!(
        output.status.success(),
        "MSRV validation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(auto_install_log).unwrap().trim(), "0");

    let output = run_just(
        msrv.path(),
        &["anvil-msrv-test-validate-prereqs"],
        &[("FAKE_CARGO_DEFAULT_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT))],
    );
    assert_eq!(
        output.status.code(),
        Some(ARBITRARY_FAILURE_EXIT.parse().unwrap()),
        "MSRV validation must preserve Cargo's failure status"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("MSRV toolchain '1.97' is unavailable"),
        "MSRV validation must retain its actionable diagnostic"
    );

    let tools = fixture(&[("versions.just", VERSIONS), ("tools.just", TOOLS)], &[]);
    let output = run_just(
        tools.path(),
        &["_check-tool", "cargo-example", "1.2.3"],
        &[
            ("FAKE_INSTALL_LIST_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT)),
            ("FAKE_INSTALL_LIST_OUTPUT", OsStr::new("selected Cargo failed")),
        ],
    );
    assert_failed(&output, "failed cargo install --list");
    assert_eq!(
        output.status.code(),
        Some(ARBITRARY_FAILURE_EXIT.parse().unwrap()),
        "tool validation must preserve cargo install --list's failure status"
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("selected stable Cargo is unavailable")
            && diagnostic.contains("selected Cargo failed")
            && !diagnostic.contains("required tool 'cargo-example' not found"),
        "unexpected Cargo-list diagnostic: {diagnostic}"
    );

    let rustc_auto_install_log = tools.path().join("rustc-auto-install.log");
    write(
        &tools.path().join("fake-bin/rustc.ps1"),
        "if ($env:FAKE_RUSTC_AUTO_INSTALL_LOG) {\n\
         \x20   Add-Content -LiteralPath $env:FAKE_RUSTC_AUTO_INSTALL_LOG -Value $env:RUSTUP_AUTO_INSTALL\n\
         }\n\
         exit [int]$env:FAKE_RUSTC_EXIT\n",
    );
    let output = run_just(
        tools.path(),
        &["_check-component", "default", "rust-src"],
        &[
            ("FAKE_RUSTC_AUTO_INSTALL_LOG", rustc_auto_install_log.as_os_str()),
            ("FAKE_RUSTC_EXIT", OsStr::new("9")),
        ],
    );
    assert_failed(&output, "unavailable selected stable toolchain");
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("selected stable toolchain")
            && diagnostic.contains("just anvil-toolchain-stable-install")
            && !diagnostic.contains("rustup toolchain install default"),
        "unexpected default-toolchain diagnostic: {diagnostic}"
    );
    assert_eq!(fs::read_to_string(&rustc_auto_install_log).unwrap().trim(), "0");

    let cargo_log = tools.path().join("component-cargo.log");
    let output = run_just(
        tools.path(),
        &["_check-component", "default", "clippy"],
        &[("FAKE_RUSTC_EXIT", OsStr::new("9")), ("FAKE_CARGO_LOG", cargo_log.as_os_str())],
    );
    assert_failed(&output, "unavailable selected toolchain for command-backed component");
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("selected stable toolchain") && !diagnostic.contains("component 'clippy' not installed"),
        "toolchain failure must precede component diagnosis: {diagnostic}"
    );
    assert!(
        fs::read_to_string(cargo_log).unwrap_or_default().is_empty(),
        "component command must not run when the selected toolchain is unavailable"
    );
}

#[test]
fn impact_format_rejects_unknown_packages_and_metadata_errors() {
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
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .lines()
            .all(|line| line == "metadata --locked --no-deps --format-version 1"),
        "impact metadata discovery must always be locked"
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

#[test]
fn impact_format_rejects_library_and_directory_aliases() {
    assert!(
        tools_available(),
        "recipe execution requires `just` and `pwsh` on PATH; install Just and PowerShell before running these tests"
    );
    let tmp = fixture(&[("impact.just", IMPACT)], &[]);
    for alias in ["workspace-leaf", "fixture_lib"] {
        write(
            &tmp.path().join("impact.json"),
            &format!(r#"{{"Modified":[],"Affected":["{alias}"],"Required":[]}}"#),
        );
        let alias_only = run_just(
            tmp.path(),
            &["_anvil-impact-format", "affected", "impact.json"],
            &[
                ("FAKE_PACKAGE_NAME", OsStr::new("fixture-package")),
                ("FAKE_LIB_NAME", OsStr::new("fixture_lib")),
                ("FAKE_PACKAGE_DIR_LEAF", OsStr::new("workspace-leaf")),
            ],
        );
        assert_failed(&alias_only, "library or directory name without a matching package");
        assert!(
            String::from_utf8_lossy(&alias_only.stderr).contains(&format!("unknown package '{alias}'")),
            "library and directory aliases must not resolve as package names:\n{}",
            String::from_utf8_lossy(&alias_only.stderr)
        );
    }
}

#[test]
fn impact_format_resolves_packages_despite_name_collisions() {
    assert!(
        tools_available(),
        "recipe execution requires `just` and `pwsh` on PATH; install Just and PowerShell before running these tests"
    );
    let tmp = fixture(&[("impact.just", IMPACT)], &[]);
    write(
        &tmp.path().join("impact.json"),
        r#"{"Modified":[],"Affected":["workspace_leaf"],"Required":[]}"#,
    );
    for (package_name, lib_name, second_package_name, second_package_dir) in [
        ("workspace_leaf", "fixture_lib", "other_package", "workspace_leaf"),
        ("other_package", "workspace_leaf", "workspace_leaf", "second-package"),
    ] {
        let resolved = run_just(
            tmp.path(),
            &["_anvil-impact-format", "affected", "impact.json"],
            &[
                ("FAKE_PACKAGE_NAME", OsStr::new(package_name)),
                ("FAKE_LIB_NAME", OsStr::new(lib_name)),
                ("FAKE_PACKAGE_DIR_LEAF", OsStr::new("first-package")),
                ("FAKE_SECOND_PACKAGE_NAME", OsStr::new(second_package_name)),
                ("FAKE_SECOND_PACKAGE_DIR_LEAF", OsStr::new(second_package_dir)),
            ],
        );
        assert!(
            resolved.status.success(),
            "an exact package name must resolve despite library or directory collisions:\n{}",
            String::from_utf8_lossy(&resolved.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&resolved.stdout).trim(), "--package workspace_leaf@0.1.0");
    }
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
            "anvil-tool-cargo-each-validate-prereqs",
            "anvil-tool-cargo-each-install installer",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
    let output = run_just(
        tmp.path(),
        &["anvil-bolero"],
        &[("FAKE_EACH_EXIT", OsStr::new("9")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );

    assert_failed(&output, "cargo bolero target discovery failure");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("target discovery failed for package 'fixture@0.1.0'"),
        "the failure must come from target discovery:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
            "anvil-tool-cargo-each-validate-prereqs",
            "anvil-tool-cargo-each-install installer",
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
        calls.contains("each --package fixture@0.1.0 -- cargo +nightly-test bolero list --profile release --package {cargo-spec}"),
        "the first workspace member must be discovered, got:\n{calls}"
    );
    assert!(
        calls.contains("each --package other-package@0.1.0 -- cargo +nightly-test bolero list --profile release --package {cargo-spec}"),
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
fn metadata_consuming_checks_fail_when_discovery_fails() {
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
                "anvil-toolchain-nightly-external-types-validate-prereqs",
                "anvil-tool-cargo-check-external-types-install installer",
                "anvil-toolchain-nightly-external-types-install",
                "anvil-tool-cargo-each-validate-prereqs",
                "anvil-tool-cargo-each-install installer",
                "anvil-impact",
            ][..],
        ),
        (
            "doc-test.just",
            DOC_TEST,
            "anvil-doc-test",
            &["anvil-doc-test-validate-prereqs", "anvil-toolchain-stable-install", "anvil-impact"][..],
        ),
    ] {
        let tmp = fixture(&[(recipe_file, contents), ("impact.just", IMPACT)], dependencies);
        seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
        if recipe == "anvil-external-types" {
            let cargo_log = tmp.path().join("cargo.log");
            let output = run_just(
                tmp.path(),
                &[recipe],
                &[
                    ("FAKE_EACH_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT)),
                    ("FAKE_CARGO_LOG", cargo_log.as_os_str()),
                ],
            );
            assert_failed(&output, "external-types cargo-each discovery failure");
            assert!(
                fs::read_to_string(cargo_log)
                    .unwrap()
                    .contains("each --package fixture@0.1.0 --filter lib --keep-going -- cargo +nightly-test check-external-types --manifest-path {manifest}"),
                "external-types must reach its cargo-each invocation"
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("anvil-external-types: failed."));
            continue;
        }
        let output = run_just(tmp.path(), &[recipe], &[("FAKE_METADATA_EXIT", OsStr::new(ARBITRARY_FAILURE_EXIT))]);
        assert_failed(&output, &format!("{recipe} cargo metadata failure"));
        if recipe == "anvil-doc-test" {
            assert_eq!(
                output.status.code(),
                Some(ARBITRARY_FAILURE_EXIT.parse().unwrap()),
                "doc-test capability discovery must preserve Cargo metadata's exact failure status"
            );
        }

        let malformed = run_just(tmp.path(), &[recipe], &[("FAKE_METADATA_INVALID", OsStr::new("1"))]);
        assert_failed(&malformed, &format!("{recipe} malformed cargo metadata"));
    }
}

#[test]
fn fmt_runs_the_pinned_nightly_over_workspace_members() {
    assert!(
        !FMT.contains("_anvil_stable_toolchain_args"),
        "formatting must pin both Cargo invocations to nightly"
    );
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
    let args_log = tmp.path().join("cargo-args.log");
    let output = run_just(
        tmp.path(),
        &["anvil-fmt"],
        &[
            ("FAKE_CARGO_LOG", args_log.as_os_str()),
            ("RUSTUP_TOOLCHAIN", OsStr::new("test-stable")),
        ],
    );
    assert!(
        output.status.success(),
        "per-package formatting failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let invocations = fs::read_to_string(&args_log).unwrap();
    assert!(
        invocations.contains("+nightly-test each --workspace --keep-going -- cargo +nightly-test fmt --manifest-path {manifest} --check"),
        "unexpected cargo invocation: {invocations}"
    );
    assert!(!invocations.contains("fmt --all"));
}

#[test]
fn fmt_fix_removes_the_check_flag() {
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
    let output = run_just(tmp.path(), &["anvil-fmt", "--fix"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
    assert!(output.status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(commands.contains("+nightly-test each --workspace --keep-going -- cargo +nightly-test fmt --manifest-path {manifest}"));
    assert!(!commands.contains("--check"));
}

#[test]
fn build_and_doc_options_produce_the_expected_cargo_commands() {
    if !tools_available() {
        return;
    }
    let build = fixture(
        &[("build.just", BUILD)],
        &["anvil-tool-rustc-validate-prereqs", "anvil-toolchain-stable-install"],
    );
    let build_log = build.path().join("cargo.log");
    let build_output = run_just(
        build.path(),
        &["anvil-build", "--package", "fixture", "--profile", "release"],
        &[("FAKE_CARGO_LOG", build_log.as_os_str())],
    );
    assert!(
        build_output.status.success(),
        "anvil-build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build_output.stdout),
        String::from_utf8_lossy(&build_output.stderr)
    );
    let build_commands = fs::read_to_string(build_log).unwrap();
    assert!(build_commands.contains("build --package fixture --profile release --all-features --all-targets --locked"));

    let docs = fixture(
        &[("doc-build.just", DOC_BUILD), ("impact.just", IMPACT)],
        &[
            "anvil-tool-rustc-validate-prereqs",
            "anvil-toolchain-stable-install",
            "anvil-impact",
        ],
    );
    seed_include(docs.path(), "required", "--workspace");
    let docs_log = docs.path().join("cargo.log");
    let docs_output = run_just(
        docs.path(),
        &["anvil-doc-build", "--open"],
        &[("FAKE_CARGO_LOG", docs_log.as_os_str())],
    );
    assert!(docs_output.status.success());
    let docs_commands = fs::read_to_string(docs_log).unwrap();
    assert!(
        docs_commands.contains("doc --workspace --all-features --no-deps --open"),
        "unexpected cargo invocation: {docs_commands}"
    );
}

#[test]
fn miri_target_options_preserve_the_default_and_select_examples_explicitly() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("miri.just", MIRI), ("impact.just", IMPACT)],
        &[
            "anvil-component-nightly-miri-install",
            "anvil-component-nightly-rust-src-install",
            "anvil-component-nightly-miri-validate-prereqs",
            "anvil-component-nightly-rust-src-validate-prereqs",
            "anvil-impact",
        ],
    );
    let log = tmp.path().join("cargo.log");
    let run_log = tmp.path().join("miri-run");
    let artifacts = r#"[{"name":"fixture-test","package_id":"fixture 0.1.0","test":true}]"#;
    let default_output = run_just(
        tmp.path(),
        &["anvil-miri"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_INCLUDE", OsStr::new("--workspace --exclude other-package")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("other-package")),
            ("FAKE_SECOND_PACKAGE_DIR_LEAF", OsStr::new("other-package")),
        ],
    );
    assert!(
        default_output.status.success(),
        "default anvil-miri failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&default_output.stdout),
        String::from_utf8_lossy(&default_output.stderr)
    );
    let filtered_output = run_just(
        tmp.path(),
        &["anvil-miri", "--package", "fixture", "--test", "module::test_name"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_MIRI_ARTIFACTS", OsStr::new(artifacts)),
            ("FAKE_MIRI_RUN_LOG", run_log.as_os_str()),
        ],
    );
    assert!(
        filtered_output.status.success(),
        "filtered anvil-miri failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&filtered_output.stdout),
        String::from_utf8_lossy(&filtered_output.stderr)
    );
    let example_output = run_just(
        tmp.path(),
        &["anvil-miri", "--package", "fixture", "--example", "basic"],
        &[("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert!(example_output.status.success());
    let commands = fs::read_to_string(log).unwrap();
    assert!(
        commands.contains(
            "+nightly-test miri test --all-features --tests --no-run --message-format=json-render-diagnostics --workspace --exclude other-package"
        ),
        "default Miri target selection was not preserved:\n{commands}"
    );
    assert!(
        commands
            .contains("+nightly-test miri test --all-features --tests --no-run --message-format=json-render-diagnostics --package fixture"),
        "explicit package selection was not forwarded:\n{commands}"
    );
    let runner_args = fs::read_to_string(run_log.with_extension("fixture-test.args")).unwrap();
    assert_eq!(
        runner_args.trim(),
        "module::test_name",
        "the libtest filter must be forwarded to the executed Miri artifact"
    );
    assert!(
        commands.contains("+nightly-test miri run --all-features --locked --package fixture --example basic"),
        "explicit example selection was not forwarded:\n{commands}"
    );
}

#[test]
fn examples_skip_when_no_packages_are_affected() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("examples.just", EXAMPLES), ("impact.just", IMPACT)],
        &[
            "anvil-tool-rustc-validate-prereqs",
            "anvil-toolchain-stable-install",
            "anvil-impact",
        ],
    );
    seed_include(tmp.path(), "affected", "--skip");
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-examples"],
        &[("ANVIL_IMPACT", OsStr::new("consume")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "empty example selection failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().replace("\r\n", "\n"),
        "anvil-examples: no affected packages; skipping\n"
    );
    assert!(
        !log.exists(),
        "an empty selection must not invoke Cargo for build, metadata, or execution"
    );
}

/// Creates a two-package workspace for example-selection tests.
///
/// The root package excludes `blocked` and `sleeping` from default execution.
fn examples_workspace() -> TempDir {
    let workspace = fixture(
        &[("examples.just", EXAMPLES), ("impact.just", IMPACT)],
        &[
            "anvil-tool-rustc-validate-prereqs",
            "anvil-toolchain-stable-install",
            "anvil-impact",
        ],
    );
    write(
        &workspace.path().join("Cargo.toml"),
        r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2024"

[package.metadata.anvil.examples]
no-run = ["blocked", "sleeping"]

[workspace]
members = ["other"]
"#,
    );
    write(&workspace.path().join("src/lib.rs"), "");
    let example = r#"fn main() {
    assert_eq!(std::env::var("ANVIL_EXAMPLE").as_deref(), Ok("1"));
    println!("example-ran: {}", env!("CARGO_PKG_NAME"));
}
"#;
    write(&workspace.path().join("examples/ok.rs"), example);
    write(
        &workspace.path().join("examples/blocked.rs"),
        "fn main() { std::process::exit(7); }\n",
    );
    write(
        &workspace.path().join("examples/sleeping.rs"),
        "fn main() { loop { std::thread::park(); } }\n",
    );
    write(
        &workspace.path().join("other/Cargo.toml"),
        "[package]\nname = \"other\"\nversion = \"0.2.0\"\nedition = \"2024\"\n",
    );
    write(&workspace.path().join("other/examples/other.rs"), example);
    let lock = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(workspace.path())
        .output()
        .expect("cargo is required to prepare the example fixture");
    assert!(lock.status.success(), "failed to create fixture lockfile");
    workspace
}

/// Cached impact scope selects packages, while explicit `--package` overrides it,
/// including when the affected set is empty (`--skip`).
#[test]
fn examples_run_eligible_examples_in_selected_packages() {
    if !tools_available() {
        return;
    }
    let workspace = examples_workspace();
    let target = workspace.path().join("target");
    for (scope, arguments, expected) in [
        ("--workspace", &["anvil-examples"][..], &["fixture", "other"][..]),
        ("--package fixture@0.1.0", &["anvil-examples"], &["fixture"]),
        (
            "--package fixture@0.1.0 --package other@0.2.0",
            &["anvil-examples"],
            &["fixture", "other"],
        ),
        ("--skip", &["anvil-examples"], &[]),
        ("--package other@0.2.0", &["anvil-examples", "--package", "fixture"], &["fixture"]),
        ("--skip", &["anvil-examples", "--package", "fixture"], &["fixture"]),
    ] {
        seed_include(workspace.path(), "affected", scope);
        let output = run_just_with_real_cargo_env(
            workspace.path(),
            arguments,
            &[("ANVIL_IMPACT", OsStr::new("consume")), ("CARGO_TARGET_DIR", target.as_os_str())],
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "example run failed for scope {scope:?} and arguments {arguments:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut actual = stdout
            .lines()
            .filter_map(|line| line.strip_prefix("example-ran: "))
            .collect::<Vec<_>>();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "wrong examples executed for scope {scope:?} and arguments {arguments:?}"
        );
    }
}

/// Reporting the excluded example's exit code proves explicit selection ran it despite `no-run`.
#[test]
fn examples_explicit_selection_overrides_no_run() {
    if !tools_available() {
        return;
    }
    let workspace = examples_workspace();
    let explicit = run_just_with_real_cargo(
        workspace.path(),
        &["anvil-examples", "--package", "fixture", "--example", "blocked"],
    );
    assert_failed(&explicit, "explicitly selected excluded example");
    assert!(
        String::from_utf8_lossy(&explicit.stderr).contains("fixture::blocked exited with code 7"),
        "the explicitly selected example must run despite no-run\nstderr:\n{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
}

/// An example exceeding its execution timeout must be terminated and identified in the diagnostic.
#[test]
fn examples_timeout_terminates_example() {
    if !tools_available() {
        return;
    }
    let workspace = examples_workspace();
    // Build separately so a compiler failure is distinct from a timeout failure.
    let build = Command::new("cargo")
        .args([
            "build",
            "--package",
            "fixture",
            "--example",
            "sleeping",
            "--all-features",
            "--locked",
        ])
        .current_dir(workspace.path())
        .output()
        .expect("cargo is required to prepare the example fixture");
    assert!(build.status.success(), "failed to build the sleeping example");

    let timeout = run_just_with_real_cargo(
        workspace.path(),
        &["anvil-examples", "--package", "fixture", "--example", "sleeping", "--timeout", "1"],
    );
    assert_failed(&timeout, "sleeping example timeout");
    // The target has no normal exit path, so completing output capture requires
    // terminating it, independent of interpreter startup or lock queue time.
    assert_eq!(timeout.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&timeout.stderr).contains("fixture::sleeping timed out after 1 seconds"),
        "timeout must identify the selected target\nstderr:\n{}",
        String::from_utf8_lossy(&timeout.stderr)
    );
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
fn doc_test_selects_only_doctest_capable_affected_packages() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("doc-test.just", DOC_TEST), ("impact.just", IMPACT)],
        &["anvil-doc-test-validate-prereqs", "anvil-toolchain-stable-install", "anvil-impact"],
    );
    let log = tmp.path().join("cargo.log");
    seed_include(
        tmp.path(),
        "affected",
        "--package Foo@0.1.0 --package foo@0.1.0 --package macro-package@0.1.0",
    );
    seed_doctest_packages(tmp.path(), &["Foo@0.1.0", "macro-package@0.1.0"]);
    let output = run_just(
        tmp.path(),
        &["anvil-doc-test"],
        &[
            ("ANVIL_IMPACT", OsStr::new("consume")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_PACKAGE_NAME", OsStr::new("Foo")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("foo")),
            ("FAKE_THIRD_PACKAGE_NAME", OsStr::new("macro-package")),
        ],
    );
    assert!(
        output.status.success(),
        "doctest-capable selection failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(&log).unwrap();
    assert_exact_doc_test_commands(&commands, "--workspace --exclude foo");
    assert_eq!(
        commands.lines().filter(|line| line.starts_with("metadata ")).collect::<Vec<_>>(),
        ["metadata --locked --no-deps --format-version 1"],
        "a consumed impact projection needs only the member query that builds the exclude list:\n{commands}"
    );

    fs::remove_file(&log).unwrap();
    seed_include(tmp.path(), "affected", "--package foo@0.1.0");
    seed_doctest_packages(tmp.path(), &[]);
    let bin_only = run_just(
        tmp.path(),
        &["anvil-doc-test"],
        &[("ANVIL_IMPACT", OsStr::new("consume")), ("FAKE_CARGO_LOG", log.as_os_str())],
    );
    assert!(bin_only.status.success(), "bin-only selection must skip cleanly");
    assert!(
        String::from_utf8_lossy(&bin_only.stdout).contains("no affected doctest-capable packages"),
        "bin-only skip must explain why no doctests ran"
    );
    let bin_only_commands = fs::read_to_string(log).unwrap_or_default();
    assert!(
        !bin_only_commands.lines().any(|line| line.contains("test --doc")),
        "a bin-only impact set must not invoke cargo test --doc:\n{bin_only_commands}"
    );
}

#[test]
fn doc_test_workspace_scope_enumerates_only_doctest_capable_members() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("doc-test.just", DOC_TEST), ("impact.just", IMPACT)],
        &["anvil-doc-test-validate-prereqs", "anvil-toolchain-stable-install", "anvil-impact"],
    );
    seed_include(tmp.path(), "affected", "--workspace");
    let log = tmp.path().join("cargo.log");
    let output = run_just(
        tmp.path(),
        &["anvil-doc-test"],
        &[
            ("ANVIL_IMPACT", OsStr::new("off")),
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("FAKE_PACKAGE_NAME", OsStr::new("Foo")),
            ("FAKE_FIRST_RLIB", OsStr::new("1")),
            ("FAKE_SECOND_PACKAGE_NAME", OsStr::new("foo")),
            ("FAKE_SECOND_BIN_ONLY", OsStr::new("1")),
            ("FAKE_SECOND_DOCTEST_FALSE", OsStr::new("1")),
            ("FAKE_THIRD_PACKAGE_NAME", OsStr::new("macro-package")),
            ("FAKE_THIRD_PROC_MACRO", OsStr::new("1")),
            ("FAKE_NON_MEMBER_PACKAGE_NAME", OsStr::new("outside")),
        ],
    );
    assert!(
        output.status.success(),
        "workspace doctest-capability selection failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commands = fs::read_to_string(&log).unwrap();
    assert_exact_doc_test_commands(&commands, "--workspace --exclude foo");
    let metadata_commands = commands.lines().filter(|line| line.starts_with("metadata ")).collect::<Vec<_>>();
    assert_eq!(
        metadata_commands, ["metadata --locked --no-deps --format-version 1"; 2],
        "unscoped runs issue one locked query for capability discovery and one for the exclude list:\n{commands}"
    );
}

#[test]
fn doc_test_locked_capability_discovery_rejects_a_stale_lockfile() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[("doc-test.just", DOC_TEST), ("impact.just", IMPACT)],
        &["anvil-doc-test-validate-prereqs", "anvil-toolchain-stable-install", "anvil-impact"],
    );
    write(
        &tmp.path().join("src/lib.rs"),
        "/// A doctest.\n/// ```\n/// assert!(true);\n/// ```\npub fn documented() {}\n",
    );
    let lock = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(tmp.path())
        .output()
        .expect("cargo is required to prepare the stale-lock fixture");
    assert!(lock.status.success(), "failed to create fixture lockfile");
    write(
        &tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.2.0\"\nrust-version = \"1.97\"\n",
    );

    let output = run_just_with_real_cargo_env(tmp.path(), &["anvil-doc-test"], &[("ANVIL_IMPACT", OsStr::new("off"))]);
    assert_failed(&output, "locked doctest capability discovery with a stale lockfile");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("lock file") || combined.contains("Cargo.lock"),
        "the failure must come from locked metadata discovery:\n{combined}"
    );
}

#[test]
fn external_types_runs_once_per_library_through_cargo_each() {
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
            "anvil-tool-cargo-each-validate-prereqs",
            "anvil-tool-cargo-each-install installer",
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
    // cargo-each selects every library member, publishable or not, and
    // passes the tool each member's manifest path.
    assert_eq!(
        commands.lines().collect::<Vec<_>>(),
        ["each --workspace --filter lib --keep-going -- cargo +nightly-test check-external-types --manifest-path {manifest}"],
        "external-types must run once per library through cargo-each:\n{commands}"
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
    seed_include(tmp.path(), "affected", "--package fixture@0.1.0");
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
    seed_include(tmp.path(), "affected", "--package named-registry@0.1.0");
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
fn coverage_recipe_delegation_contract_is_exact() {
    let normalized = LLVM_COV.replace("\r\n", "\n");
    let start = normalized
        .find("anvil-llvm-cov: anvil-llvm-cov-validate-prereqs anvil-impact\n")
        .expect("coverage recipe declaration");
    let end = normalized.find("# --- pr-test members").expect("coverage recipe footer");
    let actual = &normalized[start..end];
    let expected = r#"anvil-llvm-cov: anvil-llvm-cov-validate-prereqs anvil-impact
    $ErrorActionPreference = 'Stop'
    $include = (& "{{ just_executable() }}" _anvil-impact-include affected)
    $impactExit = $LASTEXITCODE
    if ($impactExit -ne 0) { exit $impactExit }
    if ($include -eq '--skip') {
        Write-Host 'anvil-llvm-cov: no affected packages; skipping'
        exit 0
    }
    # An empty selector list is cargo-coverage-gate's full-workspace mode.
    # Impact-selected values remain version-qualified (`name@version`) so
    # collection resolves the intended workspace member unambiguously.
    $packageArgs = @(if ($include -and $include -ne '--workspace') { -split $include })
    $collectionArgs = @(
        '--configuration', 'all-features',
        '--configuration', 'no-default-features',
        '--coverage-dir', 'target/coverage',
        # LLVM produces unusable coverage profiles on Windows ARM, so
        # coverage-gate runs that target's tests without measuring coverage.
        '--no-coverage-target', 'aarch64-pc-windows-msvc'
    )
    & cargo '+{{ rust_nightly }}' coverage-gate run @packageArgs @collectionArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

"#;
    assert_eq!(actual, expected);
    assert!(!normalized.contains("Invoke-AnvilLcovReport"));
    assert!(!normalized.contains("min-lines-percent"));
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

const MUTANTS_SELECTOR_JUSTFILE: &str = r#"set unstable
set shell := ["anvil-test-no-shell-available"]
set windows-shell := ["anvil-test-no-shell-available"]
import 'helpers.just'
"#;

#[test]
fn mutants_config_selection_is_shell_independent_and_root_relative() {
    if Command::new("just").arg("--version").output().is_err() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workspace with space's");
    let nested = root.join("nested");
    fs::create_dir_all(&nested).unwrap();
    write(&root.join("Justfile"), MUTANTS_SELECTOR_JUSTFILE);
    write(&root.join("helpers.just"), HELPERS);
    write(
        &root.join(".cargo/mutants.toml"),
        "the selector must not parse native configuration",
    );
    let host = Command::new("just")
        .arg("--justfile")
        .arg(root.join("Justfile"))
        .args(["--evaluate", "_anvil_mutants_platform_config"])
        .current_dir(&nested)
        .output()
        .unwrap();
    assert!(host.status.success(), "{}", String::from_utf8_lossy(&host.stderr));
    assert_eq!(
        String::from_utf8_lossy(&host.stdout).trim(),
        format!(".cargo/mutants.{}.toml", std::env::consts::OS)
    );
    for platform in ["windows", "linux", "macos"] {
        let os_config = format!(".cargo/mutants.{platform}.toml");
        write(&nested.join(&os_config), "nested config must not select a workspace config");
        for exists in [false, true] {
            if exists {
                write(&root.join(&os_config), "the selector only tests existence");
            }
            let selected = Command::new("just")
                .arg("--justfile")
                .arg(root.join("Justfile"))
                .arg("--evaluate")
                .arg(format!("_anvil_mutants_platform_config={os_config}"))
                .arg("_anvil_mutants_config_arg")
                .current_dir(&nested)
                .output()
                .unwrap();
            assert!(selected.status.success(), "{}", String::from_utf8_lossy(&selected.stderr));
            assert_eq!(
                String::from_utf8_lossy(&selected.stdout).trim(),
                if exists { format!("--config={os_config}") } else { String::new() },
                "selection must not require a shell or depend on the invocation directory"
            );
        }
        fs::remove_file(root.join(os_config)).unwrap();
    }
}

fn assert_mutants_diff_cleaned(root: &Path) {
    assert!(
        fs::read_dir(root)
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with("anvil-mutants-diff")),
        "mutation diff files must be removed after success or failure"
    );
}

#[test]
fn mutants_diff_reserves_a_unique_file_before_running_git() {
    let allocation = MUTANTS_DIFF
        .split_once("    $diff_path = ")
        .unwrap()
        .1
        .split_once("    # Diff the base")
        .unwrap()
        .0;
    assert_eq!(
        allocation,
        concat!(
            "Join-Path $tmp_dir ('anvil-mutants-diff-' + [Guid]::NewGuid().ToString('N') + '.diff')\n",
            "    New-Item -ItemType File -Path $diff_path | Out-Null\n",
        )
    );
}

#[test]
fn mutants_diff_normalizes_prefixes_and_cleans_up_without_masking_native_failures() {
    let execution = MUTANTS_DIFF
        .split_once("    $exitCode = 0\n")
        .unwrap()
        .1
        .split_once("\n# --- pr-mutants members ---")
        .unwrap()
        .0;
    assert_eq!(
        execution,
        concat!(
            "    try {\n",
            "        git diff --src-prefix=a/ --dst-prefix=b/ \"$base\" --output=$diff_path\n",
            "        $exitCode = $LASTEXITCODE\n",
            "        if ($exitCode -eq 0) {\n",
            "            & cargo {{_anvil_stable_toolchain_args}} mutants --in-diff $diff_path --no-shuffle --jobs 0 {{_anvil_mutants_config_arg}}\n",
            "            $exitCode = $LASTEXITCODE\n",
            "        }\n",
            "    } finally {\n",
            "        try {\n",
            "            Remove-Item -LiteralPath $diff_path\n",
            "        } catch {\n",
            "            if ($exitCode -eq 0) { throw }\n",
            "            Write-Warning \"Could not remove mutation diff '$diff_path': $_ (preserving exit code $exitCode)\"\n",
            "        }\n",
            "    }\n",
            "    exit $exitCode\n",
        )
    );
}

#[test]
fn mutants_recipes_select_only_the_host_config_and_preserve_exit_codes() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(
        &[
            ("helpers.just", HELPERS),
            ("impact.just", IMPACT),
            ("mutants-diff.just", MUTANTS_DIFF),
            ("mutants-full.just", MUTANTS_FULL),
        ],
        &[
            "anvil-tool-cargo-mutants-validate-prereqs",
            "anvil-tool-cargo-mutants-install installer=\"install\"",
            "anvil-impact",
        ],
    );
    let root = tmp.path();
    let log = root.join("cargo.log");
    let os_config = format!(".cargo/mutants.{}.toml", std::env::consts::OS);
    let other = if cfg!(windows) { "linux" } else { "windows" };
    write(&root.join(".cargo/mutants.toml"), "");
    write(&root.join(format!(".cargo/mutants.{other}.toml")), "");
    for exists in [false, true] {
        if exists {
            write(&root.join(&os_config), "");
        }
        for recipe in ["anvil-mutants-diff", "anvil-mutants-full"] {
            for native_exit in ["0", ARBITRARY_FAILURE_EXIT] {
                write(&log, "");
                let output = run_just(
                    root,
                    &[recipe],
                    &[
                        ("FAKE_CARGO_LOG", log.as_os_str()),
                        ("FAKE_CARGO_DEFAULT_EXIT", OsStr::new(native_exit)),
                        ("BASE_REF", OsStr::new("fixture-base")),
                        ("RUNNER_TEMP", root.as_os_str()),
                    ],
                );
                let calls = fs::read_to_string(&log).unwrap();
                if cfg!(windows) && cfg!(target_arch = "aarch64") {
                    assert!(output.status.success());
                    assert!(calls.is_empty(), "Windows ARM must still skip cargo-mutants");
                    continue;
                }
                assert_eq!(
                    output.status.code(),
                    Some(native_exit.parse().unwrap()),
                    "{recipe} must preserve the native exit code: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let mutants = calls.lines().filter(|line| line.starts_with("mutants ")).collect::<Vec<_>>();
                assert_eq!(mutants.len(), 1, "{calls}");
                assert!(mutants[0].contains("--no-shuffle --jobs 0"), "{calls}");
                assert!(
                    mutants[0].contains(if recipe == "anvil-mutants-full" {
                        "--workspace"
                    } else {
                        "--in-diff"
                    }),
                    "{calls}"
                );
                assert_eq!(mutants[0].contains("--config="), exists, "{calls}");
                if exists {
                    assert!(mutants[0].ends_with(&format!("--config={os_config}")), "{calls}");
                }
                assert_mutants_diff_cleaned(root);
            }
        }
    }
    seed_include(root, "affected", "--skip");
    write(&log, "");
    let skipped = run_just(root, &["anvil-mutants-diff"], &[("FAKE_CARGO_LOG", log.as_os_str())]);
    assert!(skipped.status.success(), "{}", String::from_utf8_lossy(&skipped.stderr));
    assert!(fs::read_to_string(log).unwrap().is_empty(), "an empty impact scope must still skip");
}

#[test]
fn mutants_native_platform_config_replaces_common_config_without_merging() {
    if Command::new("just").arg("--version").output().is_err()
        || !Command::new("cargo")
            .args(["mutants", "--version"])
            .output()
            .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping native config integration: just or cargo-mutants is unavailable");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(&root.join("Justfile"), MUTANTS_SELECTOR_JUSTFILE);
    write(&root.join("helpers.just"), HELPERS);
    write(
        &root.join("Cargo.toml"),
        "[package]\nname = \"native-config-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
    );
    write(&root.join("src/lib.rs"), "pub mod common;\npub mod platform;\n");
    write(&root.join("src/common.rs"), "pub fn common_marker() -> bool { true }\n");
    write(&root.join("src/platform.rs"), "pub fn platform_marker() -> bool { true }\n");
    write(&root.join(".cargo/mutants.toml"), "exclude_globs = [\"src/common.rs\"]\n");
    let os_config = format!(".cargo/mutants.{}.toml", std::env::consts::OS);
    for (config, common_included, platform_included) in [
        (None, false, true),
        (Some("exclude_globs = [\"src/platform.rs\"]\n"), true, false),
        (Some("exclude_globs = [\"src/common.rs\", \"src/platform.rs\"]\n"), false, false),
        (Some("invalid TOML {"), false, false),
    ] {
        if let Some(config) = config {
            write(&root.join(&os_config), config);
        }
        let selected = Command::new("just")
            .args(["--justfile", "Justfile", "--evaluate", "_anvil_mutants_config_arg"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(selected.status.success(), "{}", String::from_utf8_lossy(&selected.stderr));
        let argument = String::from_utf8(selected.stdout).unwrap();
        let mut command = Command::new("cargo");
        command
            .args(["mutants", "--list", "--workspace", "--no-shuffle"])
            .env("CARGO_NET_OFFLINE", "true")
            .current_dir(root);
        if !argument.trim().is_empty() {
            command.arg(argument.trim());
        }
        let output = command.output().unwrap();
        if config == Some("invalid TOML {") {
            assert_failed(&output, "an invalid selected config must not fall back to the common config");
        } else {
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let listing = String::from_utf8_lossy(&output.stdout);
            assert_eq!(listing.contains("common_marker"), common_included, "{listing}");
            assert_eq!(listing.contains("platform_marker"), platform_included, "{listing}");
        }
    }
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
    let captured = root.join("captured.diff");
    let output = run_just(
        root,
        &["anvil-mutants-diff"],
        &[
            ("FAKE_CARGO_LOG", log.as_os_str()),
            ("BASE_REF", OsStr::new(&base)),
            ("RUNNER_TEMP", root.as_os_str()),
            ("FAKE_MUTANTS_DIFF_CAPTURE", captured.as_os_str()),
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

    assert_mutants_diff_cleaned(root);
    let diff = std::fs::read_to_string(captured).unwrap();
    assert!(diff.contains("--- a/src/lib.rs\n"), "{diff}");
    assert!(diff.contains("+++ b/src/lib.rs\n"), "{diff}");
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
    stub_msrv_resolver(root);
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

/// The image installs the toolchain named by the repository's declared MSRV, so
/// raising it changes what the image contains and must rename it. The digest
/// takes the resolved value rather than the manifest declaring it: dependency
/// edits touch that file constantly while `rust-version` moves perhaps once.
#[test]
fn the_image_tag_follows_the_declared_msrv() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let root = tmp.path();
    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    write(&root.join(".anvil/container/Dockerfile"), "FROM scratch\n");
    write(&root.join(".anvil/container/Dockerfile.dockerignore"), "*\n!justfiles\n");
    write(&root.join("justfiles/anvil/mod.just"), "# recipes\n");
    stub_msrv_resolver(root);
    write(&root.join("fake-bin/git.ps1"), "exit 0\n");

    let tag = |msrv: &str| {
        let output = run_just(root, &["anvil-container-tag"], &[("FAKE_ROOT_MSRV", OsStr::new(msrv))]);
        assert!(
            output.status.success(),
            "computing the tag failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    let declared = tag("1.93.1");
    assert_ne!(
        declared,
        tag("1.94.0"),
        "an MSRV bump installs a different toolchain, so it must rename the image"
    );
    assert_ne!(
        declared,
        tag("none"),
        "a repository that declares no MSRV gets an image with no MSRV toolchain in it"
    );
    assert_eq!(declared, tag("1.93.1"), "the tag must depend on the inputs alone");
}

/// The tag must answer for a repository that owns no toolchain file rather than
/// refusing it, and must not hand it the same reference as one that owns one.
#[test]
fn the_image_tag_treats_a_root_toolchain_file_as_optional() {
    if !tools_available() {
        return;
    }
    let tmp = fixture(&[("container.just", CONTAINER)], &[]);
    let root = tmp.path();
    write(&root.join(".anvil/container/Dockerfile"), "FROM scratch\n");
    write(&root.join(".anvil/container/Dockerfile.dockerignore"), "*\n!justfiles\n");
    write(&root.join("justfiles/anvil/mod.just"), "# recipes\n");
    stub_msrv_resolver(root);
    write(&root.join("fake-bin/git.ps1"), "exit 0\n");

    let tag = || {
        let output = run_just(root, &["anvil-container-tag"], &[]);
        assert!(
            output.status.success(),
            "computing the tag failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    let none = tag();

    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.90\"\n");
    let toml = tag();
    assert_ne!(
        none, toml,
        "owning a toolchain file changes the image's compiler, so it must rename it"
    );

    write(&root.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.91\"\n");
    assert_ne!(
        toml,
        tag(),
        "the image installs the toolchain the file selects, so an edit must rename it"
    );

    fs::remove_file(root.join("rust-toolchain.toml")).unwrap();
    write(&root.join("rust-toolchain"), "[toolchain]\nchannel = \"1.90\"\n");
    let extensionless = tag();
    assert_ne!(none, extensionless, "the extensionless spelling is an image input too");
    assert_ne!(toml, extensionless, "the same bytes under the other spelling are a different input");
}

/// The ignore file re-includes either toolchain path, so a directory at one is
/// copied into the image whole, while the digest walks only `.anvil/container/`
/// and `justfiles/anvil/` and hashes nothing inside it. Discovery skips a
/// non-leaf, which would leave two images differing anywhere under that
/// directory sharing one tag, and the link guard does not fire because a plain
/// directory is not a reparse point.
#[test]
fn a_directory_at_a_toolchain_path_is_refused() {
    if !tools_available() {
        return;
    }
    for spelling in ["rust-toolchain", "rust-toolchain.toml"] {
        let tmp = fixture(&[("container.just", CONTAINER)], &[]);
        let root = tmp.path();
        write(&root.join(".anvil/container/Dockerfile"), "FROM scratch\n");
        write(&root.join(".anvil/container/Dockerfile.dockerignore"), "*\n!justfiles\n");
        write(&root.join("justfiles/anvil/mod.just"), "# recipes\n");
        stub_msrv_resolver(root);
        write(&root.join(spelling).join("payload.txt"), "A\n");

        let output = run_just(root, &["anvil-container-tag"], &[]);
        assert_failed(&output, &format!("computing a tag with a directory at {spelling}"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(spelling) && stderr.contains("regular file"),
            "the refusal must name the path and what it must be\nstderr:\n{stderr}"
        );
    }
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
    stub_msrv_resolver(root);
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
        ("a linked extensionless toolchain file", "rust-toolchain", false),
        ("a linked root manifest", "Cargo.toml", false),
        ("a linked recipe walk root", "justfiles/anvil", true),
        ("a linked container walk root", ".anvil/container", true),
    ] {
        let tmp = fixture(&[("container.just", CONTAINER)], &[]);
        let root = tmp.path();
        stub_msrv_resolver(root);
        write(&root.join("elsewhere/target.just"), "# shared\n");
        write(&root.join("elsewhere/Dockerfile"), "FROM scratch\n");
        // The fixture writes a root manifest of its own, which the link for
        // that case has to replace rather than sit beside.
        if name == "Cargo.toml" {
            fs::remove_file(root.join(name)).unwrap();
        }
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
