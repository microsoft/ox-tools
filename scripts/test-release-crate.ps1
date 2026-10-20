# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Run from the repository root: pwsh -NoProfile -File .\scripts\test-release-crate.ps1
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$releaseScript = Join-Path $PSScriptRoot 'release-crate.ps1'
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "ox-release-test-$([guid]::NewGuid())"

function Invoke-FixtureGit {
    param([Parameter(ValueFromRemainingArguments)][string[]]$Arguments)
    & git @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Fixture git command failed: git $Arguments"
    }
}

function Assert-Contains {
    param([string]$Content, [string]$Expected)
    if (-not $Content.Contains($Expected)) {
        throw "Expected generated notes to contain '$Expected'."
    }
}

$null = New-Item -ItemType Directory -Path (Join-Path $fixture 'crates\sample')
Push-Location $fixture
try {
    Invoke-FixtureGit init --quiet
    Invoke-FixtureGit config user.name 'Release test'
    Invoke-FixtureGit config user.email 'release-test@example.invalid'
    Invoke-FixtureGit config commit.gpgsign false
    Invoke-FixtureGit config core.autocrlf false
    Invoke-FixtureGit remote add origin https://github.com/microsoft/ox-tools.git
    # The manifest stays ahead of the tagged releases to detect unintended version changes.
    # A non-semantic tag must not bound the first release; commits beyond a later tag must stay out.
    '[workspace]' | Set-Content Cargo.toml
    "[package]`nname = `"sample`"`nversion = `"2.0.0`"" | Set-Content crates\sample\Cargo.toml
    'README must not change' | Set-Content crates\sample\README.md
    'initial source' | Set-Content crates\sample\source.rs
    Invoke-FixtureGit add .
    Invoke-FixtureGit commit --quiet -m 'feat(sample): initial history'
    Invoke-FixtureGit tag sample-vpreview
    'first release source' | Add-Content crates\sample\source.rs
    Invoke-FixtureGit commit --quiet -am 'fix(sample): first release fix'
    Invoke-FixtureGit tag sample-v1.0.0

    $preserved = @('Cargo.toml', 'crates\sample\Cargo.toml', 'crates\sample\README.md')
    $before = @($preserved | ForEach-Object { (Get-FileHash $_).Hash })
    & $releaseScript sample -BackfillVersion 1.0.0
    $first = Get-Content crates\sample\CHANGELOG.md -Raw
    Assert-Contains $first '## [1.0.0]'
    Assert-Contains $first 'initial history'
    Assert-Contains $first 'first release fix'

    & $releaseScript sample -BackfillVersion 1.0.0
    if ((Get-Content crates\sample\CHANGELOG.md -Raw) -cne $first) {
        throw 'First-tag backfill is not byte-stable.'
    }

    'second release source' | Add-Content crates\sample\source.rs
    Invoke-FixtureGit commit --quiet -am 'feat(sample): second release feature'
    Invoke-FixtureGit tag sample-v1.1.0
    'future source' | Add-Content crates\sample\source.rs
    Invoke-FixtureGit commit --quiet -am 'fix(sample): future change'

    & $releaseScript sample -BackfillVersion 1.1.0
    $notes = Get-Content crates\sample\CHANGELOG.md -Raw
    $second = ($notes -split '(?m)^## \[1\.0\.0\]')[0]
    Assert-Contains $second 'second release feature'
    foreach ($excluded in 'initial history', 'first release fix', 'future change') {
        if ($second.Contains($excluded)) {
            throw "Later backfill included out-of-range commit '$excluded'."
        }
    }
    & $releaseScript sample -BackfillVersion 1.0.0
    if ((Get-Content crates\sample\CHANGELOG.md -Raw) -cne $notes) {
        throw 'First-tag reconstruction changed after adding later releases.'
    }
    $after = @($preserved | ForEach-Object { (Get-FileHash $_).Hash })
    if (Compare-Object $before $after) {
        throw 'Backfill changed a manifest or README.'
    }
    Write-Host 'Release backfill checks passed (first tag, semantic bounds, stability, unchanged manifests and README).'
}
finally {
    Pop-Location
    Remove-Item -LiteralPath $fixture -Recurse -Force
}
