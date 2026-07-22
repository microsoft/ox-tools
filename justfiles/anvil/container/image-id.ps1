# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$repoRoot = (git rev-parse --show-toplevel).Trim()
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
$containerPath = Join-Path $repoRoot 'justfiles/anvil/container'
$inputs += Get-ChildItem (Join-Path $repoRoot 'justfiles/anvil') -Recurse -File -Filter '*.just' |
    Where-Object { $_.DirectoryName -ne $containerPath } |
    ForEach-Object { [IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/') }
$executionOnly = @(
    'container.just',
    'image-id.ps1',
    'image-id.sh',
    'README.md',
    'run-in-container.ps1',
    'run-in-container.sh'
)
# Auth hooks are intentionally hashed: their source defines non-secret build
# customization. Runtime tokens and secret-file contents are never read here.
$inputs += Get-ChildItem $containerPath -File |
    Where-Object { $_.Name -notin $executionOnly } |
    ForEach-Object { [IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/') }
$uniqueInputs = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($inputPath in $inputs) {
    [void]$uniqueInputs.Add($inputPath)
}
$inputs = [string[]]$uniqueInputs
[Array]::Sort($inputs, [StringComparer]::Ordinal)

$payload = [Text.StringBuilder]::new()
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
