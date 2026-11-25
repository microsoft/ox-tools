# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Build and test the packaged release candidates.
[group("anvil")]
[script("pwsh", "-NoProfile")]
anvil-release-dependencies:
    $ErrorActionPreference = 'Stop'
    $PSNativeCommandUseErrorActionPreference = $false
    Set-Location -LiteralPath '{{ replace(justfile_directory(), "'", "''") }}'
    $report = & '{{ replace(just_executable(), "'", "''") }}' --justfile '{{ replace(justfile(), "'", "''") }}' anvil-release-candidates
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $candidates = $report | ConvertFrom-Json -NoEnumerate
    if ($candidates.Count -eq 0) {
        Write-Host 'anvil-release-dependencies: no release candidates'
        exit 0
    }

    $stableArgs = @({{ anvil_stable_toolchain_arg }})
    $sysroot = & rustc @stableArgs --print sysroot
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $env:RUSTUP_TOOLCHAIN = $sysroot.Trim()

    $tempRoot = $env:RUNNER_TEMP
    if (-not $tempRoot) { $tempRoot = $env:AGENT_TEMPDIRECTORY }
    $tempArgs = @()
    if ($tempRoot) { $tempArgs = @('--temp-dir', $tempRoot) }

    $OutputEncoding = [Text.UTF8Encoding]::new($false)
    if (Test-Path -LiteralPath 'crates/cargo-anvil/src/release_dependencies.rs' -PathType Leaf) {
        $report | cargo run --locked --package cargo-anvil --bin cargo-anvil -- anvil release-dependencies @tempArgs
    } else {
        $report | cargo anvil release-dependencies @tempArgs
    }
    exit $LASTEXITCODE
