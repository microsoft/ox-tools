# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
    End-to-end test for `[[image]]` product image builds.

.DESCRIPTION
    Walks the path a repository takes to build its own OCI images with anvil:

      1. clean up          -- remove anything left by a previous run
      2. a repository      -- a crate, plus an anvil.toml declaring two images
      3. generate          -- cargo anvil
      4. list              -- `anvil-image` with no name names the images
      5. reject            -- an unknown name, and a context outside the
                              output dir, both fail rather than build
      6. build one         -- `anvil-image base` stages its files and builds
      7. build all         -- `anvil-images` builds both in dependency order
      8. clean up

    Steps 5 to 7 are the claims worth testing. Nothing here edits a generated
    file: a user never touches those, and asserting on them would test the
    generator against itself.

    Unlike the container exec image, these builds do not install a toolchain --
    they copy prebuilt binaries into a staged context. The whole run is
    therefore fast, and deliberately never builds anvil's own exec image.

    cargo-anvil is a generator: it writes files and exits, so nothing in the
    Rust test suite executes the PowerShell it emits. This script is what
    covers that.

.PARAMETER WorkspacePath
    Where to create the throwaway repository.

.PARAMETER Keep
    Leave the workspace and images in place for inspection.
#>

param(
    [Parameter(Mandatory = $false)]
    [string]$WorkspacePath = (Join-Path ([System.IO.Path]::GetTempPath()) "anvil-images-e2e"),

    [Parameter(Mandatory = $false)]
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
# Native command failures are handled explicitly below; do not fail fast.
$PSNativeCommandUseErrorActionPreference = $false

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$registry = 'anvile2e'
$script:failures = @()
$script:checks = 0

function Write-Step([string]$Message) {
    Write-Host ''
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Assert-That([object]$Condition, [string]$Message) {
    $script:checks++
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
    # together: the build notice and the failure message both matter here.
    param([string[]]$Arguments)

    $captured = $null
    if ($script:engineMode -eq 'wsl') {
        $joined = ($Arguments | ForEach-Object { "'$_'" }) -join ' '
        & wsl -e bash -c "cd '$(Get-WslWorkspace)' && just $joined 2>&1" |
            Tee-Object -Variable captured |
            ForEach-Object { Write-Host "    $_" }
    }
    else {
        Push-Location $WorkspacePath
        try {
            & just @Arguments 2>&1 |
                Tee-Object -Variable captured |
                ForEach-Object { Write-Host "    $_" }
        }
        finally { Pop-Location }
    }
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = (@($captured) -join "`n") }
}

function Invoke-Anvil {
    # Config changes only reach the recipes through the generator, exactly as a
    # user edits anvil.toml and re-runs `cargo anvil`.
    param([switch]$AllowFailure)

    $captured = $null
    Push-Location $WorkspacePath
    try {
        & $exe anvil --no-backends 2>&1 | Tee-Object -Variable captured | Out-Null
    }
    finally { Pop-Location }
    if ($LASTEXITCODE -ne 0 -and -not $AllowFailure) { throw 'cargo anvil failed' }
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = (@($captured) -join "`n") }
}

function Get-ImageTags([string]$Repository) {
    # `docker images <ref>` matches the repository exactly, so this reports
    # only the image asked about.
    return @(Invoke-Engine -Arguments @('images', "$registry/$Repository", '--format', '{{.Repository}}:{{.Tag}}')) |
        Where-Object { $_ }
}

function Remove-TestImages {
    foreach ($repository in @('base', 'svc', 'acme/renamed')) {
        $ids = @(Invoke-Engine -Arguments @('images', "$registry/$repository", '--quiet')) | Where-Object { $_ }
        if ($ids) { Invoke-Engine -Arguments (@('image', 'rm', '--force') + $ids) | Out-Null }
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
Assert-That ((Get-ImageTags 'base').Count -eq 0) 'starting with no image, as a new adopter would'

# ---------------------------------------------------------------------------
# 2. A repository that builds its own images
# ---------------------------------------------------------------------------

Write-Step 'Creating a repository that declares two images'

& cargo build --quiet --package cargo-anvil --manifest-path (Join-Path $repoRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'failed to build cargo-anvil' }
$exe = Join-Path $repoRoot 'target/debug/cargo-anvil'
if (-not (Test-Path -LiteralPath $exe)) { $exe = "$exe.exe" }
if (-not (Test-Path -LiteralPath $exe)) { throw 'cargo-anvil binary not found after build' }

function Write-RepoFile([string]$RelativePath, [string]$Content) {
    # Always LF, always a trailing newline: the images are built on Linux.
    $normalized = ($Content -replace "`r`n", "`n")
    if (-not $normalized.EndsWith("`n")) { $normalized += "`n" }
    $path = Join-Path $WorkspacePath $RelativePath
    New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force | Out-Null
    [System.IO.File]::WriteAllText($path, $normalized)
}

$license = "# Copyright (c) Microsoft Corporation.`n# Licensed under the MIT License.`n`n"

Write-RepoFile 'Cargo.toml' ($license + @'
[package]
name = "anvil-images-e2e"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
'@)

Write-RepoFile 'src/lib.rs' @'
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Not the subject of the test; the image builds are.

/// Adds two numbers.
#[must_use]
pub const fn add(left: i32, right: i32) -> i32 {
    left + right
}
'@

Write-RepoFile 'Justfile' ($license + "import 'justfiles/anvil/mod.just'`n")

# The binary each image ships. Images copy prebuilt artifacts in through
# `stage-artifacts`; nothing is ever compiled inside an image build.
Write-RepoFile 'target/debug/my-svc' @'
#!/bin/sh
echo "my-svc running"
'@

# A base image other images build on, and a service image that stages the
# prebuilt binary and selects a multi-stage target. `renamed` covers the case
# where the published path differs from the recipe name -- `/` is not a valid
# just recipe token, so the two cannot always be the same string.
Write-RepoFile 'containers/base/Dockerfile' @'
ARG BASE_IMAGE=docker.io/library/busybox:1.37.0
FROM ${BASE_IMAGE}
ARG FLAVOUR=plain
RUN printf '%s\n' "${FLAVOUR}" > /etc/anvil-flavour
'@

Write-RepoFile 'containers/svc/Dockerfile' @'
FROM docker.io/library/busybox:1.37.0 AS build
RUN printf 'unused\n' > /tmp/marker

FROM docker.io/library/busybox:1.37.0 AS runtime
COPY bin/my-svc /usr/local/bin/my-svc
RUN chmod 755 /usr/local/bin/my-svc
'@

Write-RepoFile 'anvil.toml' ($license + @'
image-output-dir = "out"

[container]
enabled = true
name = "anvil-images-e2e"

# `svc` is declared FIRST and depends on `base`, so declaration order is the
# reverse of dependency order. An implementation that ignored `depends-on` and
# simply built in source order would fail the ordering check below -- which it
# would not if the two were declared the other way round.
[[image]]
name = "svc"
repository = "acme/renamed"
dockerfile = "containers/svc/Dockerfile"
context = "out/svc"
target = "runtime"
depends-on = ["base"]
stage-artifacts = [{ from = "target/{profile}/my-svc", to = "bin/my-svc" }]

[[image]]
name = "base"
dockerfile = "containers/base/Dockerfile"
context = "out/base"
build-args = { FLAVOUR = "{tag}" }
'@)

Push-Location $WorkspacePath
try {
    & git init --quiet
    Write-Step 'Generating the anvil recipes'
    & $exe anvil --no-backends | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'cargo anvil failed' }
}
finally {
    Pop-Location
}

Assert-That (Test-Path -LiteralPath (Join-Path $WorkspacePath 'justfiles/anvil/container-images.just')) `
    'the image recipes were generated'

# ---------------------------------------------------------------------------
# 3. Discovery: the recipe names its images
# ---------------------------------------------------------------------------

Write-Step 'Asking which images exist (expect both to be named)'

$listed = Invoke-Just -Arguments @('anvil-image')

Assert-That ($listed.ExitCode -eq 0) 'listing the images succeeds'
Assert-That ($listed.Output -match 'base') 'the listing names the base image'
Assert-That ($listed.Output -match 'svc') 'the listing names the service image'

# ---------------------------------------------------------------------------
# 4. The guards fire before anything is built
# ---------------------------------------------------------------------------

Write-Step 'Asking for an image that does not exist (expect a refusal)'

$unknown = Invoke-Just -Arguments @('anvil-image', 'nope')

Assert-That ($unknown.ExitCode -ne 0) 'an unknown image name fails'
Assert-That ($unknown.Output -match 'unknown image') 'the failure says the name is unknown'
Assert-That ($unknown.Output -match 'base') 'the failure lists the valid names'

Write-Step 'Pointing an image context outside the output dir (expect a refusal)'

# The guard that stops the whole repository being sent to the engine as build
# context. A user hits this by pointing `context` at the repo root, and anvil
# refuses to generate at all -- the recipe carrying a bad context is never
# written, so the mistake cannot reach a build.
$anvilToml = Join-Path $WorkspacePath 'anvil.toml'
$original = [System.IO.File]::ReadAllText($anvilToml)
[System.IO.File]::WriteAllText($anvilToml, ($original -replace 'context = "out/base"', 'context = "."'))

$escaped = Invoke-Anvil -AllowFailure

Assert-That ($escaped.ExitCode -ne 0) 'a context outside the output dir is refused at generation'
Assert-That ($escaped.Output -match 'must live under the image output dir') `
    'the refusal explains the context must live under the output dir'
Assert-That ((Get-ImageTags 'base').Count -eq 0) 'nothing was built from the rejected context'

[System.IO.File]::WriteAllText($anvilToml, $original)
Invoke-Anvil | Out-Null

Write-Step 'Expanding a token into an escape at run time (expect a refusal)'

# The generator validates `to` before `{profile}`/`{tag}` expand, and those
# come from the caller -- so this path is reachable only at run time, and only
# the recipe's own guard can catch it.
[System.IO.File]::WriteAllText(
    $anvilToml,
    ($original -replace 'to = "bin/my-svc"', 'to = "{tag}/my-svc"'))
Invoke-Anvil | Out-Null

$escapeAtRuntime = Invoke-Just -Arguments @('anvil-image', 'svc', 'debug', '../../../anvil-escaped', $registry)

Assert-That ($escapeAtRuntime.ExitCode -ne 0) 'a token expanding to an escape is refused at run time'
Assert-That ($escapeAtRuntime.Output -match 'escapes the build context') `
    'the refusal explains the staged path escaped the context'
Assert-That (-not (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $WorkspacePath) 'anvil-escaped'))) `
    'nothing was written outside the workspace'

[System.IO.File]::WriteAllText($anvilToml, $original)
Invoke-Anvil | Out-Null

# ---------------------------------------------------------------------------
# 5. Build one image
# ---------------------------------------------------------------------------

Write-Step 'Building a single image (expect the build args to reach it)'

$base = Invoke-Just -Arguments @('anvil-image', 'base', 'debug', 'v1', $registry)

Assert-That ($base.ExitCode -eq 0) 'building the base image succeeds'
Assert-That ((Get-ImageTags 'base') -contains "$registry/base:v1") `
    'the base image is tagged with the requested tag'

# `{tag}` in a build arg expands to the tag the recipe was called with; the
# Dockerfile writes it to a file, so reading it back proves the whole path.
$flavour = (Invoke-Engine -Arguments @('run', '--rm', "$registry/base:v1", 'cat', '/etc/anvil-flavour')) -join ''
Assert-That ($flavour.Trim() -eq 'v1') "the {tag} token reached the build arg (got '$($flavour.Trim())')"

# ---------------------------------------------------------------------------
# 6. Build every image
# ---------------------------------------------------------------------------

Write-Step 'Building every image (expect dependency order and the renamed path)'

$all = Invoke-Just -Arguments @('anvil-images', 'debug', 'v2', $registry)

Assert-That ($all.ExitCode -eq 0) 'building every image succeeds'
Assert-That ((Get-ImageTags 'base') -contains "$registry/base:v2") 'the base image was rebuilt at the new tag'

# `repository` decides the published path, `name` only names the recipe.
Assert-That ((Get-ImageTags 'acme/renamed') -contains "$registry/acme/renamed:v2") `
    'the service image published under its repository, not its name'
Assert-That ((Get-ImageTags 'svc').Count -eq 0) 'nothing was published under the recipe name'

# base is declared after svc's `depends-on`, so ordering is not source order.
$baseAt = $all.Output.IndexOf('building ' + $registry + '/base')
$svcAt = $all.Output.IndexOf('building ' + $registry + '/acme/renamed')
Assert-That (($baseAt -ge 0) -and ($svcAt -gt $baseAt)) 'the dependency was built before its dependent'

# `stage-artifacts` copied the prebuilt binary in, and `target` selected the
# runtime stage. Running it proves both: a wrong stage has no binary.
$staged = (Invoke-Engine -Arguments @('run', '--rm', "$registry/acme/renamed:v2", '/usr/local/bin/my-svc')) -join ''
Assert-That ($staged -match 'my-svc running') 'the prebuilt binary was staged into the image'

$marker = (Invoke-Engine -Arguments @('run', '--rm', "$registry/acme/renamed:v2", 'ls', '/tmp/marker')) -join ''
Assert-That ($marker -notmatch '/tmp/marker') 'the build stage was not shipped; --target selected the runtime stage'

# ---------------------------------------------------------------------------
# 7. Clean up
# ---------------------------------------------------------------------------

if (-not $Keep) {
    Write-Step 'Cleaning up'
    Remove-TestImages
    if (Test-Path -LiteralPath $WorkspacePath) {
        Remove-Item -LiteralPath $WorkspacePath -Recurse -Force
    }
    Write-Host '  removed the workspace and images'
}
else {
    Write-Step "Leaving $WorkspacePath and its images in place"
}

Write-Host ''
if ($script:failures.Count -gt 0) {
    Write-Host "$($script:failures.Count) of $($script:checks) checks FAILED:" -ForegroundColor Red
    $script:failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host "all $($script:checks) checks passed" -ForegroundColor Green
exit 0
