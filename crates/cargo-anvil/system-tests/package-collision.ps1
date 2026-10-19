# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/../templates/anvil/packages.ps1"

Push-Location "$PSScriptRoot/fixtures/package-collision"
try {
    & cargo fetch --locked
    if ($LASTEXITCODE -ne 0) { throw "Fixture dependency fetch failed: $LASTEXITCODE" }
    $packages = @(Get-AnvilPackages)
    $metadata = & cargo metadata --locked --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw "Fixture metadata failed: $LASTEXITCODE" }
    foreach ($package in $packages) {
        $collisions = @($metadata.packages | Where-Object { $_.name -ceq $package.name -and $_.version -ceq $package.version })
        if ($collisions.Count -ne 2) { throw "Missing source collision for $($package.name)" }
        $packageArgs = @(Get-AnvilPackageArguments @($package) -Adapter manifest)
        foreach ($flags in @(@(), @('--no-default-features'))) {
            & cargo check @packageArgs --all-targets @flags --locked --offline
            if ($LASTEXITCODE -ne 0) { throw "Collision check failed for $($package.name): $LASTEXITCODE" }
        }
    }
} finally {
    Pop-Location
}
