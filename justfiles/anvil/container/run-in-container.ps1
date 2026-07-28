# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$Recipe
)

$ErrorActionPreference = 'Stop'

function ConvertTo-AnvilVersion([string]$Value) {
    $match = [regex]::Match($Value, '^(\d+)\.(\d+)(?:\.(\d+))?')
    if (-not $match.Success) {
        throw "anvil-container: could not parse Docker Engine version '$Value'."
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

if (-not (Get-Command wsl -ErrorAction SilentlyContinue)) {
    throw 'anvil-container: WSL 2 is required. See justfiles/anvil/container/README.md.'
}

$versionText = (& wsl -e docker version --format '{{.Server.Version}}' 2>$null)
if ($LASTEXITCODE -ne 0 -or -not $versionText) {
    throw 'anvil-container: `wsl -e docker version` must succeed. Install or start Docker Engine in the default WSL distribution; this driver does not invoke Windows docker.exe.'
}
$versionText = $versionText.Trim()
if ((ConvertTo-AnvilVersion $versionText) -lt [version]'23.0.0') {
    throw "anvil-container: Docker Engine 23.0.0 or newer is required (found $versionText)."
}
$wslArchitecture = (& wsl -e uname -m 2>$null)
if ($LASTEXITCODE -eq 0 -and $wslArchitecture) {
    $wslArchitecture = $wslArchitecture.Trim()
    if ($wslArchitecture -notin @('x86_64', 'amd64')) {
        [Console]::Error.WriteLine(
            "anvil-container: warning: $wslArchitecture requires emulation for linux/amd64; builds and checks may be substantially slower."
        )
    }
}

$repoRoot = (git rev-parse --show-toplevel 2>$null).Trim()
if ($LASTEXITCODE -ne 0 -or -not $repoRoot) {
    throw 'anvil-container must run from a Git repository.'
}

$scriptDir = Join-Path $repoRoot 'justfiles/anvil/container'
$wslRepoRoot = (& wsl -e wslpath -a $repoRoot).Trim()
if ($LASTEXITCODE -ne 0 -or -not $wslRepoRoot) {
    throw 'anvil-container: could not translate the repository path into the default WSL distribution.'
}
$wslScriptDir = "$wslRepoRoot/justfiles/anvil/container"
$imageId = (& (Join-Path $scriptDir 'image-id.ps1')).Trim()
$imageBase = if ($env:ANVIL_CONTAINER_IMAGE) { $env:ANVIL_CONTAINER_IMAGE } else { 'anvil-dev' }
$image = "${imageBase}:$imageId"
$repoBytes = [Text.Encoding]::UTF8.GetBytes($wslRepoRoot)
$repoHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($repoBytes)).ToLowerInvariant()
$targetVolume = "anvil-target-$($repoHash.Substring(0, 12))-$($imageId.Substring(0, 12))"

$needsGitHubToken = $false
foreach ($recipeArg in $Recipe) {
    if (Test-AnvilRecipeNeedsGitHubToken $recipeArg) {
        $needsGitHubToken = $true
        break
    }
}
$runsOnlyGitHubCheck = $Recipe.Count -eq 1 -and $Recipe[0] -eq 'anvil-aprz'

# Versioned customization contract: check warm/cold state before sourcing so
# customization needed only for image construction can be skipped on a warm
# run, then expose read-only inputs. See docs/design/containers.md.
$null = & wsl -e docker image inspect $image 2>$null
$imageExists = $LASTEXITCODE -eq 0

New-Variable -Name AnvilContainerCustomizationApiVersion -Value 1 -Option ReadOnly
New-Variable -Name AnvilContainerRepoRoot -Value $repoRoot -Option ReadOnly
New-Variable -Name AnvilContainerDir -Value $scriptDir -Option ReadOnly
New-Variable -Name AnvilContainerRepoRootWsl -Value $wslRepoRoot -Option ReadOnly
New-Variable -Name AnvilContainerDirWsl -Value $wslScriptDir -Option ReadOnly
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
$AnvilContainerCleanup = $null
$githubToken = $null
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
    if (-not $imageExists) {
        if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {
            throw "anvil-container: image $image is missing and ANVIL_CONTAINER_NO_REBUILD=1."
        }
        & wsl -e docker build `
            --platform linux/amd64 `
            --tag $image `
            --file "$wslScriptDir/Containerfile" `
            --build-arg "ANVIL_IMAGE_ID=$imageId" `
            @AnvilContainerBuildArgs `
            $wslRepoRoot
        if ($LASTEXITCODE -ne 0) {
            throw "anvil-container: Docker build failed with exit code $LASTEXITCODE."
        }
    }

    $containerUid = (& wsl -e id -u).Trim()
    $containerGid = (& wsl -e id -g).Trim()
    if ($containerUid -notmatch '^\d+$' -or $containerGid -notmatch '^\d+$') {
        throw 'anvil-container: could not determine the default WSL user identity.'
    }
    $registryVolume = 'anvil-cargo-registry'
    $gitVolume = 'anvil-cargo-git'
    foreach ($volume in @($registryVolume, $gitVolume, $targetVolume)) {
        $null = & wsl -e docker volume create $volume
        if ($LASTEXITCODE -ne 0) {
            throw "anvil-container: Docker volume creation failed for '$volume' with exit code $LASTEXITCODE."
        }
    }
    $mountArgs = @(
        '--mount', "type=bind,source=$wslRepoRoot,target=/workspace",
        '--mount', "type=volume,source=$registryVolume,target=/usr/local/cargo/registry",
        '--mount', "type=volume,source=$gitVolume,target=/usr/local/cargo/git",
        '--mount', "type=volume,source=$targetVolume,target=/workspace/target"
    )
    & wsl -e docker run --rm --pull=never `
        --platform linux/amd64 `
        --user 0:0 `
        @mountArgs `
        $image sh -c "chown ${containerUid}:${containerGid} /usr/local/cargo/registry /usr/local/cargo/git /workspace/target"
    if ($LASTEXITCODE -ne 0) {
        throw "anvil-container: Docker volume initialization failed with exit code $LASTEXITCODE."
    }

    $runArgs = @(
        'run', '--rm', '--pull=never',
        '--platform', 'linux/amd64',
        '--user', "${containerUid}:${containerGid}",
        '--env', 'ANVIL_IN_CONTAINER=1',
        '--env', 'HOME=/tmp/anvil-user',
        '--workdir', '/workspace'
    )
    $runArgs += $mountArgs
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
        if (Test-Path "Env:$name") {
            $runArgs += @('--env', "$name=$((Get-Item "Env:$name").Value)")
        }
    }
    if ($AnvilContainerPrepareCommand.Count -gt 0) {
        & wsl -e docker @prepareRunArgs @AnvilContainerPrepareArgs $image @AnvilContainerPrepareCommand
        if ($LASTEXITCODE -ne 0) {
            throw "anvil-container: preparation command failed with exit code $LASTEXITCODE."
        }
    }

    if ($githubToken) {
        $githubTokenFile = Join-Path ([IO.Path]::GetTempPath()) "anvil-github-token-$PID-$([guid]::NewGuid().ToString('N'))"
        [IO.File]::Create($githubTokenFile).Dispose()
        if ($IsWindows) {
            $userSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
            & icacls.exe $githubTokenFile '/inheritance:r' '/grant:r' "*$($userSid):(F)" | Out-Null
        } else {
            & chmod 600 $githubTokenFile
        }
        if ($LASTEXITCODE -ne 0) {
            throw 'anvil-container: failed to restrict permissions on the temporary GitHub token file.'
        }
        [IO.File]::WriteAllText($githubTokenFile, $githubToken, [Text.Encoding]::ASCII)
        $githubToken = $null
        $wslTokenFile = (& wsl -e wslpath -a $githubTokenFile).Trim()
        if ($LASTEXITCODE -ne 0 -or -not $wslTokenFile) {
            throw 'anvil-container: could not translate the temporary GitHub token path into WSL.'
        }
        $githubRunArgs = @($runArgs)
        $githubRunArgs += @(
            '--mount',
            "type=bind,source=$wslTokenFile,target=/run/secrets/anvil-github-token,readonly"
        )
        if ($runsOnlyGitHubCheck) {
            $runArgs = $githubRunArgs
        } else {
            & wsl -e docker @githubRunArgs $image just anvil-aprz
            if ($LASTEXITCODE -ne 0) {
                throw "anvil-container: isolated anvil-aprz failed with exit code $LASTEXITCODE."
            }
            $runArgs += @('--env', 'ANVIL_APRZ_ALREADY_RAN=1')
        }
    }

    if ($Recipe.Count -eq 0) {
        & wsl -e docker @runArgs --interactive --tty $image bash
    } else {
        & wsl -e docker @runArgs $image just @Recipe
    }
    $exitCode = $LASTEXITCODE
} finally {
    if ($githubTokenFile) {
        Remove-Item -LiteralPath $githubTokenFile -Force -ErrorAction SilentlyContinue
    }
    if ($AnvilContainerCleanup) { & $AnvilContainerCleanup }
}

exit $exitCode
