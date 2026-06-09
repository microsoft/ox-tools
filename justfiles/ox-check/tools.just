# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/local.md for the policy.

# Public: assert that every catalog tool is present at or above its
# minimum version. Called by `ox-check-tools-check` and as a dependency
# of every check group via individual `_ox-check-require` invocations.
[group("ox-check")]
[script("pwsh")]
ox-check-tools-check:
    $ErrorActionPreference = 'Stop'
    $catalog = 'justfiles/ox-check/tool-minimums.txt'
    $fail = 0
    foreach ($line in Get-Content $catalog) {
        $trimmed = $line.Trim()
        if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
        if ($trimmed -match '^([^=]+)=') {
            $tool = $Matches[1]
            & just _ox-check-require $tool
            if ($LASTEXITCODE -ne 0) { $fail = 1 }
        }
    }
    & just _ox-check-require rustc
    if ($LASTEXITCODE -ne 0) { $fail = 1 }
    exit $fail

# Public: install (or upgrade) every catalog tool to its minimum
# version. Idempotent — ``cargo install --locked`` is a no-op when
# the installed version already satisfies the requirement.
#
# Per-tool failures don't abort the whole install — a tool that
# doesn't build on the current host (e.g. cargo-mutants on
# aarch64-pc-windows-msvc, where the upstream winapi dep doesn't
# compile) shouldn't block other tools. Each failure is reported
# at the end; recipes that use the missing tool will get a clean
# error from ``_ox-check-require`` at recipe-run time. The recipe
# itself exits non-zero only if at least one install failed, so
# CI surfaces the problem.
[group("ox-check")]
[script("pwsh")]
ox-check-tools-install:
    $ErrorActionPreference = 'Continue'
    $catalog = 'justfiles/ox-check/tool-minimums.txt'
    $failed = [System.Collections.Generic.List[string]]::new()
    foreach ($line in Get-Content $catalog) {
        $trimmed = $line.Trim()
        if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
        if ($trimmed -match '^([^=]+)=(.+)$') {
            $tool = $Matches[1]
            $min = $Matches[2]
            Write-Host "ox-check-tools-install: $tool >= $min"
            cargo install --locked $tool --version ">=$min"
            if ($LASTEXITCODE -ne 0) {
                Write-Host "ox-check-tools-install: $tool install FAILED (continuing)" -ForegroundColor Yellow
                $failed.Add($tool)
            }
        }
    }
    if ($failed.Count -gt 0) {
        Write-Host ""
        Write-Host "ox-check-tools-install: $($failed.Count) tool(s) failed to install:" -ForegroundColor Yellow
        foreach ($t in $failed) { Write-Host "  - $t" -ForegroundColor Yellow }
        Write-Host "Recipes that depend on these tools will fail at run time via _ox-check-require." -ForegroundColor Yellow
        exit 1
    }

# Internal: assert that `name` meets its catalog minimum (or, for tools
# not in the catalog, that it's at least present). Three lookup paths:
#
#   - "rustc": presence-only; the workspace's rust-toolchain.toml drives
#     the actual version requirement (rustup auto-installs).
#   - "pwsh": presence-only with a per-OS install hint.
#   - Anything else: looked up in tool-minimums.txt; if present, the
#     installed version is parsed from `cargo install --list` and
#     compared via the [version] type. If not in the catalog,
#     presence-only fallback (covers system binaries like `just`
#     itself, `git`).
[script("pwsh")]
_ox-check-require name:
    $ErrorActionPreference = 'Stop'
    $name = '{{name}}'
    $catalog = 'justfiles/ox-check/tool-minimums.txt'

    switch ($name) {
        'rustc' {
            if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
                Write-Error 'ox-check: rustc not found. Install via rustup: https://rustup.rs'
                exit 1
            }
            exit 0
        }
        'pwsh' {
            if (-not (Get-Command pwsh -ErrorAction SilentlyContinue)) {
                $hint = if ($IsMacOS) {
                    'brew install --cask powershell'
                } elseif ($IsLinux) {
                    'see https://github.com/PowerShell/PowerShell'
                } else {
                    'winget install --id Microsoft.PowerShell'
                }
                Write-Error "ox-check: pwsh (PowerShell Core) not found. Install: $hint"
                exit 1
            }
            exit 0
        }
    }

    # Catalog lookup. Trim CRLF (Windows checkouts) and skip
    # comments / blank lines while we're at it.
    $spec = $null
    if (Test-Path $catalog) {
        foreach ($line in Get-Content $catalog) {
            $trimmed = $line.Trim()
            if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
            $pattern = '^' + [regex]::Escape($name) + '=(.+)$'
            if ($trimmed -match $pattern) {
                $spec = $Matches[1]
                break
            }
        }
    }

    if (-not $spec) {
        # Not in catalog: presence-only fallback for tools we don't
        # version-gate (`just` itself, `git`, system binaries).
        if (-not (Get-Command $name -ErrorAction SilentlyContinue)) {
            Write-Error "ox-check: required tool '$name' not found. Install hint: cargo install --locked $name"
            exit 1
        }
        exit 0
    }

    $min = $spec
    # Parse `cargo install --list` for installed version. Lines look like:
    #   cargo-deny v0.18.5:
    #       cargo-deny
    $installed = $null
    $listPattern = '^' + [regex]::Escape($name) + ' v(\S+):'
    foreach ($line in (cargo install --list 2>$null)) {
        if ($line -match $listPattern) {
            $installed = $Matches[1]
            break
        }
    }

    if (-not $installed) {
        Write-Error "ox-check: '$name' not installed (need >=v$min). Install: cargo install --locked $name --version '>=$min'"
        exit 1
    }

    # SemVer comparison via the [version] type. Strip any pre-release
    # / build-metadata suffix (e.g. "1.2.3-alpha.4") for parsing —
    # [version] only handles dotted numeric components.
    $minClean = ($min -split '[-+]', 2)[0]
    $instClean = ($installed -split '[-+]', 2)[0]
    $minV = [version]$minClean
    $instV = [version]$instClean
    if ($instV -lt $minV) {
        Write-Error "ox-check: '$name' is at v$installed; need >=v$min. Upgrade: cargo install --locked $name --version '>=$min'"
        exit 1
    }
