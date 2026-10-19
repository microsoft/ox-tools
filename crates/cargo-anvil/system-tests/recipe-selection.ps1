# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

param([string] $Scenario)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/../templates/anvil/packages.ps1"

if (-not $Scenario) {
    foreach ($case in @(
        @{ name = 'selected'; exit = 0 },
        @{ name = 'workspace'; exit = 0 },
        @{ name = 'empty'; exit = 0 },
        @{ name = 'skip'; exit = 0 },
        @{ name = 'keep-going'; exit = 23 },
        @{ name = 'no-default-failure'; exit = 24 },
        @{ name = 'metadata-failure'; exit = 1 },
        @{ name = 'msrv'; exit = 0 }
    )) {
        & pwsh -NoProfile -File $PSCommandPath -Scenario $case.name
        if ($LASTEXITCODE -ne $case.exit) {
            throw "$($case.name): expected exit $($case.exit), got $LASTEXITCODE"
        }
    }
    exit 0
}

$root = Join-Path ([System.IO.Path]::GetTempPath()) "anvil's selection"
$memberManifest = Join-Path $root 'member/Cargo.toml'
$rootManifest = Join-Path $root 'Cargo.toml'
$rootId = 'path+file:///fixture/root#fixture-root@1.0.0'
$memberId = 'path+file:///fixture/root/member#fixture-member@1.0.0'
$metadata = @{
    workspace_root = $root
    workspace_members = @($rootId, $memberId)
    packages = @(
        @{ name = 'fixture-root'; version = '1.0.0'; id = $rootId; manifest_path = $rootManifest },
        @{ name = 'fixture-member'; version = '1.0.0'; id = $memberId; manifest_path = $memberManifest },
        @{ name = 'fixture-root'; version = '1.0.0'; id = 'registry+https://example.invalid#index#fixture-root@1.0.0' },
        @{ name = 'fixture-member'; version = '1.0.0'; id = 'registry+https://example.invalid#index#fixture-member@1.0.0' }
    )
} | ConvertTo-Json -Depth 8 -Compress

$script:expected = [System.Collections.Generic.Queue[object]]::new()
function Expect-Cargo([string[]] $Arguments, [int] $ExitCode = 0, [string] $Output = '') {
    $script:expected.Enqueue(@{ arguments = $Arguments; exit = $ExitCode; output = $Output })
}
function cargo {
    if ($script:expected.Count -eq 0) { throw "Unexpected Cargo call: $args" }
    $call = $script:expected.Dequeue()
    $actual = ConvertTo-Json -InputObject ([string[]]$args) -Compress
    $wanted = ConvertTo-Json -InputObject @($call.arguments) -Compress
    if ($actual -cne $wanted) { throw "Expected $wanted; got $actual" }
    $global:LASTEXITCODE = $call.exit
    if ($call.output) { $call.output }
}

$scope = switch ($Scenario) {
    'selected' { '--package fixture-member@1.0.0' }
    'msrv' { '--package fixture-member@1.0.0' }
    'skip' { '--skip' }
    'empty' { '' }
    default { '--workspace' }
}
function Invoke-AnvilTestJust {
    $global:LASTEXITCODE = 0
    switch ($args -join ' ') {
        '_anvil-impact-include affected' { $scope }
        '_anvil-resolve-stable msrv' { 'fixture-msrv' }
        default { throw "Unexpected Just call: $args" }
    }
}

if ($Scenario -ne 'skip') {
    Expect-Cargo @('+fixture-stable', 'metadata', '--locked', '--no-deps', '--format-version', '1') `
        -ExitCode $(if ($Scenario -eq 'metadata-failure') { 1 } else { 0 }) -Output $metadata
}
if ($Scenario -eq 'msrv') {
    Expect-Cargo @('+fixture-msrv', 'test', '--manifest-path', $memberManifest, '--tests', '--all-features', '--locked')
    Expect-Cargo @('+fixture-msrv', 'test', '--manifest-path', $memberManifest, '--tests', '--locked')
} elseif ($Scenario -notin @('skip', 'metadata-failure')) {
    foreach ($flags in @(@(), @('--no-default-features'))) {
        if ($Scenario -ne 'selected') {
            $code = if ($Scenario -eq 'keep-going') { 23 } elseif ($Scenario -eq 'no-default-failure' -and $flags.Count) { 24 } else { 0 }
            Expect-Cargo (@('+fixture-stable', 'check', '--manifest-path', $rootManifest, '--package', $rootId.Substring(5), '--all-targets') + $flags + @('--locked')) -ExitCode $code
        }
        Expect-Cargo (@('+fixture-stable', 'check', '--manifest-path', $memberManifest, '--all-targets') + $flags + @('--locked'))
        if ($Scenario -eq 'keep-going') { break }
    }
}

$name = if ($Scenario -eq 'msrv') { 'msrv-test' } else { 'check-all-targets' }
$template = Get-Content -Raw "$PSScriptRoot/../templates/justfiles/anvil/checks/$name.just"
$match = [regex]::Match($template, "(?m)^anvil-${name}:[^\r\n]*\r?\n(?<body>(?:    [^\r\n]*\r?\n|\r?\n)+)")
if (-not $match.Success) { throw "Recipe body not found: $name" }
$body = [regex]::Replace($match.Groups['body'].Value, '(?m)^    ', '')
$body = $body.Replace('{{ just_executable() }}', 'Invoke-AnvilTestJust')
$body = $body.Replace('{{_anvil_stable_toolchain_args}}', "'+fixture-stable'")
$templateRoot = (Resolve-Path "$PSScriptRoot/../templates").Path.Replace("'", "''")
$body = $body -replace "(?m)^\. .*packages\.ps1'\r?$", ". '$templateRoot/anvil/packages.ps1'"
$recipeFailed = $false
try {
    & ([scriptblock]::Create($body))
} catch {
    $recipeFailed = $true
    throw
} finally {
    if (-not $recipeFailed -and $script:expected.Count -ne 0) { throw "$Scenario left $($script:expected.Count) Cargo calls unexecuted" }
}
