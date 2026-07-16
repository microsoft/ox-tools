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
$inputs += Get-ChildItem (Join-Path $repoRoot 'justfiles/anvil') -Recurse -File -Filter '*.just' |
    ForEach-Object { [IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/') }
$runtimeOnly = @(
    'auth.ps1',
    'auth.sh',
    'container.just',
    'image-id.ps1',
    'README.md',
    'run-in-container.ps1',
    'run-in-container.sh'
)
$inputs += Get-ChildItem (Join-Path $repoRoot 'justfiles/anvil/container') -File |
    Where-Object { $_.Name -notin $runtimeOnly } |
    ForEach-Object { [IO.Path]::GetRelativePath($repoRoot, $_.FullName).Replace('\', '/') }
$inputs = $inputs | Sort-Object -Unique

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
