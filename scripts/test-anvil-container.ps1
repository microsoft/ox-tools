# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
    End-to-end test of cargo-anvil's containerized execution, from a user's seat.

.DESCRIPTION
    Creates a throwaway repository in a temp directory, generates the anvil tree
    into it with the locally-built cargo-anvil, and then does only what a
    developer would do: run `just anvil-container <command…>` and observe what
    happens.

    The setup phase is held to that standard deliberately. If this script has to
    hand-write a file anvil should have generated, patch a generated file, or
    work around a defect to get green, that is a bug in the product and not
    something the script should paper over.

    What it proves:

      1. A generated repository carries exactly the three container artifacts.
      2. The first run builds an image and runs the recipe inside it.
      3. A second run reuses the image (the tag resolves, nothing is built),
         no cache volume masks the tools the image installed, and a host
         GITHUB_TOKEN is forwarded only with explicit opt-in from the environment.
      3b. A recipe run from a linked worktree can still reach git history.
      4. Changing a hashed input (the pinned toolchain) selects a new tag.
      5. Reverting that input returns to the original tag.
      6. Editing the Dockerfile is preserved by a re-run of the generator.
      7. A credential hook reaches both the build and the run, and the secret
         reaches neither a build command nor the image filesystem.
      8. A hook returning an empty value fails closed.
      9. A hook's returned value does not change the tag; its file content does.
     10. The recipes run natively inside the image (no nesting).

.PARAMETER Engine
    Container engine to test against. Defaults to $env:ANVIL_CONTAINER_ENGINE,
    then 'docker'.

.PARAMETER KeepArtifacts
    Leave the temp repository and built images in place for inspection.

.PARAMETER SkipCleanup
    Skip the pre-run cleanup of images and volumes left by earlier runs.

.EXAMPLE
    ./scripts/test-anvil-container.ps1
    ./scripts/test-anvil-container.ps1 -Engine podman -KeepArtifacts
#>

[CmdletBinding()]
param(
    [ValidateSet('docker', 'podman')]
    [string]$Engine = $(if ($env:ANVIL_CONTAINER_ENGINE) { $env:ANVIL_CONTAINER_ENGINE } else { 'docker' }),
    [switch]$KeepArtifacts,
    [switch]$SkipCleanup
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ---------------------------------------------------------------- reporting --

$script:Passed = 0
$script:Failed = 0
$script:Started = Get-Date

function Write-Section([string]$Title) {
    Write-Host ''
    Write-Host "=== $Title " -NoNewline -ForegroundColor Cyan
    Write-Host ('=' * [Math]::Max(0, 72 - $Title.Length)) -ForegroundColor Cyan
}

function Write-Step([string]$Message) {
    Write-Host "  -> $Message" -ForegroundColor DarkGray
}

function Write-Detail([string]$Message) {
    foreach ($line in ($Message -split "`r?`n")) {
        if ($line.Trim()) { Write-Host "     | $line" -ForegroundColor DarkGray }
    }
}

function Assert-That([string]$Name, [bool]$Condition, [string]$Detail = '') {
    if ($Condition) {
        $script:Passed++
        Write-Host "  [PASS] $Name" -ForegroundColor Green
    } else {
        $script:Failed++
        Write-Host "  [FAIL] $Name" -ForegroundColor Red
        if ($Detail) { Write-Detail $Detail }
    }
}

function Assert-Equal([string]$Name, $Expected, $Actual) {
    Assert-That $Name ($Expected -eq $Actual) "expected: $Expected`nactual:   $Actual"
}

# ------------------------------------------------------------------ helpers --

function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$Command,
        [string[]]$Arguments = @(),
        [string]$WorkingDirectory,
        [hashtable]$Environment = @{},
        [switch]$AllowFailure
    )

    $start = [Diagnostics.ProcessStartInfo]::new($Command)
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    if ($WorkingDirectory) { $start.WorkingDirectory = $WorkingDirectory }
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    foreach ($key in $Environment.Keys) { $start.Environment[$key] = $Environment[$key] }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw "could not start $Command" }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $result = [pscustomobject]@{
            ExitCode = $process.ExitCode
            StdOut = $stdout.GetAwaiter().GetResult()
            StdErr = $stderr.GetAwaiter().GetResult()
        }
    } finally {
        $process.Dispose()
    }

    if (-not $AllowFailure -and $result.ExitCode -ne 0) {
        Write-Detail $result.StdOut
        Write-Detail $result.StdErr
        throw "$Command $($Arguments -join ' ') failed with exit code $($result.ExitCode)"
    }
    $result
}

function Resolve-Engine {
    # Mirrors what container.just does: prefer the engine on PATH, and fall
    # back to the default WSL distribution on Windows. The script must not
    # assume more than the product does.
    if (Get-Command $Engine -ErrorAction SilentlyContinue) {
        return [pscustomobject]@{ Exe = $Engine; Prefix = @(); ViaWsl = $false }
    }
    if ($IsWindows -and (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        & wsl.exe --exec $Engine --version *> $null
        if ($LASTEXITCODE -eq 0) {
            return [pscustomobject]@{ Exe = 'wsl.exe'; Prefix = @('--exec', $Engine); ViaWsl = $true }
        }
    }
    $null
}

function Invoke-Engine {
    param([string[]]$Arguments, [switch]$AllowFailure)
    Invoke-Native -Command $script:EngineExe -Arguments ($script:EnginePrefix + $Arguments) -AllowFailure:$AllowFailure
}

function ConvertTo-EnginePath([string]$Path) {
    if (-not $script:EngineViaWsl) { return $Path }
    # --exec, not --: plain `wsl.exe --` re-parses through the login shell,
    # which eats `$` in a path and still exits 0.
    (& wsl.exe --exec wslpath -a -u $Path).Trim()
}

function Write-Fixture([string]$Path, [string]$Content) {
    # LF, no BOM. This script is a CRLF file, so its here-strings carry CRLF;
    # writing those verbatim would hand the fixture a repository that
    # `anvil-fmt` correctly rejects for its newline style. A user cloning a
    # normal repository does not start from that state, so neither should we.
    $normalized = ($Content -replace "`r`n", "`n")
    if (-not $normalized.EndsWith("`n")) { $normalized += "`n" }
    $directory = Split-Path -Parent $Path
    if ($directory -and -not (Test-Path -LiteralPath $directory)) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    [System.IO.File]::WriteAllText($Path, $normalized, [System.Text.UTF8Encoding]::new($false))
}

function Invoke-Just {
    param(
        [Parameter(Mandatory)][string]$Repo,
        [Parameter(Mandatory)][string[]]$Arguments,
        [hashtable]$Environment = @{},
        [switch]$AllowFailure
    )
    $env = @{ ANVIL_CONTAINER_ENGINE = $Engine } + $Environment
    Invoke-Native -Command 'just' -Arguments $Arguments -WorkingDirectory $Repo -Environment $env -AllowFailure:$AllowFailure
}

function Get-ImageReference {
    param([Parameter(Mandatory)][string]$Repo, [hashtable]$Environment = @{})
    # anvil-container-status reports the reference without building it.
    $status = Invoke-Just -Repo $Repo -Arguments @('anvil-container-status') -Environment $Environment -AllowFailure
    $line = ($status.StdOut -split "`r?`n") | Where-Object { $_ -match '^\s*image:\s*(\S+)' } | Select-Object -First 1
    if ($line -match '^\s*image:\s*(\S+)') { return $Matches[1] }
    ''
}

function Test-ImagePresent([string]$Reference) {
    if (-not $Reference) { return $false }
    (Invoke-Engine -Arguments @('image', 'inspect', $Reference) -AllowFailure).ExitCode -eq 0
}

function Remove-AnvilImages([string]$Prefix) {
    $images = Invoke-Engine -Arguments @('images', '--format', '{{.Repository}}:{{.Tag}}') -AllowFailure
    # Podman reports images fully qualified (`localhost/anvil-…`), docker does
    # not, so match anywhere in the reference rather than at the start.
    $matching = ($images.StdOut -split "`r?`n") | Where-Object { $_ -like "*$Prefix*" }
    foreach ($image in $matching) {
        Write-Step "removing image $image"
        Invoke-Engine -Arguments @('rmi', '-f', $image) -AllowFailure | Out-Null
    }
    $volumes = Invoke-Engine -Arguments @('volume', 'ls', '--format', '{{.Name}}') -AllowFailure
    $matchingVolumes = ($volumes.StdOut -split "`r?`n") | Where-Object { $_ -like "*$Prefix*" }
    foreach ($volume in $matchingVolumes) {
        Write-Step "removing volume $volume"
        Invoke-Engine -Arguments @('volume', 'rm', '-f', $volume) -AllowFailure | Out-Null
    }
}

# ------------------------------------------------------------ prerequisites --

Write-Section 'Prerequisites'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Write-Step "source repository: $repoRoot"
Write-Step "engine: $Engine"

foreach ($tool in @('just', 'cargo')) {
    $found = Get-Command $tool -ErrorAction SilentlyContinue
    Assert-That "$tool is on PATH" ([bool]$found) "install $tool"
}

$resolved = Resolve-Engine
Assert-That "$Engine is reachable" ($null -ne $resolved) `
    "install $Engine so it is callable from this shell, or run it in the default WSL distribution"
if ($script:Failed -gt 0) {
    Write-Host "`nPrerequisites missing; aborting." -ForegroundColor Red
    exit 1
}
$script:EngineExe = $resolved.Exe
$script:EnginePrefix = $resolved.Prefix
$script:EngineViaWsl = $resolved.ViaWsl
if ($resolved.ViaWsl) {
    Write-Step "engine reached through the default WSL distribution (no Windows CLI on PATH)"
}

$engineInfo = Invoke-Engine -Arguments @('version', '--format', '{{.Server.Version}}') -AllowFailure
if ($engineInfo.ExitCode -eq 0) {
    Write-Step "engine server version: $($engineInfo.StdOut.Trim())"
} else {
    Write-Host "  [FAIL] $Engine is installed but its daemon is not reachable" -ForegroundColor Red
    Write-Detail $engineInfo.StdErr
    exit 1
}

# The tool under test is the one in this worktree, not whatever is installed.
Write-Step 'building cargo-anvil from this worktree'
Invoke-Native -Command 'cargo' -Arguments @('build', '-q', '-p', 'cargo-anvil') -WorkingDirectory $repoRoot | Out-Null
$anvilExe = Join-Path $repoRoot 'target/debug/cargo-anvil.exe'
if (-not (Test-Path -LiteralPath $anvilExe)) { $anvilExe = Join-Path $repoRoot 'target/debug/cargo-anvil' }
Assert-That 'cargo-anvil built' (Test-Path -LiteralPath $anvilExe)

# ---------------------------------------------------------------- the repo ---

Write-Section 'Fixture repository'

# A stable directory name keeps the image name stable across runs, which is what
# makes the pre-run cleanup below able to find leftovers.
$fixtureName = 'anvil-e2e'
$workRoot = Join-Path ([System.IO.Path]::GetTempPath()) 'anvil-container-e2e'
$repo = Join-Path $workRoot $fixtureName
$imagePrefix = "anvil-$fixtureName"

if (-not $SkipCleanup) {
    Write-Step 'pre-run cleanup'
    if (Test-Path -LiteralPath $workRoot) {
        Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
    Remove-AnvilImages -Prefix $imagePrefix
}

New-Item -ItemType Directory -Path $repo -Force | Out-Null
Write-Step "fixture: $repo"

# Everything below is what a user would author by hand in a new repository.
Write-Fixture (Join-Path $repo 'Cargo.toml') @'
[package]
name = "anvil-e2e"
version = "0.1.0"
edition = "2021"

[dependencies]

[workspace]
members = ["."]
'@
New-Item -ItemType Directory -Path (Join-Path $repo 'src') -Force | Out-Null
Write-Fixture (Join-Path $repo 'src/lib.rs') @'
//! A fixture crate for the container end-to-end test.

/// Adds two numbers.
#[must_use]
pub const fn add(left: u64, right: u64) -> u64 {
    left + right
}
'@
Write-Fixture (Join-Path $repo 'rust-toolchain.toml') @'
[toolchain]
channel = "1.99.1"
'@
Write-Fixture (Join-Path $repo 'Justfile') @'
set unstable

# A repository-owned recipe, to prove that forwarded values arrive.
e2e-show-env:
    @echo "E2E:$ANVIL_E2E_RUNTIME"

# Proves the driver forwards only an explicitly opted-in synthetic host token.
# `:-` because just runs recipe lines under `sh -u`, where a bare $NAME that
# was correctly *not* forwarded would abort instead of printing empty.
e2e-show-token:
    @echo "E2E-TOKEN:[${GITHUB_TOKEN:-}]"

# Proves git resolves inside the container, which a linked worktree breaks
# unless the driver mounts the common git directory.
e2e-show-git:
    @echo "E2E-GIT:[$(git rev-parse --abbrev-ref HEAD)]"
'@

Invoke-Native -Command 'git' -Arguments @('init', '-q') -WorkingDirectory $repo | Out-Null
# Pin the newline policy: the fixture writes LF, and a developer with
# core.autocrlf=true globally would otherwise fail to stage it.
Invoke-Native -Command 'git' -Arguments @('config', 'core.autocrlf', 'false') -WorkingDirectory $repo | Out-Null

Write-Step 'generating the anvil tree (cargo anvil --no-backends)'
$generate = Invoke-Native -Command $anvilExe -Arguments @('anvil', '--no-backends') -WorkingDirectory $repo
Write-Detail (($generate.StdOut -split "`r?`n" | Select-Object -Last 3) -join "`n")

# ------------------------------------------------------- 1. what was emitted --

Write-Section '1. Generated artifacts'

$dockerfile = Join-Path $repo '.anvil/container/Dockerfile'
$dockerignore = Join-Path $repo '.anvil/container/Dockerfile.dockerignore'
$containerJust = Join-Path $repo 'justfiles/anvil/container.just'
$hooks = Join-Path $repo '.anvil/container/hooks.ps1'

Assert-That 'Dockerfile emitted' (Test-Path -LiteralPath $dockerfile)
Assert-That 'Dockerfile.dockerignore emitted' (Test-Path -LiteralPath $dockerignore)
Assert-That 'container.just emitted' (Test-Path -LiteralPath $containerJust)
Assert-That 'no hook emitted by default' (-not (Test-Path -LiteralPath $hooks))
Assert-That 'no config file emitted' (-not (Test-Path -LiteralPath (Join-Path $repo 'anvil.toml')))
Assert-That 'no runner seam emitted' (-not (Test-Path -LiteralPath (Join-Path $repo 'justfiles/anvil/runner.just')))

$containerDir = Get-ChildItem -LiteralPath (Join-Path $repo '.anvil/container') -File
Assert-Equal 'container directory holds exactly two files' 2 $containerDir.Count

$justList = Invoke-Just -Repo $repo -Arguments @('--list')
Assert-That 'anvil-container is discoverable in just --list' ($justList.StdOut -match 'anvil-container')

# ------------------------------------------------------------- 2. first run --

Write-Section '2. First run builds the image'

$reference = Get-ImageReference -Repo $repo
Assert-That 'status reports an image reference' ($reference -like "$imagePrefix*") "got: '$reference'"
Assert-That 'image is absent before the first run' (-not (Test-ImagePresent $reference))

Write-Step "building and running (this takes several minutes on a cold cache)"
$firstRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'anvil-fmt')
Write-Detail (($firstRun.StdErr -split "`r?`n" | Select-Object -Last 4) -join "`n")

Assert-Equal 'anvil-fmt succeeds inside the container' 0 $firstRun.ExitCode
Assert-That 'the run reported building the image' ($firstRun.StdErr -match 'building .*(inputs changed|first run)')
Assert-That 'image is present afterwards' (Test-ImagePresent $reference)

# ------------------------------------------------------------ 3. second run --

Write-Section '3. Second run reuses the image'

$secondRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'anvil-fmt')
Assert-Equal 'anvil-fmt succeeds again' 0 $secondRun.ExitCode
Assert-That 'nothing was rebuilt' (-not ($secondRun.StdErr -match 'building ')) $secondRun.StdErr
Assert-Equal 'the reference is unchanged' $reference (Get-ImageReference -Repo $repo)

# The argv is executed verbatim, so a program that is not `just` is reachable
# too. This is the branch a recipe name would never exercise.
$bareCommand = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'cargo', '--version') -AllowFailure
Assert-Equal 'a non-just command runs in the image' 0 $bareCommand.ExitCode
Assert-That 'the bare command produced its own output' ($bareCommand.StdOut -match 'cargo\s+\d') $bareCommand.StdOut

$status = Invoke-Just -Repo $repo -Arguments @('anvil-container-status')
Assert-That 'status reports present and current' ($status.StdOut -match 'present and current') $status.StdOut
Assert-That 'status reports the selected engine' ($status.StdOut -match "engine:\s+.*$Engine") $status.StdOut

# The image's tools must not be masked by a cache volume. An engine seeds a
# named volume from the image only when the volume is first created, so a
# volume over $CARGO_HOME or $RUSTUP_HOME would pin the first image's binaries
# over every later tag -- a bumped tool would change the tag, build a new
# image, and still run the old binary.
$volumes = (Invoke-Engine -Arguments @('volume', 'ls', '--format', '{{.Name}}')).StdOut
$fixtureVolumes = @($volumes -split "`r?`n" | Where-Object { $_ -like "$imagePrefix*" })
Assert-That 'a registry cache volume exists' `
    (@($fixtureVolumes | Where-Object { $_ -like '*-cargo-registry' }).Count -eq 1) ($fixtureVolumes -join ', ')
Assert-That 'no volume masks CARGO_HOME or RUSTUP_HOME' `
    (@($fixtureVolumes | Where-Object { $_ -like '*-cargo' -or $_ -like '*-rustup' }).Count -eq 0) ($fixtureVolumes -join ', ')

# The positive half: a binary the image installed is still visible at run time
# with the caches mounted, so the tools a run uses are the ones the tag names.
# Argument vector deliberately free of spaces -- Start-Process joins
# -ArgumentList without quoting, so `bash -c '...'` would be re-split.
$probe = Invoke-Engine -AllowFailure -Arguments @(
    'run', '--rm', '--platform', 'linux/amd64',
    '-v', "$imagePrefix-cargo-registry:/usr/local/cargo/registry",
    '-v', "$imagePrefix-cargo-git:/usr/local/cargo/git",
    $reference, 'ls', '/usr/local/cargo/bin/cargo-binstall'
)
Assert-Equal 'a tool installed by the image survives the cache mounts' 0 $probe.ExitCode
Assert-That 'the tool resolves inside the image, not a volume' `
    ($probe.StdOut -match '/usr/local/cargo/bin/cargo-binstall') "$($probe.StdOut)$($probe.StdErr)"

# Only synthetic credentials are used; never inspect the developer's gh token.
function Hide-Token([string]$Text) { $Text -replace 'E2E-TOKEN:\[[^\]]+\]', 'E2E-TOKEN:[<redacted>]' }

$defaultToken = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'e2e-show-token') `
    -Environment @{ GITHUB_TOKEN = 'e2e-forwarded-token' }
Assert-That 'a host GITHUB_TOKEN is not forwarded by default' `
    ($defaultToken.StdOut -match 'E2E-TOKEN:\[\]') (Hide-Token "$($defaultToken.StdOut)$($defaultToken.StdErr)")

$withToken = Invoke-Just -Repo $repo -Arguments @('anvil-container', '--github-token', 'just', 'e2e-show-token') `
    -Environment @{ GITHUB_TOKEN = 'e2e-forwarded-token' }
Assert-That 'an explicitly opted-in GITHUB_TOKEN reaches a recipe in the container' `
    ($withToken.StdOut -match 'E2E-TOKEN:\[e2e-forwarded-token\]') (Hide-Token "$($withToken.StdOut)$($withToken.StdErr)")

$withoutToken = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'e2e-show-token') `
    -Environment @{ GITHUB_TOKEN = ''; GH_TOKEN = '' }
Assert-That 'no token is invented when the host has none' `
    ($withoutToken.StdOut -match 'E2E-TOKEN:\[\]') (Hide-Token "$($withoutToken.StdOut)$($withoutToken.StdErr)")

$missingToken = Invoke-Just -Repo $repo -Arguments @('anvil-container', '--github-token', 'just', 'e2e-show-token') `
    -Environment @{ GITHUB_TOKEN = '' } -AllowFailure
Assert-That 'opt-in without a supplied token fails' ($missingToken.ExitCode -ne 0)
Assert-That 'the missing-token failure explains the explicit input' `
    ($missingToken.StdErr -match '--github-token requires an explicitly supplied GITHUB_TOKEN')

# --------------------------------------------------------- 3b. worktrees -----

Write-Section '3b. A linked worktree resolves its git directory'

# A linked worktree's `.git` is a file naming an absolute host path outside the
# checkout. Bind-mounting only the worktree leaves that path unreachable, so git
# inside the container resolves nothing -- not HEAD, not origin/* -- and every
# check that needs history fails. This is not exotic: worktrees are the ordinary
# way to work on two branches at once.
#
# The worktree is given the same directory name as the fixture so the image name
# matches, and it checks out the same committed content, so the tag is identical
# and no rebuild is needed.
Invoke-Native -Command 'git' -Arguments @('add', '-A') -WorkingDirectory $repo | Out-Null
Invoke-Native -Command 'git' -Arguments @('-c', 'user.email=e2e@example.invalid', '-c', 'user.name=e2e',
    'commit', '-q', '-m', 'fixture') -WorkingDirectory $repo | Out-Null

$worktreeParent = Join-Path $workRoot 'wt'
$worktree = Join-Path $worktreeParent $fixtureName
Invoke-Native -Command 'git' -Arguments @('worktree', 'add', '-q', '-b', 'e2e-worktree', $worktree) `
    -WorkingDirectory $repo -AllowFailure | Out-Null
Assert-That 'the fixture worktree was created' (Test-Path -LiteralPath $worktree) $worktree
Assert-That 'its .git is a file, not a directory' `
    (Test-Path -LiteralPath (Join-Path $worktree '.git') -PathType Leaf) 'a linked worktree stores a gitdir pointer'

$wtReference = Get-ImageReference -Repo $worktree
Assert-Equal 'the worktree selects the same image, so nothing rebuilds' $reference $wtReference

$wtGit = Invoke-Just -Repo $worktree -Arguments @('anvil-container', 'just', 'e2e-show-git') -AllowFailure
Assert-Equal 'a recipe run from a worktree succeeds' 0 $wtGit.ExitCode
Assert-That 'git resolves the branch inside the container' `
    ($wtGit.StdOut -match 'E2E-GIT:\[e2e-worktree\]') "$($wtGit.StdOut)$($wtGit.StdErr)"

Invoke-Native -Command 'git' -Arguments @('worktree', 'remove', '--force', $worktree) `
    -WorkingDirectory $repo -AllowFailure | Out-Null

# ------------------------------------------------------ 4/5. hashed inputs ---

Write-Section '4. A changed input selects a new tag'

$toolchainPath = Join-Path $repo 'rust-toolchain.toml'
$originalToolchain = Get-Content -LiteralPath $toolchainPath -Raw
Write-Fixture $toolchainPath @'
[toolchain]
channel = "1.94"
'@

$bumped = Get-ImageReference -Repo $repo
Assert-That 'the reference changed with the toolchain' ($bumped -ne $reference) "before: $reference`nafter:  $bumped"
Assert-That 'the new tag is not already present' (-not (Test-ImagePresent $bumped))

Write-Section '5. Reverting the input returns to the original tag'

Write-Fixture $toolchainPath $originalToolchain
$reverted = Get-ImageReference -Repo $repo
Assert-Equal 'the original reference is restored' $reference $reverted
Assert-That 'the original image is still present' (Test-ImagePresent $reverted)

# ---------------------------------------------- 6. composing the Dockerfile --

Write-Section '6. A repository composes the Dockerfile around anvil''s regions'

$dockerfileBody = Get-Content -LiteralPath $dockerfile -Raw

# The gap between the base and tool regions: where a root CA or a proxy goes,
# and the reason the file is composed rather than owned outright.
$gapMarker = "# <<< anvil-managed: anvil-container-base`n"
Assert-That 'the composed Dockerfile carries the base region' ($dockerfileBody.Contains($gapMarker))
$composed = $dockerfileBody.Replace($gapMarker, $gapMarker + "`n# a repository-owned edit`nENV ANVIL_E2E_GAP=1`n")
Write-Fixture $dockerfile $composed
$editedReference = Get-ImageReference -Repo $repo
Assert-That 'adding to a gap selects a new tag' ($editedReference -ne $reference)

Write-Step 're-running the generator over the composed file'
$regen = Invoke-Native -Command $anvilExe -Arguments @('anvil', '--no-backends') -WorkingDirectory $repo -AllowFailure
Assert-Equal 'the generator succeeds over a composed file' 0 $regen.ExitCode
$afterRegen = Get-Content -LiteralPath $dockerfile -Raw
Assert-That 'content in a gap survives regeneration' ($afterRegen -match 'a repository-owned edit') `
    'anvil must preserve everything outside its own sentinels'
Assert-Equal 'the composed tag is unchanged by regeneration' $editedReference (Get-ImageReference -Repo $repo)

# Anvil never overwrites repository content, and a region body is no exception:
# an edit inside one is preserved, exactly as `updates.md` §2 preserves an
# edited owned file. That is why the gaps matter -- editing inside a region
# silently freezes the base digest and the tool pins at today's values while
# the tag keeps resolving, so the layout has to make the gaps the obvious place
# to add things rather than relying on the engine to police it.
Write-Step 'editing inside a region, which anvil preserves rather than overwrites'
$frozen = $afterRegen.Replace('ARG JUST_VERSION=', 'ARG JUST_VERSION=0.0.0 # ')
Assert-That 'the edit landed inside the region' ($frozen -ne $afterRegen)
Write-Fixture $dockerfile $frozen
$regen = Invoke-Native -Command $anvilExe -Arguments @('anvil', '--no-backends') -WorkingDirectory $repo -AllowFailure
Assert-Equal 'the generator succeeds over an edited region' 0 $regen.ExitCode
$reclaimed = Get-Content -LiteralPath $dockerfile -Raw
Assert-That 'an edit inside a region is preserved, not overwritten' ($reclaimed -match 'JUST_VERSION=0\.0\.0') `
    'anvil must never destroy repository content, in a region or a file'
Assert-That 'the surrounding gap content is untouched' ($reclaimed -match 'a repository-owned edit')
Assert-That 'the sentinels survive the edit' ($reclaimed -match '# <<< anvil-managed: anvil-container-base')

Write-Fixture $dockerfile $dockerfileBody
Assert-Equal 'restoring the Dockerfile restores the tag' $reference (Get-ImageReference -Repo $repo)

# ----------------------------------------------------------------- 7. hook ---

Write-Section '7. The credential hook reaches build and run'

# podman on Windows cannot mount a build secret at all: it composes its own temp
# path from the already-translated build context and joins it with a Windows
# separator. That is an engine defect with no client-side workaround, documented
# in docs/design/containers.md. Reporting it as a failure every run would train
# the reader to ignore red, so it is called out and skipped.
$buildSecretsSupported = -not ($Engine -eq 'podman' -and $IsWindows)
if (-not $buildSecretsSupported) {
    Write-Step 'skipping the hook sections: podman on Windows cannot mount build secrets'
    Write-Step 'everything above is engine-agnostic and has already run'
} else {
# A user writes this file by hand; the public catalog does not emit one.
$env:ANVIL_E2E_BUILD_SECRET = 'build-secret-value'
Write-Fixture $hooks @'
function Anvil-BuildSecrets {
    @{ Secrets = @{ e2e_token = $env:ANVIL_E2E_BUILD_SECRET } }
}

function Anvil-RunEnv {
    @{ Env = @{ ANVIL_E2E_RUNTIME = 'run-value' } }
}
'@

$hookReference = Get-ImageReference -Repo $repo
Assert-That 'adding a hook selects a new tag' ($hookReference -ne $reference) `
    "the hook file's content must be part of the image identity"

# The default Dockerfile does not consume the secret, so prove the wiring by
# having the image read it. This is a fixture-side Dockerfile edit, which is a
# supported user action (proved in section 6).
#
# The build *writes* the secret and deletes it in the same layer, which is what
# the real Dockerfile does with the credential files an install leaves behind.
# Reading it alone would make the filesystem assertion below unfalsifiable:
# nothing would have written the string, so `grep` would find nothing whatever
# the layering did. A control build immediately below proves the probe can in
# fact see a leak.
$secretStanza = @'

# --- e2e: prove the build secret arrives and never lands in a layer ---
RUN --mount=type=secret,id=e2e_token,required=true \
    test -s /run/secrets/e2e_token \
    && echo "e2e: secret length $(wc -c < /run/secrets/e2e_token)" \
    && cp /run/secrets/e2e_token /usr/local/cargo/credentials.toml \
    && rm -f /usr/local/cargo/credentials.toml
'@

# The same stanza without the deletion. Built first, so a probe that cannot
# detect a leak fails here rather than passing silently on the real image.
$leakControlStanza = $secretStanza -replace '(?m)\s*&& rm -f /usr/local/cargo/credentials\.toml$', ''
Write-Fixture $dockerfile ($dockerfileBody + $leakControlStanza)
Write-Step 'building a deliberately leaking image to prove the probe works'
$controlRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'anvil-fmt') -AllowFailure
Assert-Equal 'the control image builds' 0 $controlRun.ExitCode
$controlLeak = Invoke-Engine -Arguments @(
    'run', '--rm', '--pull=never', (Get-ImageReference -Repo $repo),
    'grep', '-rsq', 'build-secret-value', '/opt/anvil', '/root', '/usr/local/cargo', '/tmp', '/run'
) -AllowFailure
Assert-Equal 'the probe detects a secret left in the filesystem' 0 $controlLeak.ExitCode

Write-Fixture $dockerfile ($dockerfileBody + $secretStanza)

Write-Step 'rebuilding with the hook active'
$hookRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'anvil-fmt') -AllowFailure
Assert-Equal 'the run with a hook succeeds' 0 $hookRun.ExitCode
Assert-That 'the hook announced itself at build time' ($hookRun.StdErr -match 'Anvil-BuildSecrets')
Assert-That 'the build secret was declared' ($hookRun.StdErr -match 'build secrets: e2e_token')
Assert-That 'the hook announced itself at run time' ($hookRun.StdErr -match 'Anvil-RunEnv')
Assert-That 'forwarded names are reported' ($hookRun.StdErr -match 'forwarding env: ANVIL_E2E_RUNTIME')

$secretReference = Get-ImageReference -Repo $repo
$layers = Invoke-Engine -Arguments @('history', '--no-trunc', $secretReference) -AllowFailure
Assert-Equal 'the image history is readable' 0 $layers.ExitCode
Assert-That 'no build command records the secret' `
    (-not ($layers.StdOut -match 'build-secret-value')) 'a secret must never reach a build argument'

# `history` reports the command that created each layer, not its contents, so on
# its own it cannot see a secret that was *written* into the filesystem -- which
# is the hazard the Dockerfile guards against by deleting credential files in
# the same layer as the install. Look at the filesystem the image actually
# carries. grep exits 1 for "no match", which is the result we want; -s keeps an
# unreadable path from turning into exit 2 and passing for the wrong reason.
$leak = Invoke-Engine -Arguments @(
    'run', '--rm', '--pull=never', $secretReference,
    'grep', '-rsq', 'build-secret-value', '/opt/anvil', '/root', '/usr/local/cargo', '/tmp', '/run'
) -AllowFailure
Assert-Equal 'the secret is absent from the image filesystem' 1 $leak.ExitCode

Write-Step 'checking that the forwarded value arrives inside the container'
$showEnv = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'e2e-show-env') -AllowFailure
Assert-That 'the run-time value reaches a recipe in the container' `
    ($showEnv.StdOut -match 'E2E:run-value') "stdout: $($showEnv.StdOut)`nstderr: $($showEnv.StdErr)"

# ------------------------------------------------------ 8. hook fails closed --

Write-Section '8. An empty hook value fails closed'

Write-Fixture $hooks @'
function Anvil-BuildSecrets {
    @{ Secrets = @{ e2e_token = '' } }
}
'@

$emptyHook = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'just', 'anvil-fmt') -AllowFailure
Assert-That 'an empty secret aborts the run' ($emptyHook.ExitCode -ne 0) `
    'BuildKit would mount an empty secret and exit 0, tagging a degraded image with a valid hash'
Assert-That 'the failure names the offending secret' ($emptyHook.StdErr -match "empty value for secret 'e2e_token'") `
    $emptyHook.StdErr

# ------------------------------------------- 9. hook output is not the tag ---

Write-Section '9. Hook output does not change the tag'

Write-Fixture $hooks @'
function Anvil-BuildSecrets {
    @{ Secrets = @{ e2e_token = $env:ANVIL_E2E_BUILD_SECRET } }
}

function Anvil-RunEnv {
    @{ Env = @{ ANVIL_E2E_RUNTIME = 'run-value' } }
}
'@
Assert-Equal 'restoring the hook restores the tag' $secretReference (Get-ImageReference -Repo $repo)

# The invariant that matters: a *minted* credential must never influence the
# tag, or two developers holding different tokens would compute different
# images from identical inputs -- and a rotated token would force a rebuild.
# Proving it needs the hook file to be byte-identical while what it returns
# differs, so the value is read from the environment rather than written into
# the file. Changing the file instead would only re-prove that file content is
# hashed, which section 7 already covers.
Write-Fixture $hooks @'
function Anvil-BuildSecrets {
    @{ Secrets = @{ e2e_token = $env:ANVIL_E2E_MINT } }
}

function Anvil-RunEnv {
    @{ Env = @{ ANVIL_E2E_RUNTIME = 'run-value' } }
}
'@
$mintedA = Get-ImageReference -Repo $repo -Environment @{ ANVIL_E2E_MINT = 'first-minted-value' }
$mintedB = Get-ImageReference -Repo $repo -Environment @{ ANVIL_E2E_MINT = 'a-completely-different-second-value' }
Assert-That 'the tag is stable across two different minted values' `
    ($mintedA -and $mintedA -eq $mintedB) "first: $mintedA`nsecond: $mintedB"

# ...while the file that produces those values is itself hashed, so a changed
# hook still renames the image.
$hookBodyChanged = $mintedA -ne $secretReference
Assert-That 'a changed hook body still changes the tag' $hookBodyChanged `
    "the hook file is a hashed input; before: $secretReference, after: $mintedA"

}   # end of the build-secret sections (7-9)

# ------------------------------------------------------ 10. no nested runs ---

Write-Section '10. Recipes run natively inside the image'

# Sections 7-9 build the image that carries the secret stanza; without them the
# current reference is the plain one.
#
# Impact scoping is switched off because this fixture is a fresh `git init`
# with no `origin/main`, so the snapshot behind `anvil-fmt` has no base ref to
# diff against. That is a property of the fixture, not of containerized
# execution, and this section is about the re-entry guard running the recipe
# natively rather than about history being reachable.
$nestedReference = if ($buildSecretsSupported) { $secretReference } else { Get-ImageReference -Repo $repo }
$nested = Invoke-Engine -Arguments @(
    'run', '--rm', '-e', 'ANVIL_IN_CONTAINER=1', '-e', 'ANVIL_IMPACT=off',
    '-v', "$(ConvertTo-EnginePath $repo):/workspace", '-w', '/workspace',
    $nestedReference, 'just', 'anvil-container', 'just', 'anvil-fmt'
) -AllowFailure
Assert-Equal 'anvil-container passes through inside the image' 0 $nested.ExitCode
Assert-That 'no engine was invoked from inside the container' `
    (-not ($nested.StdErr -match 'building |Cannot connect to the Docker daemon')) $nested.StdErr

# ---------------------------------------------------- authenticated prepare --

Write-Section '11. Authenticated preparation and fresh offline validation'

$preparedImage = $reference
$registryName = "$imagePrefix-fixture-registry"
$registryDir = Join-Path $repo 'fixture-registry'
$engineRepo = ConvertTo-EnginePath $repo
$configPath = Join-Path $repo '.cargo/config.toml'
$acquisitions = Join-Path $repo 'fixture-acquisitions'
$fixtureToken = 'synthetic-fixture-registry-token'
$secondToken = 'synthetic.fixture.second+[token]'

function Assert-PreparedCleanup([string]$Case) {
    $volumes = (Invoke-Engine -Arguments @('volume', 'ls', '--format', '{{.Name}}')).StdOut
    $containers = (Invoke-Engine -Arguments @('ps', '-a', '--format', '{{.Names}}')).StdOut
    Assert-That "$Case removes invocation cache volumes" `
        (-not (($volumes -split "`r?`n") | Where-Object { $_ -like "$imagePrefix-prepare-*" }))
    Assert-That "$Case removes invocation containers" `
        (-not (($containers -split "`r?`n") | Where-Object { $_ -like "$imagePrefix-prepare-*" }))
}

function Invoke-PreparedCase([string]$Case, [string[]]$Command, [bool]$Succeeds = $true, [hashtable]$ExtraEnvironment = @{}) {
    $before = if (Test-Path -LiteralPath $acquisitions) { @(Get-Content -LiteralPath $acquisitions).Count } else { 0 }
    $environment = @{
        FIXTURE_IMAGE = $preparedImage
        FIXTURE_TOKEN = $fixtureToken
        FIXTURE_SECOND_TOKEN = $secondToken
        FIXTURE_ACQUISITIONS = $acquisitions
        ANVIL_CONTAINER_NO_REBUILD = '1'
    }
    foreach ($key in $ExtraEnvironment.Keys) { $environment[$key] = $ExtraEnvironment[$key] }
    $result = Invoke-Just -Repo $repo -Arguments (@('anvil-container') + $Command) -Environment $environment -AllowFailure
    Assert-That $Case (($result.ExitCode -eq 0) -eq $Succeeds) "$($result.StdOut)`n$($result.StdErr)"
    $after = if (Test-Path -LiteralPath $acquisitions) { @(Get-Content -LiteralPath $acquisitions).Count } else { 0 }
    Assert-Equal "$Case acquires preparation environment exactly once" ($before + 1) $after
    Assert-That "$Case never surfaces either preparation value" `
        (-not ("$($result.StdOut)$($result.StdErr)".Contains($fixtureToken)) -and
         -not ("$($result.StdOut)$($result.StdErr)".Contains($secondToken)))
    Assert-PreparedCleanup $Case
    $result
}

Write-Fixture (Join-Path $registryDir 'source/private-dependency-0.1.0/Cargo.toml') @'
[package]
name = "private-dependency"
version = "0.1.0"
edition = "2021"
'@
Write-Fixture (Join-Path $registryDir 'source/private-dependency-0.1.0/src/lib.rs') 'pub fn answer() -> u8 { 42 }'
Write-Fixture (Join-Path $registryDir 'server.ps1') @'
$ErrorActionPreference = 'Stop'
$listener = [Net.HttpListener]::new()
$listener.Prefixes.Add('http://+:8080/')
$listener.Start()
Write-Output 'REGISTRY_READY'
while ($listener.IsListening) {
    $context = $listener.GetContext()
    try {
        $path = $context.Request.Url.AbsolutePath.TrimStart('/')
        if ($path -ne 'config.json' -and $context.Request.Headers['Authorization'] -cne $env:FIXTURE_AUTH_TOKEN) {
            Write-Output 'AUTH_REJECTED'
            $context.Response.StatusCode = 401
        } elseif ($path.Contains('..')) {
            $context.Response.StatusCode = 400
        } else {
            if ($path -ne 'config.json') { Write-Output 'AUTH_ACCEPTED' }
            $file = Join-Path /srv $path
            if (Test-Path -LiteralPath $file -PathType Leaf) {
                $bytes = [IO.File]::ReadAllBytes($file)
                $context.Response.ContentLength64 = $bytes.Length
                $context.Response.OutputStream.Write($bytes)
            } else {
                $context.Response.StatusCode = 404
            }
        }
    } finally { $context.Response.Close() }
}
'@
Write-Fixture (Join-Path $registryDir 'archive.ps1') @'
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force /srv/private-dependency/0.1.0 | Out-Null
& tar -czf /srv/private-dependency/0.1.0/download --directory /srv/source private-dependency-0.1.0
exit $LASTEXITCODE
'@
Invoke-Engine -Arguments @('run', '--rm', '-v', "${engineRepo}/fixture-registry:/srv", $preparedImage,
    'pwsh', '-NoLogo', '-NoProfile', '-File', '/srv/archive.ps1') | Out-Null
$checksum = (Get-FileHash -LiteralPath (Join-Path $registryDir 'private-dependency/0.1.0/download') -Algorithm SHA256).Hash.ToLowerInvariant()
$index = @{ name = 'private-dependency'; vers = '0.1.0'; deps = @(); cksum = $checksum; features = @{}; yanked = $false }
Write-Fixture (Join-Path $registryDir 'pr/iv/private-dependency') (ConvertTo-Json -InputObject $index -Compress)

try {
    Invoke-Engine -Arguments @('run', '-d', '--name', $registryName,
        '-e', "FIXTURE_AUTH_TOKEN=$fixtureToken", '-v', "${engineRepo}/fixture-registry:/srv:ro",
        $preparedImage, 'pwsh', '-NoLogo', '-NoProfile', '-File', '/srv/server.ps1') | Out-Null
    $registryInfo = (Invoke-Engine -Arguments @('inspect', $registryName)).StdOut | ConvertFrom-Json
    $addresses = @($registryInfo[0].NetworkSettings.Networks.PSObject.Properties | ForEach-Object { $_.Value.IPAddress } | Where-Object { $_ })
    if ($addresses.Count -ne 1) { throw 'fixture registry needs exactly one default network address' }
    $registryUrl = "http://$($addresses[0]):8080/"
    Write-Fixture (Join-Path $registryDir 'config.json') (ConvertTo-Json -Compress @{
        dl = "${registryUrl}{crate}/{version}/download"; 'auth-required' = $true
    })
    $ready = $false
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $logs = (Invoke-Engine -Arguments @('logs', $registryName)).StdOut
        if ($logs.Contains('REGISTRY_READY')) { $ready = $true; break }
        Start-Sleep -Milliseconds 250
    }
    Assert-That 'the authentication-enforcing registry is ready' $ready
    if (-not $ready) { throw 'fixture registry did not become ready' }

    $baseConfig = @"
[registries.fixture]
index = "sparse+$registryUrl"
[registry]
global-credential-providers = ["cargo:token"]
[env]
FIXTURE_UNRELATED = { value = "preserved", force = true }
"@
    Write-Fixture $configPath ($baseConfig + "`n[build]`nrustflags = ['--cfg=fixture_flag']`n")
    Write-Fixture (Join-Path $repo 'Cargo.toml') @'
[package]
name = "anvil-e2e"
version = "0.1.0"
edition = "2021"
[dependencies]
private-dependency = { version = "=0.1.0", registry = "fixture" }
probe-macro = { path = "probe-macro" }
[workspace]
members = ["probe-macro"]
'@
    Write-Fixture (Join-Path $repo 'build.rs') @'
fn main() {
    assert!(std::env::var_os("CARGO_REGISTRIES_FIXTURE_TOKEN").is_none());
    assert!(std::env::var_os("FIXTURE_SECOND_CREDENTIAL").is_none());
    assert!(std::env::var_os("RUSTC_BOOTSTRAP").is_none());
    assert_eq!(std::env::var("FIXTURE_UNRELATED").unwrap(), "preserved");
    println!("cargo:rustc-check-cfg=cfg(fixture_flag)");
    println!("cargo:warning=BUILD_SCRIPT_CREDENTIAL_FREE");
}
'@
    Write-Fixture (Join-Path $repo 'probe-macro/Cargo.toml') @'
[package]
name = "probe-macro"
version = "0.1.0"
edition = "2021"
[lib]
proc-macro = true
'@
    Write-Fixture (Join-Path $repo 'probe-macro/src/lib.rs') @'
extern crate proc_macro;
#[proc_macro]
pub fn credential_free(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    assert!(std::env::var_os("CARGO_REGISTRIES_FIXTURE_TOKEN").is_none());
    assert!(std::env::var_os("FIXTURE_SECOND_CREDENTIAL").is_none());
    eprintln!("PROC_MACRO_CREDENTIAL_FREE");
    "true".parse().unwrap()
}
'@
    Write-Fixture (Join-Path $repo 'src/lib.rs') @'
#[test]
fn prepared_dependency_is_usable_without_credentials() {
    assert!(std::env::var_os("CARGO_REGISTRIES_FIXTURE_TOKEN").is_none());
    assert!(std::env::var_os("FIXTURE_SECOND_CREDENTIAL").is_none());
    assert!(std::env::var_os("RUSTC_BOOTSTRAP").is_none());
    assert_eq!(std::env::var("CARGO_NET_OFFLINE").unwrap(), "true");
    assert_eq!(std::env::var("FIXTURE_UNRELATED").unwrap(), "preserved");
    assert!(cfg!(fixture_flag));
    assert!(probe_macro::credential_free!());
    assert_eq!(private_dependency::answer(), 42);
}
'@
    Write-Fixture (Join-Path $repo 'assert-prepared.ps1') @'
$ErrorActionPreference = 'Stop'
foreach ($name in @('CARGO_REGISTRIES_FIXTURE_TOKEN', 'FIXTURE_SECOND_CREDENTIAL', 'RUSTC_BOOTSTRAP')) {
    if ($null -ne [Environment]::GetEnvironmentVariable($name)) { throw "preparation environment survived: $name" }
}
if ($env:CARGO_NET_OFFLINE -ne 'true') { throw 'validation must be offline' }
foreach ($file in @("$env:CARGO_HOME/credentials", "$env:CARGO_HOME/credentials.toml", "$env:HOME/.netrc")) {
    if (Test-Path -LiteralPath $file) { throw 'credential file reached validation' }
}
Write-Output 'DIRECT_COMMAND_CREDENTIAL_FREE'
'@
    $lockSetup = @('run', '--rm', '-v', "${engineRepo}:/workspace", '-w', '/workspace',
        '-e', "CARGO_REGISTRIES_FIXTURE_TOKEN=$fixtureToken", $preparedImage, 'cargo', 'generate-lockfile')
    # Locate by content rather than depending on the engine argument layout.
    $wrongCredential = @($lockSetup | ForEach-Object {
        if ($_ -eq "CARGO_REGISTRIES_FIXTURE_TOKEN=$fixtureToken") { 'CARGO_REGISTRIES_FIXTURE_TOKEN=synthetic-wrong-token' } else { $_ }
    })
    $denied = Invoke-Engine -Arguments $wrongCredential -AllowFailure
    Assert-That 'wrong credentials cannot fetch the private dependency' ($denied.ExitCode -ne 0)
    Invoke-Engine -Arguments $lockSetup | Out-Null
    Assert-That 'the authenticated fixture produces a locked dependency graph' (Test-Path -LiteralPath (Join-Path $repo 'Cargo.lock'))
    $authLogs = (Invoke-Engine -Arguments @('logs', $registryName)).StdOut
    Assert-That 'the registry observed both rejection and genuine authenticated access' `
        ($authLogs.Contains('AUTH_REJECTED') -and $authLogs.Contains('AUTH_ACCEPTED'))

    Write-Fixture $hooks @'
function Anvil-ResolveImage { param([string]$Tag) $env:FIXTURE_IMAGE }
function Anvil-PrepareEnv {
    Add-Content -LiteralPath $env:FIXTURE_ACQUISITIONS -Value 'acquired'
    @{ Env = @{
        CARGO_REGISTRIES_FIXTURE_TOKEN = $env:FIXTURE_TOKEN
        FIXTURE_SECOND_CREDENTIAL = $env:FIXTURE_SECOND_TOKEN
    } }
}
'@
    $direct = Invoke-PreparedCase 'a direct command runs without preparation credentials' @('pwsh', '-NoLogo', '-NoProfile', '-File', '/workspace/assert-prepared.ps1')
    Assert-That 'the direct credential-absence probe actually executed' ($direct.StdOut.Contains('DIRECT_COMMAND_CREDENTIAL_FREE'))
    $compiled = Invoke-PreparedCase 'fresh offline build scripts, procedural macros and tests succeed' @('cargo', 'test', '--locked', '--', '--nocapture')
    Assert-That 'the build-script and procedural-macro probes actually executed' `
        ($compiled.StdErr.Contains('BUILD_SCRIPT_CREDENTIAL_FREE') -and $compiled.StdErr.Contains('PROC_MACRO_CREDENTIAL_FREE'))
    Assert-That 'the actual dependency-using test ran' ($compiled.StdOut.Contains('1 passed'))
    Invoke-PreparedCase 'a bad preparation credential fails without validation' @('pwsh', '-File', '/workspace/assert-prepared.ps1') $false `
        @{ FIXTURE_TOKEN = 'synthetic-wrong-token' } | Out-Null
    Write-Fixture (Join-Path $repo 'incomplete-cache.ps1') @'
$ErrorActionPreference = 'Stop'
Remove-Item -LiteralPath "$env:CARGO_HOME/registry/cache", "$env:CARGO_HOME/registry/src" -Recurse -Force
& cargo test --locked
exit $LASTEXITCODE
'@
    $incomplete = Invoke-PreparedCase 'an incomplete cache fails offline without credential reacquisition' @('pwsh', '-File', '/workspace/incomplete-cache.ps1') $false
    Assert-That 'the incomplete-cache failure is from Cargo offline enforcement' `
        ("$($incomplete.StdOut)$($incomplete.StdErr)".Contains('--offline'))
    Write-Fixture (Join-Path $repo 'validation-failure.ps1') 'exit 23'
    $failed = Invoke-PreparedCase 'a validation failure cleans up' @('pwsh', '-File', '/workspace/validation-failure.ps1') $false
    Assert-Equal 'validation exit status is preserved' 23 $failed.ExitCode

    Write-Section '12. Preparation suppresses workspace compiler and native-loader callbacks'
    Write-Fixture (Join-Path $repo 'compiler-probe.sh') @'
#!/bin/sh
if [ -n "${CARGO_REGISTRIES_FIXTURE_TOKEN:-}" ]; then
    echo CALLBACK_WITH_CREDENTIAL >&2
    exit 91
fi
echo compiler-credential-free >> /workspace/callback-observations
case "$1" in
    */rustc) exec "$@" ;;
    *) exec "$(rustup which rustc)" "$@" ;;
esac
'@
    Write-Fixture (Join-Path $repo 'native-probe.c') @'
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
__attribute__((constructor)) static void observe(void) {
    if (getenv("CARGO_REGISTRIES_FIXTURE_TOKEN") != NULL) {
        fputs("CALLBACK_WITH_CREDENTIAL\n", stderr);
        _exit(91);
    }
    FILE *out = fopen("/workspace/callback-observations", "a");
    if (out != NULL) { fputs("native-credential-free\n", out); fclose(out); }
}
'@
    Write-Fixture (Join-Path $repo 'setup-probes.ps1') @'
$ErrorActionPreference = 'Stop'
& chmod +x /workspace/compiler-probe.sh
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& cc -shared -fPIC /workspace/native-probe.c -o /workspace/native-probe.so
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$env:RUSTC_BOOTSTRAP = '1'
$hostTriple = ((& rustc -vV) | Where-Object { $_.StartsWith('host: ') }).Substring(6)
$json = (& rustc -Zunstable-options --print target-spec-json --target $hostTriple) -join "`n"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$target = $json | ConvertFrom-Json -AsHashtable
$target['default-codegen-backend'] = '/workspace/native-probe.so'
[IO.File]::WriteAllText('/workspace/probe-target.json', (ConvertTo-Json -InputObject $target -Depth 20))
Write-Output "IMAGE_HOST:$hostTriple"
'@
    $setup = Invoke-Engine -Arguments @('run', '--rm', '-v', "${engineRepo}:/workspace", '-w', '/workspace',
        $preparedImage, 'pwsh', '-NoProfile', '-File', '/workspace/setup-probes.ps1')
    $hostLine = @($setup.StdOut -split "`r?`n" | Where-Object { $_.StartsWith('IMAGE_HOST:') })
    if ($hostLine.Count -ne 1) { throw 'could not resolve the actual image compiler host' }
    $imageHost = $hostLine[0].Substring(11)
    $callbackCases = [ordered]@{
        'build.rustc' = "[build]`nrustc = '/workspace/compiler-probe.sh'`nrustflags = ['--cfg=fixture_flag']"
        'build.rustc-wrapper' = "[build]`nrustc-wrapper = '/workspace/compiler-probe.sh'`nrustflags = ['--cfg=fixture_flag']"
        'build.rustc-workspace-wrapper' = "[build]`nrustc-workspace-wrapper = '/workspace/compiler-probe.sh'`nrustflags = ['--cfg=fixture_flag']"
        'build backend flags' = "[build]`nrustflags = ['-Zcodegen-backend=/workspace/native-probe.so']"
        'target backend flags' = "[target.$imageHost]`nrustflags = ['-Zcodegen-backend=/workspace/native-probe.so']"
        'host flags' = "[unstable]`nhost-config = true`ntarget-applies-to-host = true`n[host]`nrustflags = ['-Zcodegen-backend=/workspace/native-probe.so']"
        'host triple flags' = "[unstable]`nhost-config = true`ntarget-applies-to-host = true`n[host.$imageHost]`nrustflags = ['-Zcodegen-backend=/workspace/native-probe.so']"
        'scalar custom target backend' = "[unstable]`njson-target-spec = true`n[build]`ntarget = '/workspace/probe-target.json'"
        'array custom target backend' = "[unstable]`njson-target-spec = true`n[build]`ntarget = ['/workspace/probe-target.json']"
        'mixed custom target backend' = "[unstable]`njson-target-spec = true`n[build]`ntarget = ['$imageHost', '/workspace/probe-target.json']"
        'forced native loader' = "[env.LD_PRELOAD]`nvalue = '/workspace/native-probe.so'`nforce = true"
        'forced relative native loader' = "[env.LD_PRELOAD]`nvalue = 'native-probe.so'`nforce = true`nrelative = true"
    }
    foreach ($case in $callbackCases.Keys) {
        Write-Fixture $configPath ($baseConfig + "`n" + $callbackCases[$case] + "`n")
        $control = Invoke-Engine -Arguments @('run', '--rm', '-v', "${engineRepo}:/workspace", '-w', '/workspace',
            '-e', "CARGO_REGISTRIES_FIXTURE_TOKEN=$fixtureToken", '-e', 'RUSTC_BOOTSTRAP=1',
            $preparedImage, 'cargo', 'fetch', '--locked') -AllowFailure
        Assert-That "$case has a live uncontrolled callback positive control" `
            ($control.ExitCode -ne 0 -and "$($control.StdOut)$($control.StdErr)".Contains('CALLBACK_WITH_CREDENTIAL'))
        Invoke-PreparedCase "$case cannot execute with preparation credentials" `
            @('pwsh', '-File', '/workspace/assert-prepared.ps1') | Out-Null
        if ($case.StartsWith('build.rustc')) {
            $observations = Join-Path $repo 'callback-observations'
            if (Test-Path -LiteralPath $observations) { Remove-Item -LiteralPath $observations -Force }
            Invoke-PreparedCase "$case remains active in credential-free validation" @('cargo', 'test', '--locked') | Out-Null
            Assert-That "$case actually ran without credentials" `
                ((Test-Path -LiteralPath $observations) -and (Get-Content -LiteralPath $observations -Raw).Contains('compiler-credential-free'))
        }
    }
    Write-Fixture $configPath ($baseConfig + "`n[env.LD_PRELOAD]`nvalue = '/workspace/native-probe.so'`nforce = true`n[env.RUSTC_BOOTSTRAP]`nvalue = '-1'`nforce = true`n[build]`nrustflags = ['--cfg=fixture_flag']`n")
    # Forced bootstrap is deliberately retained in validation; use a direct
    # command so the build-script bootstrap-absence assertion stays meaningful.
    Invoke-PreparedCase 'forced bootstrap cannot undo preparation controls' @('true') | Out-Null
    Write-Fixture $configPath ($baseConfig + "`n[env.LD_PRELOAD]`nvalue = '/workspace/native-probe.so'`nforce = true`n[build]`nrustflags = ['--cfg=fixture_flag']`n")
    $observations = Join-Path $repo 'callback-observations'
    if (Test-Path -LiteralPath $observations) { Remove-Item -LiteralPath $observations -Force }
    Invoke-PreparedCase 'normal flags and forced native loading remain active in validation' @('cargo', 'test', '--locked') | Out-Null
    Assert-That 'the native callback actually executed without credentials' `
        ((Test-Path -LiteralPath $observations) -and (Get-Content -LiteralPath $observations -Raw).Contains('native-credential-free'))

    Write-Section '13. Cache leak rejection and incomplete inspection'
    Write-Fixture (Join-Path $repo 'fixture-provider.ps1') @'
$ErrorActionPreference = 'Stop'
[Console]::Out.WriteLine('{"v":[1]}')
$request = [Console]::In.ReadLine() | ConvertFrom-Json
$mode = [IO.File]::ReadAllText('/workspace/fixture-leak-mode').Trim()
$value = if ($mode -eq 'second') { $env:FIXTURE_SECOND_CREDENTIAL } else { $env:CARGO_REGISTRIES_FIXTURE_TOKEN }
switch ($mode) {
    'registry' { [IO.File]::WriteAllText("$env:CARGO_HOME/registry/.fixture-leak", $value) }
    'git' { [IO.File]::WriteAllText("$env:CARGO_HOME/git/.fixture-leak", $value) }
    'second' { [IO.File]::WriteAllText("$env:CARGO_HOME/registry/.fixture-leak", $value) }
    'binary' {
        $bytes = [byte[]]::new(70000) + [Text.Encoding]::UTF8.GetBytes($value) + [byte[]]@(0, 255, 0)
        [IO.File]::WriteAllBytes("$env:CARGO_HOME/registry/.fixture-leak", $bytes)
    }
    'inspection' {
        New-Item -ItemType SymbolicLink -Path "$env:CARGO_HOME/registry/.fixture-link" -Target /workspace/src | Out-Null
    }
    'stderr' { [Console]::Error.WriteLine($value) }
}
[Console]::Out.WriteLine((ConvertTo-Json -Compress @{
    Ok = @{ kind = 'get'; token = $env:CARGO_REGISTRIES_FIXTURE_TOKEN; 'cache' = 'session'; 'operation_independent' = $true }
}))
'@
    $providerConfig = $baseConfig.Replace('["cargo:token"]', '["pwsh -NoLogo -NoProfile -File /workspace/fixture-provider.ps1"]')
    Write-Fixture $configPath $providerConfig
    Write-Fixture (Join-Path $repo 'fixture-leak-mode') 'clean'
    Invoke-PreparedCase 'the trusted authentication provider works without injecting a leak' `
        @('pwsh', '-File', '/workspace/assert-prepared.ps1') | Out-Null
    foreach ($mode in @('registry', 'git', 'second', 'binary', 'inspection', 'stderr')) {
        Write-Fixture (Join-Path $repo 'fixture-leak-mode') $mode
        $rejected = Invoke-PreparedCase "$mode credential leak or incomplete inspection rejects preparation" `
            @('pwsh', '-File', '/workspace/assert-prepared.ps1') $false
        Assert-That "$mode never starts requested validation" (-not $rejected.StdOut.Contains('DIRECT_COMMAND_CREDENTIAL_FREE'))
    }
} finally {
    Invoke-Engine -Arguments @('rm', '-f', $registryName) -AllowFailure | Out-Null
}

# --------------------------------------------------------------- teardown ----

Write-Section 'Teardown'

if ($KeepArtifacts) {
    Write-Step "keeping $repo and images matching $imagePrefix*"
} else {
    Write-Step 'removing cache volumes via anvil-container-down'
    Invoke-Just -Repo $repo -Arguments @('anvil-container-down') -AllowFailure | Out-Null
    Remove-AnvilImages -Prefix $imagePrefix
    Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
    Write-Step 'removed the fixture repository'
}

$elapsed = (Get-Date) - $script:Started
Write-Host ''
Write-Host ('-' * 78)
$summary = "{0}/{1} checks passed in {2:mm\:ss}" -f $script:Passed, ($script:Passed + $script:Failed), $elapsed
if ($script:Failed -eq 0) {
    Write-Host "PASS  $summary" -ForegroundColor Green
    exit 0
}
Write-Host "FAIL  $summary ($($script:Failed) failed)" -ForegroundColor Red
exit 1
