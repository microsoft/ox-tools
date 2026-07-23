# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$Recipe
)

$ErrorActionPreference = 'Stop'

function ConvertTo-PosixShellArg([string]$Value) {
    $singleQuote = [string][char]39
    $doubleQuote = [string][char]34
    $escape = $singleQuote + $doubleQuote + $singleQuote + $doubleQuote + $singleQuote
    $singleQuote + $Value.Replace($singleQuote, $escape) + $singleQuote
}

function ConvertTo-AnvilVersion([string]$Value) {
    $match = [regex]::Match($Value, '^(\d+)\.(\d+)(?:\.(\d+))?')
    if (-not $match.Success) {
        throw "anvil-container: could not parse Podman version '$Value'."
    }
    [version]::new(
        [int]$match.Groups[1].Value,
        [int]$match.Groups[2].Value,
        $(if ($match.Groups[3].Success) { [int]$match.Groups[3].Value } else { 0 })
    )
}

function Test-AnvilContainerStringArray([string]$Name, $Value) {
    if ($Value -isnot [array]) {
        throw "anvil-container: `$$Name must be a string array."
    }
    foreach ($item in $Value) {
        if ($item -isnot [string] -or [string]::IsNullOrEmpty($item)) {
            throw "anvil-container: `$$Name entries must be non-empty strings."
        }
    }
}

function Test-AnvilRecipeNeedsGitHubToken([string]$Name) {
    $Name -in @(
        'anvil-aprz',
        'anvil-pr',
        '_anvil-pr',
        'anvil-pr-fast',
        'anvil-scheduled',
        '_anvil-scheduled',
        'anvil-scheduled-advisories',
        'anvil-full',
        '_anvil-full'
    )
}

function Get-AnvilGitHubToken {
    $token = $env:GITHUB_TOKEN
    if (-not $token -and (Get-Command gh -ErrorAction SilentlyContinue)) {
        try {
            $token = (& gh auth token --hostname github.com 2>$null)
            if ($LASTEXITCODE -ne 0) { $token = $null }
        } catch {
            $token = $null
        }
    }
    if ($token) { $token = $token.Trim() }
    if ($token) { return $token }
    return $null
}

if ($env:ANVIL_IN_CONTAINER) {
    if ($Recipe.Count -eq 0) { & bash } else { & just @Recipe }
    exit $LASTEXITCODE
}

if ($Recipe.Count -gt 0 -and $Recipe[0] -notmatch '^_?anvil-[A-Za-z0-9-]+$') {
    throw "anvil-container: expected an anvil-* recipe, got '$($Recipe[0])'."
}

if (-not (Get-Command podman -ErrorAction SilentlyContinue)) {
    throw 'anvil-container: Podman is required. See justfiles/anvil/container/README.md.'
}

$versionText = (podman version --format '{{.Client.Version}}' 2>$null)
if (-not $versionText) { $versionText = (podman --version) -replace '^podman version ', '' }
if ((ConvertTo-AnvilVersion $versionText) -lt [version]'4.3.0') {
    throw "anvil-container: Podman 4.3.0 or newer is required (found $versionText)."
}

$repoRoot = (git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0 -or -not $repoRoot) {
    throw 'anvil-container must run from a Git repository.'
}

$scriptDir = Join-Path $repoRoot 'justfiles/anvil/container'
$imageId = (& (Join-Path $scriptDir 'image-id.ps1')).Trim()
$imageBase = if ($env:ANVIL_CONTAINER_IMAGE) { $env:ANVIL_CONTAINER_IMAGE } else { 'anvil-dev' }
$image = "${imageBase}:$imageId"
$repoBytes = [Text.Encoding]::UTF8.GetBytes($repoRoot.ToLowerInvariant())
$repoHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($repoBytes)).ToLowerInvariant()
$targetVolume = "anvil-target-$($repoHash.Substring(0, 12))-$($imageId.Substring(0, 12))"

$needsGitHubToken = $false
foreach ($name in $Recipe) {
    if (Test-AnvilRecipeNeedsGitHubToken $name) {
        $needsGitHubToken = $true
        break
    }
}
$runsOnlyGitHubCheck = $Recipe.Count -eq 1 -and $Recipe[0] -eq 'anvil-aprz'
$githubToken = if ($needsGitHubToken) { Get-AnvilGitHubToken } else { $null }
if ($needsGitHubToken -and -not $githubToken) {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        throw 'anvil-container: GitHub authentication is required for anvil-aprz. Install the GitHub CLI and run `gh auth login --hostname github.com`, or set GITHUB_TOKEN before rerunning.'
    }
    if (-not [Environment]::UserInteractive -or [Console]::IsInputRedirected) {
        throw 'anvil-container: GitHub authentication is required for anvil-aprz. Run `gh auth login --hostname github.com` or set GITHUB_TOKEN before rerunning.'
    }
    Write-Host 'anvil-container: anvil-aprz requires GitHub authentication to avoid the 60 requests/hour unauthenticated API limit.'
    [void](Read-Host 'Run `gh auth login --hostname github.com` in another terminal, then press Enter to continue (Ctrl+C to cancel)')
    $githubToken = Get-AnvilGitHubToken
    if (-not $githubToken) {
        throw 'anvil-container: GitHub authentication is still unavailable. Complete `gh auth login --hostname github.com`, then rerun.'
    }
}

# Versioned customization contract: check warm/cold state before sourcing so
# customization needed only for image construction can be skipped on a warm
# run, then expose read-only inputs. See docs/design/containers.md.
& podman image exists $image
$imageExists = $LASTEXITCODE -eq 0

New-Variable -Name AnvilContainerCustomizationApiVersion -Value 1 -Option ReadOnly
New-Variable -Name AnvilContainerRepoRoot -Value $repoRoot -Option ReadOnly
New-Variable -Name AnvilContainerDir -Value $scriptDir -Option ReadOnly
New-Variable -Name AnvilContainerResolvedImage -Value $image -Option ReadOnly
New-Variable -Name AnvilContainerImageExists -Value $imageExists -Option ReadOnly
New-Variable -Name AnvilContainerRequestedRecipes -Value $Recipe -Option ReadOnly
New-Variable -Name AnvilContainerHostIsWindows -Value ([bool]$IsWindows) -Option ReadOnly

# Customization outputs, initialized before sourcing so a missing customize.ps1
# leaves every phase a documented no-op.
$AnvilContainerBuildArgs = @()
$AnvilContainerPrepareArgs = @()
$AnvilContainerPrepareCommand = @()
$AnvilContainerRunArgs = @()
$AnvilContainerBuildInMachine = $false
$AnvilContainerCleanup = $null
$githubTokenFile = $null
$exitCode = 0
$customizeScript = Join-Path $scriptDir 'customize.ps1'

try {
    if (Test-Path -LiteralPath $customizeScript -PathType Leaf) {
        . $customizeScript
    }

    Test-AnvilContainerStringArray 'AnvilContainerBuildArgs' $AnvilContainerBuildArgs
    Test-AnvilContainerStringArray 'AnvilContainerPrepareArgs' $AnvilContainerPrepareArgs
    Test-AnvilContainerStringArray 'AnvilContainerPrepareCommand' $AnvilContainerPrepareCommand
    Test-AnvilContainerStringArray 'AnvilContainerRunArgs' $AnvilContainerRunArgs
    if ($AnvilContainerPrepareArgs.Count -gt 0 -and $AnvilContainerPrepareCommand.Count -eq 0) {
        throw 'anvil-container: $AnvilContainerPrepareArgs requires $AnvilContainerPrepareCommand.'
    }
    if ($AnvilContainerCleanup -and $AnvilContainerCleanup -isnot [scriptblock]) {
        throw 'anvil-container: $AnvilContainerCleanup must be a script block.'
    }
    if ($AnvilContainerBuildInMachine -isnot [bool]) {
        throw 'anvil-container: $AnvilContainerBuildInMachine must be a Boolean.'
    }

    if (-not $imageExists) {
        if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {
            throw "anvil-container: image $image is missing and ANVIL_CONTAINER_NO_REBUILD=1."
        }
        if ($AnvilContainerBuildInMachine) {
            $machineRepo = (wsl -e wslpath -a $repoRoot).Trim()
            $buildArgs = @(
                'podman', 'build',
                '--platform', 'linux/amd64',
                '--tag', $image,
                '--file', 'justfiles/anvil/container/Containerfile',
                '--ignorefile', 'justfiles/anvil/container/container.ignore',
                '--build-arg', "ANVIL_IMAGE_ID=$imageId"
            )
            $buildArgs += $AnvilContainerBuildArgs
            $buildArgs += '.'
            $buildCommand = ($buildArgs | ForEach-Object { ConvertTo-PosixShellArg $_ }) -join ' '
            $command = "cd $(ConvertTo-PosixShellArg $machineRepo) && $buildCommand"
            & podman machine ssh -- $command
        } else {
            & podman build `
                --platform linux/amd64 `
                --tag $image `
                --file (Join-Path $scriptDir 'Containerfile') `
                --ignorefile (Join-Path $scriptDir 'container.ignore') `
                --build-arg "ANVIL_IMAGE_ID=$imageId" `
                @AnvilContainerBuildArgs `
                $repoRoot
        }
        if ($LASTEXITCODE -ne 0) {
            throw "anvil-container: Podman build failed with exit code $LASTEXITCODE."
        }
    }

    $runArgs = @(
        'run', '--rm', '--pull=never',
        '--platform', 'linux/amd64',
        '--userns', 'keep-id',
        '--env', 'ANVIL_IN_CONTAINER=1',
        '--env', 'HOME=/tmp/anvil-user',
        '--volume', "${repoRoot}:/workspace:Z",
        '--volume', 'anvil-cargo-registry:/usr/local/cargo/registry:U',
        '--volume', 'anvil-cargo-git:/usr/local/cargo/git:U',
        '--volume', "${targetVolume}:/workspace/target:U",
        '--workdir', '/workspace'
    )
    $prepareRunArgs = @($runArgs)
    $runArgs += $AnvilContainerRunArgs
    foreach ($name in @(
        'PR_TITLE',
        'BASE_REF',
        'ANVIL_INCLUDE_MODIFIED',
        'ANVIL_INCLUDE_AFFECTED',
        'ANVIL_INCLUDE_REQUIRED',
        'GITHUB_BASE_REF',
        'SYSTEM_PULLREQUEST_TARGETBRANCH'
    )) {
        if (Test-Path "Env:$name") { $runArgs += @('--env', $name) }
    }
    if ($AnvilContainerPrepareCommand.Count -gt 0) {
        & podman @prepareRunArgs @AnvilContainerPrepareArgs $image @AnvilContainerPrepareCommand
        if ($LASTEXITCODE -ne 0) {
            throw "anvil-container: preparation command failed with exit code $LASTEXITCODE."
        }
    }

    if ($githubToken) {
        $githubTokenFile = Join-Path ([IO.Path]::GetTempPath()) "anvil-github-token-$PID-$([guid]::NewGuid().ToString('N'))"
        [IO.File]::WriteAllText($githubTokenFile, $githubToken, [Text.Encoding]::ASCII)
        if ($IsWindows) {
            $userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
            & icacls.exe $githubTokenFile '/inheritance:r' '/grant:r' "*$($userSid):(F)" | Out-Null
        } else {
            & chmod 600 $githubTokenFile
        }
        if ($LASTEXITCODE -ne 0) {
            throw 'anvil-container: failed to restrict permissions on the temporary GitHub token file.'
        }
        $githubToken = $null
        $githubRunArgs = @($runArgs)
        $githubRunArgs += @(
            '--mount',
            "type=bind,src=$githubTokenFile,dst=/run/secrets/anvil-github-token,readonly"
        )
        if ($runsOnlyGitHubCheck) {
            $runArgs = $githubRunArgs
        } else {
            & podman @githubRunArgs $image just anvil-aprz
            if ($LASTEXITCODE -ne 0) {
                throw "anvil-container: isolated anvil-aprz failed with exit code $LASTEXITCODE."
            }
            $runArgs += @('--env', 'ANVIL_APRZ_ALREADY_RAN=1')
        }
    }

    if ($Recipe.Count -eq 0) {
        & podman @runArgs --interactive --tty $image bash
    } else {
        & podman @runArgs $image just @Recipe
    }
    $exitCode = $LASTEXITCODE
} finally {
    if ($githubTokenFile) {
        Remove-Item -LiteralPath $githubTokenFile -Force -ErrorAction SilentlyContinue
    }
    if ($AnvilContainerCleanup) { & $AnvilContainerCleanup }
}

exit $exitCode
