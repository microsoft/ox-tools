# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# End-to-end check for cargo-anvil's containerized execution.
#
# This walks the experience the README promises, and nothing else:
#
#   1. clean up           -- remove anything left by a previous run
#   2. a repository       -- a small crate, plus an anvil.toml enabling containers
#   3. generate           -- cargo anvil
#   4. run a recipe       -- no image exists, so one is built, and the recipe
#                            runs inside it
#   5. run it again       -- the image is reused; nothing is rebuilt
#   6. bump the toolchain -- the image no longer matches, so a new one is built
#   7. clean up
#
# Steps 4 and 6 are the claims worth testing. Nothing here edits a generated
# file: how the image identity is computed is an implementation detail, covered
# by the Rust test suite, and a user never touches those files.
#
# cargo-anvil is a generator -- it writes files and exits -- so nothing in the
# Rust test suite executes the PowerShell or the Dockerfile it emits. This
# script is what covers them.
#
# Requirements: `cargo`, `just` and `git` on PATH, plus a container engine.
# On Windows the engine is often installed inside WSL rather than on the host.
# The script detects that and runs the generated recipes there, so a split
# setup (cargo on Windows, Docker in WSL) works as well as a uniform one.
#
# The first run builds a toolchain image and takes several minutes.

param(
    # Where to create the throwaway repository. Must be visible to the
    # container engine, so keep it on a local drive.
    [Parameter(Mandatory = $false)]
    [string]$WorkspacePath = (Join-Path ([System.IO.Path]::GetTempPath()) "anvil-container-e2e"),

    # The recipe to run. Must be a tier or a group: those are the entry points
    # anvil containerizes, and running one is the whole point of the exercise.
    #
    # `anvil-pr-test` is the default because it is self-contained -- it
    # compiles the crate and runs its tests, and needs no network, credentials
    # or repository policy. A full gate like `anvil-pr-fast` also works, but
    # only against a repository that can satisfy all of it: its licence and
    # advisory checks want real dependencies, and its PR checks want a PR.
    [Parameter(Mandatory = $false)]
    [string]$Recipe = 'anvil-pr-test',

    # Stop after generation. Checks that a repository configuring nothing but
    # `enabled = true` gets a working setup, without paying for a build.
    [Parameter(Mandatory = $false)]
    [switch]$SkipBuild,

    # Leave the workspace and images in place for inspection.
    [Parameter(Mandatory = $false)]
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
# Native command failures are handled explicitly below; do not fail fast.
$PSNativeCommandUseErrorActionPreference = $false

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$repoName = 'anvil-container-e2e'
$imageName = "anvil-$repoName"
$script:failures = @()
$script:checks = 0

function Write-Step([string]$Message) {
    Write-Host ''
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Assert-That([object]$Condition, [string]$Message) {
    $script:checks++
    # Coerce defensively: a PowerShell comparison against an array yields an
    # array, and silently treating that as truthy would hide a failure.
    $passed = ($Condition -is [bool]) ? $Condition : [bool]$Condition
    if ($passed) {
        Write-Host "  [ok]   $Message"
    }
    else {
        Write-Host "  [FAIL] $Message"
        $script:failures += $Message
    }
}

# `just` and the container engine must see the same filesystem, so on a Windows
# host whose engine lives in WSL the recipes run inside WSL against the /mnt
# path rather than through a Windows `docker` that is not there.
$script:engineMode = 'native'
$script:wslWorkspace = $null

function Initialize-EngineMode {
    if (Get-Command 'docker' -ErrorAction SilentlyContinue) {
        & docker version *> $null
        if ($LASTEXITCODE -eq 0) { $script:engineMode = 'native'; return }
    }
    if ($IsWindows -and (Get-Command 'wsl' -ErrorAction SilentlyContinue)) {
        & wsl -e docker version *> $null
        if ($LASTEXITCODE -eq 0) {
            $script:engineMode = 'wsl'
            Write-Host '  container engine found inside WSL; recipes will run there'
            return
        }
    }
    throw 'no reachable container engine (looked for docker on PATH, then inside WSL)'
}

function Get-WslWorkspace {
    if (-not $script:wslWorkspace) {
        $script:wslWorkspace = (& wsl -e wslpath -a $WorkspacePath.Replace('\', '/')).Trim()
    }
    return $script:wslWorkspace
}

function Invoke-Engine {
    param([string[]]$Arguments)
    if ($script:engineMode -eq 'wsl') { return & wsl -e docker @Arguments 2>$null }
    return & docker @Arguments 2>$null
}

function Invoke-Just {
    # Run a recipe the way a developer would, capturing stdout and stderr
    # together: the build notice and the check output both matter here.
    # Output is teed to the console as it arrives -- the first build takes
    # minutes, and a silent script looks like a hung one.
    param([string]$RecipeName)

    $captured = $null
    if ($script:engineMode -eq 'wsl') {
        & wsl -e bash -c "cd '$(Get-WslWorkspace)' && just $RecipeName 2>&1" |
            Tee-Object -Variable captured |
            ForEach-Object { Write-Host "    $_" }
    }
    else {
        Push-Location $WorkspacePath
        try {
            & just $RecipeName 2>&1 |
                Tee-Object -Variable captured |
                ForEach-Object { Write-Host "    $_" }
        }
        finally { Pop-Location }
    }
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = (@($captured) -join "`n") }
}

function Get-BuiltImages {
    return @(Invoke-Engine -Arguments @('images', $imageName, '--format', '{{.Repository}}:{{.Tag}}')) |
        Where-Object { $_ }
}

function Remove-TestImages {
    $ids = @(Invoke-Engine -Arguments @('images', $imageName, '--quiet')) | Where-Object { $_ }
    if ($ids) { Invoke-Engine -Arguments (@('image', 'rm', '--force') + $ids) | Out-Null }
    foreach ($volume in @('cargo', 'rustup')) {
        Invoke-Engine -Arguments @('volume', 'rm', '--force', "$repoName-$volume") | Out-Null
    }
}

# ---------------------------------------------------------------------------
# 1. Clean up
# ---------------------------------------------------------------------------

Write-Step 'Cleaning up anything left by a previous run'

foreach ($tool in @('cargo', 'just', 'git')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool is required but was not found on PATH"
    }
}
Initialize-EngineMode

if (Test-Path -LiteralPath $WorkspacePath) {
    Remove-Item -LiteralPath $WorkspacePath -Recurse -Force
    Write-Host "  removed $WorkspacePath"
}
Remove-TestImages
Assert-That ((Get-BuiltImages).Count -eq 0) 'starting with no image, as a new adopter would'

# ---------------------------------------------------------------------------
# 2. A repository that wants containerized checks
# ---------------------------------------------------------------------------

Write-Step 'Creating a small repository'

& cargo build --quiet --package cargo-anvil --manifest-path (Join-Path $repoRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'failed to build cargo-anvil' }
$exe = Join-Path $repoRoot 'target/debug/cargo-anvil'
if (-not (Test-Path -LiteralPath $exe)) { $exe = "$exe.exe" }
if (-not (Test-Path -LiteralPath $exe)) { throw 'cargo-anvil binary not found after build' }

New-Item -ItemType Directory -Path (Join-Path $WorkspacePath 'src') -Force | Out-Null

function Write-RepoFile([string]$RelativePath, [string]$Content) {
    # Always LF, always a trailing newline: the checks run on Linux, and
    # rustfmt and the license-header check both object to a missing one.
    $normalized = ($Content -replace "`r`n", "`n")
    if (-not $normalized.EndsWith("`n")) { $normalized += "`n" }
    [System.IO.File]::WriteAllText((Join-Path $WorkspacePath $RelativePath), $normalized)
}

$license = "# Copyright (c) Microsoft Corporation.`n# Licensed under the MIT License.`n`n"

Write-RepoFile 'Cargo.toml' ($license + @'
[package]
name = "anvil-container-e2e"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
'@)

Write-RepoFile 'src/lib.rs' @'
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A deliberately trivial crate. It gives the checks something real to compile
//! and run; it is not the subject of the test.

/// Adds two numbers.
#[must_use]
pub const fn add(left: i32, right: i32) -> i32 {
    left + right
}

#[cfg(test)]
mod tests {
    use super::add;

    #[test]
    fn adds() {
        assert_eq!(add(2, 2), 4);
    }
}
'@

# Files anvil does not own outright: it writes a managed region into each, and
# the repository owns the rest -- including the licence header the header check
# requires. A real adopter has these; creating them empty is what makes the
# generated regions land in a conformant file.
foreach ($policyFile in @('clippy.toml', 'deny.toml', 'rustfmt.toml', 'spellcheck.toml')) {
    Write-RepoFile $policyFile $license.TrimEnd()
}

# The spellcheck dictionary is repository vocabulary, so it is ours to provide.
Write-RepoFile '.spelling' @'
anvil
'@

# Read by the license-header check. Repository policy, not anvil's to generate.
Write-RepoFile '.cargo-heather.toml' @'
header = """
Copyright (c) Microsoft Corporation.
Licensed under the MIT License.
"""
'@

# Track this repository's pinned toolchain, so the image installs a catalog
# whose tools are known to agree with each other.
Write-RepoFile 'rust-toolchain.toml' (Get-Content (Join-Path $repoRoot 'rust-toolchain.toml') -Raw)

# anvil owns the `anvil-*` recipes; the root Justfile only imports them.
Write-RepoFile 'Justfile' ($license + "import 'justfiles/anvil/mod.just'`n")

# The entire opt-in. No image, no Dockerfile, no registry: this is the case
# the README describes, and the one that has to work out of the box.
Write-RepoFile 'anvil.toml' ($license + @'
[container]
enabled = true
# Pins the repo identity so cache-volume names do not follow the directory
# name, which differs between checkouts.
name = "anvil-container-e2e"
'@)

Push-Location $WorkspacePath
try {
    # anvil resolves the repository root through git, as it would in a clone.
    & git init --quiet
    Write-Step 'Generating the anvil recipes'
    & $exe anvil --no-backends | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'cargo anvil failed' }
    & cargo generate-lockfile --quiet
}
finally {
    Pop-Location
}

Assert-That (Test-Path -LiteralPath (Join-Path $WorkspacePath 'justfiles/anvil/container.just')) `
    'the container recipes were generated'
Assert-That (Test-Path -LiteralPath (Join-Path $WorkspacePath '.anvil/container/Dockerfile')) `
    'a Dockerfile was generated, so no image has to exist anywhere first'
Assert-That (Test-Path -LiteralPath (Join-Path $WorkspacePath '.anvil/container/Dockerfile.dockerignore')) `
    'the build-context filter sits beside the Dockerfile, where BuildKit looks for it'

# `justfiles/` carries just recipes and nothing else; container assets live
# under `.anvil/`. Asserted here because only a real generation run can catch a
# new artifact being emitted into the wrong tree.
$strays = @(
    Get-ChildItem -LiteralPath (Join-Path $WorkspacePath 'justfiles') -Recurse -File |
        Where-Object { $_.Extension -ne '.just' }
)
Assert-That ($strays.Count -eq 0) `
    ("justfiles/ contains only .just files (found: {0})" -f (($strays | ForEach-Object { $_.Name }) -join ', '))

if ($SkipBuild) {
    Write-Step 'Stopping before the build (-SkipBuild)'
}
else {

    # -----------------------------------------------------------------------
    # 3. Run a recipe. No image exists yet.
    # -----------------------------------------------------------------------

    Write-Step "Running '$Recipe' with no image present (expect a build, then the checks)"
    Write-Host '  the first build takes several minutes'

    $cold = Invoke-Just -RecipeName $Recipe

    Assert-That ($cold.Output -match 'anvil: building') `
        'the missing image was built automatically, without being asked for'
    $images = Get-BuiltImages
    Assert-That ($images.Count -eq 1) "exactly one image now exists ($($images -join ', '))"
    Assert-That ($cold.Output -match "/workspaces/$repoName") `
        'the checks ran inside the container, against the mounted repository'
    Assert-That ($cold.ExitCode -eq 0) "'$Recipe' passed"

    # -----------------------------------------------------------------------
    # 4. Run it again. The image is current, so nothing should be rebuilt.
    # -----------------------------------------------------------------------

    Write-Step "Running '$Recipe' again (expect no build)"

    $warmStart = Get-Date
    $warm = Invoke-Just -RecipeName $Recipe
    $warmSeconds = ((Get-Date) - $warmStart).TotalSeconds

    Assert-That (-not ($warm.Output -match 'anvil: building')) 'the existing image was reused'
    Assert-That ((Get-BuiltImages).Count -eq 1) 'no second image was created'
    Assert-That ($warm.ExitCode -eq 0) "'$Recipe' passed again"
    Write-Host ("  (took {0:N0}s)" -f $warmSeconds)

    # -----------------------------------------------------------------------
    # 5. Change the toolchain. The image no longer matches the repository.
    # -----------------------------------------------------------------------

    Write-Step 'Bumping the toolchain pin (expect a rebuild on the next run)'

    $toolchainPath = Join-Path $WorkspacePath 'rust-toolchain.toml'
    $toolchain = [System.IO.File]::ReadAllText($toolchainPath)
    # A comment is enough: the point is that the file the image was built from
    # is no longer the file in the repository.
    [System.IO.File]::WriteAllText($toolchainPath, $toolchain + "`n# toolchain review pending`n")

    $stale = Invoke-Just -RecipeName $Recipe

    Assert-That ($stale.Output -match 'anvil: building') `
        'the stale image was not reused; a new one was built'
    $images = Get-BuiltImages
    Assert-That ($images.Count -eq 2) `
        "the previous image is still there for other branches ($($images.Count) present)"
    Assert-That ($stale.ExitCode -eq 0) "'$Recipe' passed against the rebuilt image"
}

# ---------------------------------------------------------------------------
# 6. Clean up
# ---------------------------------------------------------------------------

Write-Step 'Cleaning up'

if ($Keep) {
    Write-Host "  keeping $WorkspacePath and the built images (-Keep)"
}
else {
    Remove-Item -LiteralPath $WorkspacePath -Recurse -Force -ErrorAction SilentlyContinue
    Remove-TestImages
    Write-Host '  removed the workspace, images and cache volumes'
}

Write-Host ''
if ($script:failures.Count -gt 0) {
    Write-Host "$($script:failures.Count) of $($script:checks) checks FAILED:" -ForegroundColor Red
    foreach ($failure in $script:failures) { Write-Host "  - $failure" }
    exit 1
}

Write-Host "all $($script:checks) checks passed" -ForegroundColor Green
exit 0
