# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/local.md for the policy.

# Public: probe for system-level prerequisites that catalog tools need
# to BUILD from source. The `binstall` install path downloads pre-built
# binaries and does not need these, so this check is primarily relevant
# for the source-build `install` path (used by local devs and the ADO
# backend) and is best-effort (skipped silently) for `binstall`.
#
# Scope policy: only system libs that an ox-check catalog tool DIRECTLY
# requires. We do not try to be a general-purpose dev-env doctor.
# Current entries:
#   - libclang: required by cargo-spellcheck (clang-sys / hunspell-rs)
#     at build time.
#
# Detection uses presence-only probes (file existence in standard install
# dirs + `LIBCLANG_PATH` env var). No version checks -- system libs upgrade
# independently and any reasonably modern libclang works for clang-sys.
#
# On missing deps the recipe prints copy-paste install hints per OS /
# package manager and exits non-zero. No auto-install: admin / sudo and
# package-manager choice stay with the user.
[group("ox-check")]
[script("pwsh")]
ox-check-system-deps-check:
    $ErrorActionPreference = 'Stop'
    $missing = @()

    # libclang: required to BUILD cargo-spellcheck from source.
    $haveLibclang = $false
    $libclangFiles = @('libclang.dll', 'libclang.so', 'libclang.so.1', 'libclang.dylib')
    if ($env:LIBCLANG_PATH) {
        foreach ($f in $libclangFiles) {
            if (Test-Path (Join-Path $env:LIBCLANG_PATH $f)) { $haveLibclang = $true; break }
        }
    }
    if (-not $haveLibclang) {
        $probes = if ($IsWindows) {
            @(
                'C:\Program Files\LLVM\bin\libclang.dll',
                "$env:USERPROFILE\scoop\apps\llvm\current\bin\libclang.dll"
            )
        } elseif ($IsMacOS) {
            @(
                '/usr/local/opt/llvm/lib/libclang.dylib',
                '/opt/homebrew/opt/llvm/lib/libclang.dylib'
            )
        } else {
            @(
                '/usr/lib/x86_64-linux-gnu/libclang.so.1',
                '/usr/lib64/libclang.so',
                '/usr/lib64/libclang.so.1'
            )
        }
        foreach ($p in $probes) {
            if (Get-Item -LiteralPath $p -ErrorAction SilentlyContinue) { $haveLibclang = $true; break }
        }
        # Linux distros often add a version suffix (libclang-19.so etc.); glob fallback.
        if (-not $haveLibclang -and -not $IsWindows -and -not $IsMacOS) {
            $glob = Get-ChildItem -Path '/usr/lib','/usr/lib64','/usr/lib/x86_64-linux-gnu' -Filter 'libclang*.so*' -ErrorAction SilentlyContinue
            if ($glob) { $haveLibclang = $true }
        }
    }
    if (-not $haveLibclang) {
        $missing += [pscustomobject]@{
            name = 'libclang'
            why = 'cargo-spellcheck (clang-sys/hunspell-rs) needs libclang at build time'
            hints = @(
                'Linux (Ubuntu/Debian):    sudo apt-get install -y libclang-dev'
                'Linux (Azure Linux/RHEL): sudo tdnf install -y clang-devel'
                'macOS (homebrew):         brew install llvm'
                'Windows (scoop):          scoop install llvm     # no admin'
                'Windows (winget, admin):  winget install LLVM.LLVM'
            )
        }
    }

    if ($missing.Count -gt 0) {
        Write-Host ''
        foreach ($m in $missing) {
            Write-Host "ox-check: missing system dependency '$($m.name)'" -ForegroundColor Yellow
            Write-Host "  why: $($m.why)"
            Write-Host '  install:'
            foreach ($h in $m.hints) { Write-Host "    $h" }
            Write-Host ''
        }
        Write-Host "If the dep is installed but not on the standard search path, set LIBCLANG_PATH and re-run." -ForegroundColor Yellow
        exit 1
    }

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
# `installer` selects the install backend:
#   - "install"  (default): ``cargo install --locked`` — pure-source
#     builds; works in any cargo environment, no extra runtime
#     dependencies. Slow (~30 minutes for the full catalog on a cold
#     runner). Used by ADO, where ``cargo-binstall`` has known
#     compliance issues for internal pipelines.
#   - "binstall": ``cargo binstall --no-confirm --locked`` — downloads
#     a pre-built binary from each tool's GitHub Releases when
#     available, falling back to ``cargo install`` per-tool when
#     not. Cuts catalog install time from ~30 min to ~1 min on a
#     cold runner. Requires ``cargo-binstall`` to be installed
#     beforehand. Used by the GitHub Actions setup action.
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
ox-check-tools-install installer="install":
    $ErrorActionPreference = 'Continue'
    $installer = '{{installer}}'
    if ($installer -ne 'install' -and $installer -ne 'binstall') {
        Write-Error "ox-check-tools-install: unknown installer '$installer' (expected 'install' or 'binstall')"
        exit 2
    }
    # System-level prerequisites first -- the binstall path downloads
    # prebuilt binaries and does not need them, but the source-build
    # path does. Failing fast with a clear install hint is much better
    # than a cryptic clang-sys build error 10 minutes into the install.
    if ($installer -eq 'install') {
        & just ox-check-system-deps-check
        if ($LASTEXITCODE -ne 0) {
            Write-Error "ox-check-tools-install: system dependency check failed; install the missing libs and re-run."
            exit $LASTEXITCODE
        }
    }
    $catalog = 'justfiles/ox-check/tool-minimums.txt'
    $failed = [System.Collections.Generic.List[string]]::new()
    foreach ($line in Get-Content $catalog) {
        $trimmed = $line.Trim()
        if ($trimmed -eq '' -or $trimmed.StartsWith('#')) { continue }
        if ($trimmed -match '^([^=]+)=(.+)$') {
            $tool = $Matches[1]
            $min = $Matches[2]
            # Skip if the tool is already installed at or above the
            # required version. This is what makes the actions/cache
            # restore actually useful — without the early-exit, every
            # post-restore run would try to re-install on top of the
            # cached binaries and fail with "binary already exists in
            # destination". The `2>$null` swallows the help-text noise
            # _ox-check-require emits when the tool is missing.
            & just _ox-check-require $tool 2>$null
            if ($LASTEXITCODE -eq 0) {
                Write-Host "ox-check-tools-install ($installer): $tool >= $min (already satisfied)"
                continue
            }
            Write-Host "ox-check-tools-install ($installer): $tool >= $min"
            $installed = $false
            if ($installer -eq 'binstall') {
                cargo binstall --no-confirm --locked $tool --version ">=$min"
                if ($LASTEXITCODE -eq 0) {
                    $installed = $true
                } else {
                    Write-Host "  binstall failed for $tool; falling back to cargo install" -ForegroundColor Yellow
                    cargo install --locked $tool --version ">=$min"
                    if ($LASTEXITCODE -eq 0) { $installed = $true }
                }
            } else {
                cargo install --locked $tool --version ">=$min"
                if ($LASTEXITCODE -eq 0) { $installed = $true }
            }
            if (-not $installed) {
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
