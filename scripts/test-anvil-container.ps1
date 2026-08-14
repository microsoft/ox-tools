# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
    End-to-end test of cargo-anvil's containerized execution, from a user's seat.

.DESCRIPTION
    Creates a throwaway repository in a temp directory, generates the anvil tree
    into it with the locally-built cargo-anvil, and then does only what a
    developer would do: run `just anvil-container <recipe>` and observe what
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
         GITHUB_TOKEN is forwarded while an absent one is not invented.
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

    $previous = @{}
    foreach ($key in $Environment.Keys) {
        $previous[$key] = [Environment]::GetEnvironmentVariable($key)
        Set-Item -LiteralPath "Env:$key" -Value $Environment[$key]
    }
    $entered = $false
    try {
        if ($WorkingDirectory) { Push-Location $WorkingDirectory; $entered = $true }
        $stdoutFile = [System.IO.Path]::GetTempFileName()
        $stderrFile = [System.IO.Path]::GetTempFileName()
        try {
            $process = Start-Process -FilePath $Command -ArgumentList $Arguments -NoNewWindow -Wait -PassThru `
                -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
            $result = [pscustomobject]@{
                ExitCode = $process.ExitCode
                StdOut   = (Get-Content -LiteralPath $stdoutFile -Raw -ErrorAction SilentlyContinue) ?? ''
                StdErr   = (Get-Content -LiteralPath $stderrFile -Raw -ErrorAction SilentlyContinue) ?? ''
            }
        } finally {
            Remove-Item -LiteralPath $stdoutFile, $stderrFile -Force -ErrorAction SilentlyContinue
        }
    } finally {
        if ($entered) { Pop-Location }
        foreach ($key in $Environment.Keys) {
            if ($null -eq $previous[$key]) {
                Remove-Item -LiteralPath "Env:$key" -ErrorAction SilentlyContinue
            } else {
                Set-Item -LiteralPath "Env:$key" -Value $previous[$key]
            }
        }
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
channel = "1.95"
'@
Write-Fixture (Join-Path $repo 'Justfile') @'
set unstable

# A repository-owned recipe, to prove that forwarded values arrive.
e2e-show-env:
    @echo "E2E:$ANVIL_E2E_RUNTIME"

# Proves the driver forwards a host token, and invents one when it should not.
# `:-` because just runs recipe lines under `sh -u`, where a bare $NAME that
# was correctly *not* forwarded would abort instead of printing empty.
e2e-show-token:
    @echo "E2E-TOKEN:[${GITHUB_TOKEN:-}]"
'@

Invoke-Native -Command 'git' -Arguments @('init', '-q') -WorkingDirectory $repo | Out-Null

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
$firstRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'anvil-fmt')
Write-Detail (($firstRun.StdErr -split "`r?`n" | Select-Object -Last 4) -join "`n")

Assert-Equal 'anvil-fmt succeeds inside the container' 0 $firstRun.ExitCode
Assert-That 'the run reported building the image' ($firstRun.StdErr -match 'building .*(inputs changed|first run)')
Assert-That 'image is present afterwards' (Test-ImagePresent $reference)

# ------------------------------------------------------------ 3. second run --

Write-Section '3. Second run reuses the image'

$secondRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'anvil-fmt')
Assert-Equal 'anvil-fmt succeeds again' 0 $secondRun.ExitCode
Assert-That 'nothing was rebuilt' (-not ($secondRun.StdErr -match 'building ')) $secondRun.StdErr
Assert-Equal 'the reference is unchanged' $reference (Get-ImageReference -Repo $repo)

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

# anvil-aprz runs in pr-fast and is rate-limited without a token, so a host
# token has to reach the container -- but only one the host actually set.
$withToken = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'e2e-show-token') `
    -Environment @{ GITHUB_TOKEN = 'e2e-forwarded-token' }
Assert-That 'a host GITHUB_TOKEN reaches a recipe in the container' `
    ($withToken.StdOut -match 'E2E-TOKEN:\[e2e-forwarded-token\]') "$($withToken.StdOut)$($withToken.StdErr)"

$withoutToken = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'e2e-show-token') `
    -Environment @{ GITHUB_TOKEN = '' }
Assert-That 'no token is invented when the host has none' `
    ($withoutToken.StdOut -match 'E2E-TOKEN:\[\]') "$($withoutToken.StdOut)$($withoutToken.StdErr)"

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

# ------------------------------------------------- 6. editing the Dockerfile --

Write-Section '6. A repository can edit the Dockerfile'

$dockerfileBody = Get-Content -LiteralPath $dockerfile -Raw
Write-Fixture $dockerfile ($dockerfileBody + "`n# a repository-owned edit`n")
$editedReference = Get-ImageReference -Repo $repo
Assert-That 'editing the Dockerfile selects a new tag' ($editedReference -ne $reference)

Write-Step 're-running the generator over the edited file'
Invoke-Native -Command $anvilExe -Arguments @('anvil', '--no-backends') -WorkingDirectory $repo -AllowFailure | Out-Null
$afterRegen = Get-Content -LiteralPath $dockerfile -Raw
Assert-That 'the edit survives regeneration' ($afterRegen -match 'a repository-owned edit') `
    'anvil must preserve a user-modified owned file'

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
Write-Fixture $hooks @'
function Anvil-PreBuild {
    @{ Secrets = @{ e2e_token = 'build-secret-value' } }
}

function Anvil-PreRun {
    @{ Env = @{ ANVIL_E2E_RUNTIME = 'run-value' } }
}
'@

$hookReference = Get-ImageReference -Repo $repo
Assert-That 'adding a hook selects a new tag' ($hookReference -ne $reference) `
    "the hook file's content must be part of the image identity"

# The default Dockerfile does not consume the secret, so prove the wiring by
# having the image read it. This is a fixture-side Dockerfile edit, which is a
# supported user action (proved in section 6).
$secretStanza = @'

# --- e2e: prove the build secret arrives and never lands in a layer ---
RUN --mount=type=secret,id=e2e_token,required=true \
    test -s /run/secrets/e2e_token \
    && echo "e2e: secret length $(wc -c < /run/secrets/e2e_token)"
'@
Write-Fixture $dockerfile ($dockerfileBody + $secretStanza)

Write-Step 'rebuilding with the hook active'
$hookRun = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'anvil-fmt') -AllowFailure
Assert-Equal 'the run with a hook succeeds' 0 $hookRun.ExitCode
Assert-That 'the hook announced itself at build time' ($hookRun.StdErr -match 'Anvil-PreBuild')
Assert-That 'the build secret was declared' ($hookRun.StdErr -match 'build secrets: e2e_token')
Assert-That 'the hook announced itself at run time' ($hookRun.StdErr -match 'Anvil-PreRun')
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
$showEnv = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'e2e-show-env') -AllowFailure
Assert-That 'the run-time value reaches a recipe in the container' `
    ($showEnv.StdOut -match 'E2E:run-value') "stdout: $($showEnv.StdOut)`nstderr: $($showEnv.StdErr)"

# ------------------------------------------------------ 8. hook fails closed --

Write-Section '8. An empty hook value fails closed'

Write-Fixture $hooks @'
function Anvil-PreBuild {
    @{ Secrets = @{ e2e_token = '' } }
}
'@

$emptyHook = Invoke-Just -Repo $repo -Arguments @('anvil-container', 'anvil-fmt') -AllowFailure
Assert-That 'an empty secret aborts the run' ($emptyHook.ExitCode -ne 0) `
    'BuildKit would mount an empty secret and exit 0, tagging a degraded image with a valid hash'
Assert-That 'the failure names the offending secret' ($emptyHook.StdErr -match "empty value for secret 'e2e_token'") `
    $emptyHook.StdErr

# ------------------------------------------- 9. hook output is not the tag ---

Write-Section '9. Hook output does not change the tag'

Write-Fixture $hooks @'
function Anvil-PreBuild {
    @{ Secrets = @{ e2e_token = 'build-secret-value' } }
}

function Anvil-PreRun {
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
function Anvil-PreBuild {
    @{ Secrets = @{ e2e_token = $env:ANVIL_E2E_MINT } }
}

function Anvil-PreRun {
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
$nestedReference = if ($buildSecretsSupported) { $secretReference } else { Get-ImageReference -Repo $repo }
$nested = Invoke-Engine -Arguments @(
    'run', '--rm', '-e', 'ANVIL_IN_CONTAINER=1', '-v', "$(ConvertTo-EnginePath $repo):/workspace", '-w', '/workspace',
    $nestedReference, 'just', 'anvil-container', 'anvil-fmt'
) -AllowFailure
Assert-Equal 'anvil-container passes through inside the image' 0 $nested.ExitCode
Assert-That 'no engine was invoked from inside the container' `
    (-not ($nested.StdErr -match 'building |Cannot connect to the Docker daemon')) $nested.StdErr

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
