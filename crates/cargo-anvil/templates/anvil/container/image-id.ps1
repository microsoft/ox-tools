# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repoRoot = (git rev-parse --show-toplevel 2>$null).Trim()
if ($LASTEXITCODE -ne 0 -or -not $repoRoot) {
    throw 'anvil-container must run from a Git repository.'
}

$inputs = @(
    'rust-toolchain.toml'
)
$toolchainPath = Join-Path $repoRoot 'rust-toolchain.toml'
if (-not (Test-Path -LiteralPath $toolchainPath -PathType Leaf)) {
    throw 'anvil-container requires a repository-owned rust-toolchain.toml.'
}
$containerPath = Join-Path $repoRoot '.anvil/container'
$containerfile = Join-Path $containerPath 'Containerfile'
$baseImageMatch = [regex]::Match([IO.File]::ReadAllText($containerfile), '(?m)^ARG BASE_IMAGE=([^\r\n]+)')
if (-not $baseImageMatch.Success) {
    throw 'anvil-container: Containerfile must define ARG BASE_IMAGE=<digest-pinned-image>.'
}
$defaultBaseImage = $baseImageMatch.Groups[1].Value
$baseImage = if ($env:ANVIL_CONTAINER_BASE_IMAGE) { $env:ANVIL_CONTAINER_BASE_IMAGE } else { $defaultBaseImage }
if ($baseImage -notmatch '@sha256:[0-9a-fA-F]{64}$') {
    throw 'anvil-container: ANVIL_CONTAINER_BASE_IMAGE must be pinned by sha256 digest (image@sha256:<64 hex characters>).'
}
# Every recipe reaching the build context is hashed, including the
# anvil-container entry recipe: mod.just imports it, so `just anvil-setup`
# cannot parse without it during the image build.
$inputs += Get-ChildItem (Join-Path $repoRoot 'justfiles/anvil') -Recurse -File -Filter '*.just' |
    ForEach-Object { [IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/') }
$executionOnly = @(
    'image-id.ps1',
    'image-id.sh',
    'README.md',
    'run-in-container.ps1',
    'run-in-container.sh',
    'customize.sh',
    'customize.ps1'
)
# Host-only files never reach the build context (see
# Containerfile.dockerignore), so they must not select an image either.
# customize.sh/customize.ps1 are trusted runtime orchestration; static,
# non-secret build customization belongs in a hashed artifact instead.
$inputs += Get-ChildItem $containerPath -File |
    Where-Object { $_.Name -notin $executionOnly } |
    ForEach-Object { [IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/') }

# Files a repository declared through [[container.image.file]] are copied into
# the image, so their contents select it. They are learned from the generated
# Containerfile's COPY instructions rather than from a second list, so the
# helper needs no TOML parser and the two lists cannot disagree.
$inputs += [regex]::Matches(
    [IO.File]::ReadAllText($containerfile),
    '(?m)^COPY \["([^"]*)", "[^"]*"\]$'
) | ForEach-Object { $_.Groups[1].Value }
$uniqueInputs = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($inputPath in $inputs) {
    [void]$uniqueInputs.Add($inputPath)
}
$inputs = [string[]]$uniqueInputs
[Array]::Sort($inputs, [StringComparer]::Ordinal)

$payload = [Text.StringBuilder]::new()
[void]$payload.Append("ANVIL_CONTAINER_BASE_IMAGE`n").Append($baseImage).Append("`n")
foreach ($relative in $inputs) {
    $path = Join-Path $repoRoot $relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Container image input is missing: $relative"
    }
    $content = [IO.File]::ReadAllText($path).Replace("`r`n", "`n").Replace("`r", "`n")
    [void]$payload.Append($relative).Append("`n").Append($content).Append("`n")
}

$bytes = [Text.Encoding]::UTF8.GetBytes($payload.ToString())
$hash = [Security.Cryptography.SHA256]::HashData($bytes)
Write-Output ([Convert]::ToHexString($hash).ToLowerInvariant())
