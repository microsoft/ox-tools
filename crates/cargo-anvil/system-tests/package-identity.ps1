# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/../templates/anvil/packages.ps1"

function Assert-Equal($Actual, $Expected) {
    $actualJson = ConvertTo-Json -InputObject $Actual -Depth 8 -Compress
    $expectedJson = ConvertTo-Json -InputObject $Expected -Depth 8 -Compress
    if ($actualJson -cne $expectedJson) {
        throw "Expected $expectedJson; got $actualJson"
    }
}

function cargo {
    Assert-Equal @($args) @('+fixture-nightly', 'metadata', '--locked', '--no-deps', '--format-version', 1)
    $global:LASTEXITCODE = 0
    $script:fixture | ConvertTo-Json -Depth 8 -Compress
}

foreach ($case in @(
    @{ manifest = "/runner's checkout/local crate/Cargo.toml"; id = 'path+file:///runner%27s%20checkout/local%20crate#ohno@0.5.2' },
    @{ manifest = 'C:\runner checkout\local crate\Cargo.toml'; id = 'path+file:///C:/runner%20checkout/local%20crate#ohno@0.5.2' }
)) {
    $script:fixture = @{
        workspace_root = 'unused'
        workspace_members = @($case.id)
        packages = @(
            @{ name = 'ohno'; version = '0.5.2'; id = $case.id; manifest_path = $case.manifest },
            @{ name = 'ohno'; version = '0.5.2'; id = 'registry+https://github.com/rust-lang/crates.io-index#ohno@0.5.2'; manifest_path = '/registry/ohno/Cargo.toml' }
        )
    }
    $selected = @(Get-AnvilPackages '--package ohno@0.5.2' -Toolchain '+fixture-nightly')
    Assert-Equal @($selected.id) @($case.id)
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter manifest) @('--manifest-path', $case.manifest)
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter llvm-cov) @('--package', $case.id)
    Assert-Equal @(Get-AnvilPackageArguments $selected) @('--package', $case.id.Substring(5))
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter each) @('--package', 'ohno@0.5.2')
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter names) @('--package', 'ohno')
    Assert-Equal @(Resolve-AnvilPackageArguments '--package ohno' -Toolchain '+fixture-nightly') @('--manifest-path', $case.manifest)
    $explicit = @(Get-AnvilPackages '--package ohno' -Toolchain '+fixture-nightly')
    Assert-Equal @($explicit.id) @($case.id)
    $deduplicated = @(Get-AnvilPackages '--package ohno --package ohno@0.5.2' -Toolchain '+fixture-nightly')
    Assert-Equal @($deduplicated.id) @($case.id)
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter semver) @('--manifest-path', $case.manifest, '--package', 'ohno')
    $selected[0].anvil_workspace_root = Split-Path -Parent $case.manifest
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter manifest) @('--manifest-path', $case.manifest, '--package', $case.id.Substring(5))
    Assert-Equal @(Get-AnvilPackageArguments $selected -Adapter semver) @('--manifest-path', $case.manifest, '--package', 'ohno')
    Assert-Equal @(Get-AnvilPackages '--skip') @()
    $message = $null
    try { Get-AnvilPackages '--package missing@0.1.0' -Toolchain '+fixture-nightly' | Out-Null }
    catch { $message = $_.Exception.Message }
    Assert-Equal $message "anvil: package 'missing@0.1.0' must resolve to exactly one workspace member"
}
