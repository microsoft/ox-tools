# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
    Dogfood test: run this repository's own anvil checks inside the container.

.DESCRIPTION
    `test-anvil-container.ps1` covers the container mechanism against a
    throwaway fixture: artifacts, tags, drift, hooks. This script covers the
    other half -- that the mechanism works on a real workspace. It runs the
    generated recipes against `ox-tools` itself: a multi-crate workspace with
    real dependencies, real lints, and the full pinned tool catalog.

    It runs against both engines by default. On Windows that also covers both
    invocation paths, since docker is reached through the default WSL
    distribution and podman runs natively.

    Steps are ordered by cost, so a break is reported in seconds rather than
    after a full tier:

      1. Preconditions -- the generated tree is current, and the container
         artifacts are present.
      2. The image builds from the repository's own Dockerfile.
      3. Every pinned tool in the catalog executes inside the image.
      4. `anvil-aprz` runs, exercising a prebuilt binary and the advisory API.
      5. Every recipe file defines the image: editing a check, a tier, the
         driver, `tools.just` or `versions.just` must all rename it, and
         dropping a check's `-setup` dependency from a group -- which changes
         the installed tool set while leaving `tools.just` untouched -- must
         rename it too.
      6. The requested tier runs to completion inside the image.
      7. A second run reuses the image rather than rebuilding it.

    Step 3 is cheap and broad: `anvil-setup` proves a tool downloaded, while
    executing it proves the image can run it. Twenty tools cost seconds here
    and would otherwise surface one at a time as tiers reach them.

.PARAMETER Engine
    Which engine(s) to test. 'both' (default) runs the suite against docker and
    then podman, reporting them separately.

.PARAMETER Tier
    Recipe(s) to run for step 5. Defaults to `anvil-pr`, the full PR tier,
    which includes mutants and runtime analysis and is measured in tens of
    minutes. `anvil-pr-fast` covers the same plumbing more quickly.

.PARAMETER SkipTier
    Stop after step 4. The cheap steps cover the container contract.

.PARAMETER KeepImages
    Leave built images and cache volumes in place.

.PARAMETER Clean
    Remove this repository's anvil images and cache volumes before starting,
    forcing a cold build. Use when testing the image definition itself.

.EXAMPLE
    ./scripts/test-anvil-dogfood.ps1
    ./scripts/test-anvil-dogfood.ps1 -Engine docker -Tier anvil-pr-fast
    ./scripts/test-anvil-dogfood.ps1 -SkipTier -Clean
#>

[CmdletBinding()]
param(
    [ValidateSet('docker', 'podman', 'both')]
    [string]$Engine = 'both',
    [string[]]$Tier = @('anvil-pr'),
    [switch]$SkipTier,
    [switch]$KeepImages,
    [switch]$Clean
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# ---------------------------------------------------------------- reporting --

$script:Passed = 0
$script:Failed = 0
$script:Skipped = 0
$script:Started = Get-Date
$script:Results = [ordered]@{}
$script:CurrentEngine = ''

function Write-Section([string]$Title) {
    Write-Host ''
    Write-Host "=== $Title " -NoNewline -ForegroundColor Cyan
    Write-Host ('=' * [Math]::Max(0, 72 - $Title.Length)) -ForegroundColor Cyan
}

function Write-Step([string]$Message) {
    Write-Host "  -> $Message" -ForegroundColor DarkGray
}

function Write-Detail([string]$Message) {
    if (-not $Message) { return }
    foreach ($line in ($Message -split "`r?`n")) {
        if ($line.Trim()) { Write-Host "     | $line" -ForegroundColor DarkGray }
    }
}

function Write-Tail([string]$Message, [int]$Lines = 25) {
    if (-not $Message) { return }
    $all = @($Message -split "`r?`n" | Where-Object { $_.Trim() })
    Write-Detail (($all | Select-Object -Last $Lines) -join "`n")
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

function Write-Skipped([string]$Name, [string]$Why) {
    $script:Skipped++
    Write-Host "  [SKIP] $Name" -ForegroundColor Yellow
    Write-Detail $Why
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

function Resolve-Engine([string]$Name) {
    # Mirrors what container.just does: prefer the engine on PATH, and fall
    # back to the default WSL distribution on Windows. The script must not
    # assume more than the product does.
    if (Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue) {
        return [pscustomobject]@{ Exe = $Name; Prefix = @(); ViaWsl = $false }
    }
    if ($IsWindows -and (Get-Command wsl.exe -CommandType Application -ErrorAction SilentlyContinue)) {
        & wsl.exe --exec $Name --version *> $null
        if ($LASTEXITCODE -eq 0) {
            return [pscustomobject]@{ Exe = 'wsl.exe'; Prefix = @('--exec', $Name); ViaWsl = $true }
        }
    }
    $null
}

function Invoke-Engine {
    param([string[]]$Arguments, [switch]$AllowFailure)
    Invoke-Native -Command $script:EngineExe -Arguments ($script:EnginePrefix + $Arguments) -AllowFailure:$AllowFailure
}

function Invoke-Just {
    param(
        [Parameter(Mandatory)][string[]]$Arguments,
        [hashtable]$Environment = @{},
        [switch]$AllowFailure
    )
    $env = @{ ANVIL_CONTAINER_ENGINE = $script:CurrentEngine } + $Environment
    Invoke-Native -Command 'just' -Arguments $Arguments -WorkingDirectory $RepoRoot -Environment $env -AllowFailure:$AllowFailure
}

function Get-ImageReference {
    # anvil-container-status reports the reference without building it.
    $status = Invoke-Just -Arguments @('anvil-container-status') -AllowFailure
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
    # Podman reports images fully qualified (`localhost/anvil-...`), docker does
    # not, so match anywhere in the reference rather than at the start.
    foreach ($image in (($images.StdOut -split "`r?`n") | Where-Object { $_ -like "*$Prefix*" })) {
        Write-Step "removing image $image"
        Invoke-Engine -Arguments @('rmi', '-f', $image) -AllowFailure | Out-Null
    }
    $volumes = Invoke-Engine -Arguments @('volume', 'ls', '--format', '{{.Name}}') -AllowFailure
    foreach ($volume in (($volumes.StdOut -split "`r?`n") | Where-Object { $_ -like "*$Prefix*" })) {
        Write-Step "removing volume $volume"
        Invoke-Engine -Arguments @('volume', 'rm', '-f', $volume) -AllowFailure | Out-Null
    }
}

# The pinned catalog, read from the generated recipes rather than restated
# here. A tool added to the catalog is covered without editing this script --
# the same reason the image installs by running `anvil-setup` instead of
# carrying its own list.
function Get-PinnedTools {
    $versions = Join-Path $RepoRoot 'justfiles/anvil/versions.just'
    $tools = [ordered]@{}
    foreach ($line in (Get-Content -LiteralPath $versions)) {
        if ($line -match '^\s*([a-z0-9_]+)_version\s*:=\s*"([^"]+)"') {
            $name = $Matches[1] -replace '_', '-'
            if ($name -like 'cargo-*') { $tools[$name] = $Matches[2] }
        }
    }
    $tools
}

# ---------------------------------------------------------------- the suite --

function Invoke-Suite([string]$EngineName) {
    $script:CurrentEngine = $EngineName
    $before = $script:Failed

    Write-Section "$EngineName : preconditions"

    $resolved = Resolve-Engine $EngineName
    if (-not $resolved) {
        Write-Skipped "$EngineName is available" "not on PATH, and not reachable in the default WSL distribution"
        $script:Results[$EngineName] = 'skipped'
        return
    }
    $script:EngineExe = $resolved.Exe
    $script:EnginePrefix = $resolved.Prefix
    $script:EngineViaWsl = $resolved.ViaWsl
    Write-Step ("engine: {0}{1}" -f $EngineName, $(if ($resolved.ViaWsl) { ' (via WSL)' } else { ' (native)' }))

    # The dogfood claim is only meaningful if the committed tree is what the
    # generator produces. A stale tree would test something no user can obtain.
    $dryRun = Invoke-Native -Command 'cargo' -Arguments @('run', '--quiet', '-p', 'cargo-anvil', '--', 'anvil', '--dry-run') `
        -WorkingDirectory $RepoRoot -AllowFailure
    Assert-Equal 'the committed anvil tree is current (cargo anvil --dry-run)' 0 $dryRun.ExitCode

    foreach ($artifact in @('.anvil/container/Dockerfile', '.anvil/container/Dockerfile.dockerignore', 'justfiles/anvil/container.just')) {
        Assert-That "$artifact is present" (Test-Path -LiteralPath (Join-Path $RepoRoot $artifact))
    }

    $imagePrefix = 'anvil-' + (Split-Path -Leaf $RepoRoot).ToLowerInvariant()
    if ($Clean) {
        Write-Step 'removing existing images and volumes for a cold build'
        Remove-AnvilImages -Prefix $imagePrefix
    }

    Write-Section "$EngineName : image"

    $reference = Get-ImageReference
    Assert-That 'anvil-container-status reports an image reference' ([bool]$reference) 'no image: line in status output'
    Write-Step "reference: $reference"

    $up = Invoke-Just -Arguments @('anvil-container', 'anvil-container-tag') -AllowFailure
    Assert-Equal 'the first run builds the image if it is missing' 0 $up.ExitCode
    if ($up.ExitCode -ne 0) {
        Write-Tail $up.StdErr
        # Everything below needs an image; stop this engine rather than
        # reporting a cascade of failures that all have one cause.
        $script:Results[$EngineName] = 'failed'
        return
    }
    Assert-That 'the image is present after the first run' (Test-ImagePresent $reference)

    Write-Section "$EngineName : the catalog executes inside the image"

    # `anvil-setup` proves a tool downloaded; executing it proves the image can
    # run it. The catalog is installed as prebuilt binaries, so a base older
    # than the runner they were built on yields tools that are present and
    # unrunnable.
    #
    # This runs the engine directly rather than through `anvil-container`,
    # which dispatches `just <recipe>` and so cannot invoke a bare binary. The
    # image reference still comes from the product (`anvil-container-status`),
    # and the property under test belongs to the image rather than the recipe.
    $tools = Get-PinnedTools
    Write-Step "$($tools.Count) pinned tools"
    $broken = @()
    $missing = @()
    foreach ($tool in $tools.Keys) {
        $run = Invoke-Engine -Arguments @('run', '--rm', $reference, $tool, '--version') -AllowFailure
        $combined = "$($run.StdOut)`n$($run.StdErr)"
        # The dynamic loader reports an ABI mismatch before main() runs, so
        # this is independent of whether a tool implements --version at all.
        if ($combined -match 'GLIBC_[0-9.]+.{0,40}not found|error while loading shared libraries|cannot execute binary file') {
            $line = (($combined -split "`r?`n") | Where-Object { $_ -match 'GLIBC|shared libraries|cannot execute' } | Select-Object -First 1).Trim()
            $broken += "$tool -> $line"
        } elseif ($combined -match 'executable file .*not found|no such file or directory') {
            $missing += $tool
        }
    }
    Assert-That 'every pinned tool executes inside the image' ($broken.Count -eq 0) ($broken -join "`n")
    Assert-That 'every pinned tool is present in the image' ($missing.Count -eq 0) ($missing -join ', ')

    Write-Section "$EngineName : checks"

    # The check whose prebuilt binary exercises both the loader and the
    # advisory API. Kept as its own step so a regression names itself.
    $aprz = Invoke-Just -Arguments @('anvil-container', 'anvil-aprz') -AllowFailure
    Assert-Equal 'anvil-aprz runs inside the image' 0 $aprz.ExitCode
    if ($aprz.ExitCode -ne 0) { Write-Tail "$($aprz.StdOut)`n$($aprz.StdErr)" }

    # A check that reads the workspace rather than the network, so a failure
    # points at the mount rather than at connectivity.
    $fmt = Invoke-Just -Arguments @('anvil-container', 'anvil-fmt') -AllowFailure
    Assert-Equal 'anvil-fmt runs inside the image' 0 $fmt.ExitCode
    if ($fmt.ExitCode -ne 0) { Write-Tail "$($fmt.StdOut)`n$($fmt.StdErr)" }

    Write-Section "$EngineName : image reuse"

    $secondReference = Get-ImageReference
    Assert-Equal 'the tag is stable across runs' $reference $secondReference
    $reuse = Invoke-Just -Arguments @('anvil-container', 'anvil-fmt') -AllowFailure
    Assert-Equal 'a later run succeeds' 0 $reuse.ExitCode
    Assert-That 'a later run does not rebuild the image' `
        (-not ("$($reuse.StdOut)`n$($reuse.StdErr)" -match 'building |Step 1/|FROM ')) `
        'a rebuild happened when the tag should have resolved'

    Write-Section "$EngineName : every recipe file defines the image"

    # `just anvil-setup` reaches the install recipes through the tier, group and
    # check recipes, so the routing decides *whether* a tool is installed as
    # surely as tools.just decides *how*. Hashing only the install definitions
    # let a group drop an `anvil-<check>-setup` dependency -- changing the
    # installed set -- while the tag stayed byte-identical, so the stale image
    # was reused forever. The whole tree is hashed for that reason, and these
    # cases are what keep it that way.
    #
    # Edits are made against a byte copy and restored from it. Never
    # `git checkout --` on a generated file: that restores the last *commit*,
    # not the generated state, and anvil then preserves the stale file as a
    # user modification. If this script is killed mid-section, regenerate with
    # `cargo run -p cargo-anvil -- anvil` to return the tree to a known state.
    $baseline = Get-ImageReference
    Assert-That 'a baseline tag is available' ([bool]$baseline)

    $cases = @(
        @{ File = 'justfiles/anvil/checks/clippy.just'; Why = 'a check carries the setup dependency that installs its tool' }
        @{ File = 'justfiles/anvil/container.just';     Why = 'the driver passes the build args, secrets and PreBuild output into the build' }
        @{ File = 'justfiles/anvil/tiers.just';         Why = 'a tier decides which groups, and so which setups, are reached' }
        @{ File = 'justfiles/anvil/versions.just';      Why = 'a pin decides which build is installed' }
        @{ File = 'justfiles/anvil/tools.just';         Why = 'the install recipes decide what is installed' }
    )

    foreach ($case in $cases) {
        $path = Join-Path $RepoRoot $case.File
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Write-Skipped "$($case.File) is present" 'not emitted by this catalog'
            continue
        }
        $backup = [System.IO.Path]::GetTempFileName()
        Copy-Item -LiteralPath $path -Destination $backup -Force
        try {
            Add-Content -LiteralPath $path -Value "`n# dogfood scratch"
            $edited = Get-ImageReference
            Assert-That "editing $($case.File) renames the image" ($edited -ne $baseline) `
                "$($case.Why); tag stayed $edited"
        } finally {
            Copy-Item -LiteralPath $backup -Destination $path -Force
            Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
        }
    }

    Assert-Equal 'restoring every file restores the original tag' $baseline (Get-ImageReference)

    # The regression that motivated hashing the whole tree, stated in the terms
    # it actually occurred in: a group drops a check's `-setup` dependency, so
    # the image installs one tool fewer, while tools.just and versions.just are
    # untouched. The tag has to move or the reduced image is reused forever.
    $group = Join-Path $RepoRoot 'justfiles/anvil/groups/pr-fast.just'
    if (Test-Path -LiteralPath $group -PathType Leaf) {
        $groupBackup = [System.IO.Path]::GetTempFileName()
        Copy-Item -LiteralPath $group -Destination $groupBackup -Force
        try {
            $kept = @(Get-Content -LiteralPath $group | Where-Object { $_ -notmatch 'anvil-spellcheck-setup' })
            Set-Content -LiteralPath $group -Value $kept
            Assert-That 'dropping a setup dependency renames the image' ((Get-ImageReference) -ne $baseline) `
                'the installed tool set changed while the tag did not'
        } finally {
            Copy-Item -LiteralPath $groupBackup -Destination $group -Force
            Remove-Item -LiteralPath $groupBackup -Force -ErrorAction SilentlyContinue
        }
        Assert-Equal 'restoring the group restores the original tag' $baseline (Get-ImageReference)
    } else {
        Write-Skipped 'dropping a setup dependency renames the image' 'no pr-fast group in this catalog'
    }

    if ($SkipTier) {
        Write-Skipped "$EngineName : tier" '-SkipTier was passed'
    } else {
        Write-Section "$EngineName : tier"
        foreach ($recipe in $Tier) {
            Write-Step "running $recipe (this is the long one)"
            $started = Get-Date
            $run = Invoke-Just -Arguments @('anvil-container', $recipe) -AllowFailure
            $took = (Get-Date) - $started
            Assert-Equal ("{0} passes inside the image (took {1:mm\:ss})" -f $recipe, $took) 0 $run.ExitCode
            if ($run.ExitCode -ne 0) { Write-Tail "$($run.StdOut)`n$($run.StdErr)" 40 }
        }
    }

    $script:Results[$EngineName] = $(if ($script:Failed -gt $before) { 'failed' } else { 'passed' })
}

# -------------------------------------------------------------------- main ----

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

Write-Host ''
Write-Host 'anvil containerized execution - dogfood against this repository' -ForegroundColor White
Write-Host "repository: $RepoRoot" -ForegroundColor DarkGray
Write-Host "tier:       $(if ($SkipTier) { '(skipped)' } else { $Tier -join ', ' })" -ForegroundColor DarkGray

foreach ($tool in @('just', 'cargo')) {
    if (-not (Get-Command $tool -CommandType Application -ErrorAction SilentlyContinue)) {
        Write-Host "FAIL  $tool is required on PATH" -ForegroundColor Red
        exit 1
    }
}

$engines = if ($Engine -eq 'both') { @('docker', 'podman') } else { @($Engine) }
foreach ($name in $engines) {
    Invoke-Suite $name
}

if (-not $KeepImages -and -not $Clean) {
    # The image is expensive to build and is keyed by content, so keeping it is
    # correct: the next run reuses it, and an input change renames it anyway.
    Write-Step 'keeping built images (content-addressed; -Clean forces a cold build)'
}

$elapsed = (Get-Date) - $script:Started
Write-Host ''
Write-Host ('-' * 78)
foreach ($name in $script:Results.Keys) {
    $state = $script:Results[$name]
    $color = switch ($state) { 'passed' { 'Green' } 'skipped' { 'Yellow' } default { 'Red' } }
    Write-Host ("  {0,-8} {1}" -f $name, $state) -ForegroundColor $color
}
$summary = "{0}/{1} checks passed in {2:hh\:mm\:ss}" -f $script:Passed, ($script:Passed + $script:Failed), $elapsed
if ($script:Skipped) { $summary += " ($($script:Skipped) skipped)" }
if ($script:Failed -eq 0) {
    Write-Host "PASS  $summary" -ForegroundColor Green
    exit 0
}
Write-Host "FAIL  $summary ($($script:Failed) failed)" -ForegroundColor Red
exit 1
