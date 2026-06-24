# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See ../../docs/design/local.md S4 (Impact scoping) for the full design.
#
# Impact analysis lives in one place: the `anvil-impact` recipe. It runs
# cargo-delta against the working tree vs. the base ref and writes durable
# artifacts under target/anvil/impact/ that the per-crate check recipes
# consume via `_anvil-impact-include`. The same recipe runs locally and in
# cloud workflows; cloud workflows just share the artifacts between jobs.
#
# Escape hatch: set ANVIL_IMPACT=off in the environment that invokes `just`
# to disable scoping entirely (no git, no snapshot, no cargo-delta). Because
# `just` runs dependencies in that same environment, the guard is honored
# even when these recipes run as a `: anvil-impact` dependency of a check.
# This is how the scheduled tier stays full-workspace.

# Private. Produces the two cargo-delta snapshots under
# target/anvil/impact/snapshots/ with two independent cache keys:
#   * baseline.json  -- snapshot of the base ref. EXPENSIVE (needs a throwaway
#     git worktree). Keyed solely on the base commit sha (baseline.sha), so the
#     worktree is only recreated when the base actually moves.
#   * current.json   -- snapshot of the working tree. Cheap. Keyed on
#     <HEAD sha + working-tree-diff hash> (current.state).
[script("pwsh")]
_anvil-impact-snapshot:
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_IMPACT -eq 'off') { exit 0 }

    # cargo-delta is validated here (after the off-guard) rather than as a
    # `just` dependency so that an ANVIL_IMPACT=off run -- e.g. the scheduled
    # tier, whose jobs never install cargo-delta -- can fire `anvil-impact`
    # as a check dependency and cleanly no-op without the tool present.
    just anvil-tool-cargo-delta-validate-prereqs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    # --- Resolve + normalize the base ref (never fetch on the user's behalf). ---
    function Normalize-Base([string]$ref) {
        if (-not $ref) { return $null }
        if ($ref -match '^refs/heads/(.+)$') { return "origin/$($Matches[1])" }
        if ($ref -notmatch '/') { return "origin/$ref" }
        return $ref
    }
    $base = Normalize-Base $env:BASE_REF
    if (-not $base) { $base = Normalize-Base $env:GITHUB_BASE_REF }
    if (-not $base) { $base = Normalize-Base $env:SYSTEM_PULLREQUEST_TARGETBRANCH }
    if (-not $base) {
        foreach ($candidate in @('origin/main', 'origin/master')) {
            git rev-parse --verify "$candidate^{commit}" 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { $base = $candidate; break }
        }
    }
    if (-not $base) {
        Write-Error 'anvil-impact: could not resolve a base ref (tried BASE_REF, PR target branch, origin/main, origin/master).'
        exit 1
    }

    # Base ref must be present locally. We deliberately do NOT `git fetch` --
    # mutating git state as a side effect of a build check is surprising and
    # can race the user's own git operations.
    git rev-parse --verify "$base^{commit}" 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) {
        $branch = ($base -replace '^origin/', '')
        Write-Error "anvil-impact: base ref '$base' not found locally. Run ``git fetch origin $branch`` and retry."
        exit 1
    }
    if ((git rev-parse --is-shallow-repository) -eq 'true') {
        Write-Error 'anvil-impact: shallow clone detected; cargo-delta needs full history for the base ref. Run ``git fetch --unshallow`` and retry.'
        exit 1
    }
    $baseSha = (git rev-parse $base).Trim()

    $impactDir = 'target/anvil/impact'
    $snapDir = Join-Path $impactDir 'snapshots'
    [System.IO.Directory]::CreateDirectory($snapDir) | Out-Null
    $baselineJson = Join-Path $snapDir 'baseline.json'
    $baselineShaFile = Join-Path $snapDir 'baseline.sha'
    $currentJson = Join-Path $snapDir 'current.json'
    $currentStateFile = Join-Path $snapDir 'current.state'

    # --- Baseline snapshot: only when the base sha changed. ---
    $haveBaseline = (Test-Path $baselineJson) -and (Test-Path $baselineShaFile) -and `
        ((Get-Content -Raw $baselineShaFile).Trim() -eq $baseSha)
    if ($haveBaseline) {
        Write-Host "anvil-impact: baseline snapshot up to date (base $baseSha)"
    } else {
        Write-Host "anvil-impact: snapshotting baseline at $base ($baseSha)"
        $tmpRoot = $env:RUNNER_TEMP
        if (-not $tmpRoot) { $tmpRoot = $env:AGENT_TEMPDIRECTORY }
        if (-not $tmpRoot) { $tmpRoot = [System.IO.Path]::GetTempPath() }
        $wt = Join-Path $tmpRoot "anvil-baseline-$PID"
        if (Test-Path $wt) { git worktree remove --force $wt 2>$null | Out-Null }
        # GIT_LFS_SKIP_SMUDGE=1: the worktree inherits the repo config but not
        # the credential helper, so LFS smudge would fail. cargo-delta only
        # needs file content hashes; LFS pointer files hash consistently.
        $env:GIT_LFS_SKIP_SMUDGE = '1'
        git worktree add --detach $wt $baseSha 2>&1 | Select-Object -Last 1 | Out-Null
        if ($LASTEXITCODE -ne 0) { Write-Error "anvil-impact: failed to create baseline worktree at $wt"; exit 1 }
        try {
            Push-Location $wt
            try {
                $baseSnap = cargo delta snapshot
                if ($LASTEXITCODE -ne 0) { throw 'cargo delta snapshot (baseline) failed' }
            } finally {
                Pop-Location
            }
        } finally {
            git worktree remove --force $wt 2>$null | Out-Null
        }
        Set-Content -LiteralPath $baselineJson -Value $baseSnap -Encoding UTF8
        Set-Content -LiteralPath $baselineShaFile -Value $baseSha -Encoding UTF8 -NoNewline
    }

    # --- Current snapshot: only when the working tree changed. ---
    $headSha = (git rev-parse HEAD).Trim()
    $diffText = (git diff HEAD) + "`n" + (git status --porcelain) | Out-String
    $diffHash = [System.BitConverter]::ToString(
        [System.Security.Cryptography.SHA256]::HashData([System.Text.Encoding]::UTF8.GetBytes($diffText))
    ).Replace('-', '')
    $currentState = "$headSha $diffHash"
    $haveCurrent = (Test-Path $currentJson) -and (Test-Path $currentStateFile) -and `
        ((Get-Content -Raw $currentStateFile).Trim() -eq $currentState)
    if ($haveCurrent) {
        Write-Host 'anvil-impact: current snapshot up to date'
    } else {
        Write-Host 'anvil-impact: snapshotting working tree'
        $curSnap = cargo delta snapshot
        if ($LASTEXITCODE -ne 0) { Write-Error 'anvil-impact: cargo delta snapshot (current) failed'; exit 1 }
        Set-Content -LiteralPath $currentJson -Value $curSnap -Encoding UTF8
        Set-Content -LiteralPath $currentStateFile -Value $currentState -Encoding UTF8 -NoNewline
    }

# Public. Computes the impact set from the two snapshots and projects each
# tier into a pre-built `--package X --package Y` string (or the sentinel
# `--skip` when the tier is empty), written to include_<tier>.txt. Per-crate
# checks depend on this recipe and read those files via _anvil-impact-include.
# Cache-aware: no-ops when the snapshots haven't changed.
[group("anvil")]
[script("pwsh")]
anvil-impact: _anvil-impact-snapshot
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_IMPACT -eq 'off') { exit 0 }

    $impactDir = 'target/anvil/impact'
    $snapDir = Join-Path $impactDir 'snapshots'
    $baselineJson = Join-Path $snapDir 'baseline.json'
    $currentJson = Join-Path $snapDir 'current.json'
    $baselineShaFile = Join-Path $snapDir 'baseline.sha'
    $currentStateFile = Join-Path $snapDir 'current.state'
    $impactJson = Join-Path $impactDir 'impact.json'
    $impactStateFile = Join-Path $impactDir 'impact.state'

    # Content cache key: regenerate the projection only when a snapshot moved
    # or an output is missing. Keyed on the snapshot markers, not mtimes, so a
    # downstream cloud-workflow job that downloaded the artifact recognizes it.
    $wantState = ((Get-Content -Raw $baselineShaFile).Trim()) + ' ' + ((Get-Content -Raw $currentStateFile).Trim())
    $includeFiles = @('modified', 'affected', 'required') | ForEach-Object { Join-Path $impactDir "include_$_.txt" }
    $fresh = (Test-Path $impactJson) -and (Test-Path $impactStateFile) -and `
        ((Get-Content -Raw $impactStateFile).Trim() -eq $wantState) -and `
        (@($includeFiles | Where-Object { -not (Test-Path $_) }).Count -eq 0)
    if ($fresh) {
        Write-Host 'anvil-impact: impact set up to date (cache hit)'
        exit 0
    }

    # Full impact JSON -- the durable source of truth. cargo-delta prints
    # nothing when no files changed between baseline and current (it
    # "quits" early), so default to an empty object so impact.json always
    # exists -- the freshness check above keys on its presence, and a
    # missing file would force a recompute on every invocation.
    $impactOut = (cargo delta impact --baseline $baselineJson --current $currentJson | Out-String)
    if ($LASTEXITCODE -ne 0) { Write-Error 'anvil-impact: cargo delta impact failed'; exit 1 }
    if ([string]::IsNullOrWhiteSpace($impactOut)) { $impactOut = '{}' }
    Set-Content -LiteralPath $impactJson -Value $impactOut.Trim() -Encoding UTF8

    # Build a name -> package-name map from cargo metadata. cargo-delta
    # emits *library* names (snake_case, e.g. cargo_ensure_no_cyclic_deps),
    # but cargo --package wants the *package* name (which may be hyphenated,
    # e.g. cargo-ensure-no-cyclic-deps). Map both the package name (identity)
    # and each target's lib name to the package name; names that match
    # neither are dropped with a warning (defensive; rare).
    $pkgMap = @{}
    $meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    foreach ($p in $meta.packages) {
        $pkgMap[$p.name] = $p.name
        foreach ($t in $p.targets) {
            if ($t.kind -contains 'lib') { $pkgMap[$t.name] = $p.name }
        }
    }

    foreach ($tier in @('modified', 'affected', 'required')) {
        $names = cargo delta impact --baseline $baselineJson --current $currentJson -f names "--$tier" 2>$null
        $pkgs = @($names | Where-Object { $_ -and $_.Trim() } | ForEach-Object { $_.Trim() })
        $args = @()
        $seen = @{}
        foreach ($pkg in $pkgs) {
            $mapped = $pkgMap[$pkg]
            if ($mapped) {
                if (-not $seen[$mapped]) { $args += @('--package', $mapped); $seen[$mapped] = $true }
            } else {
                Write-Host "anvil-impact: dropping unknown package '$pkg' from $tier set" -ForegroundColor Yellow
            }
        }
        $value = if ($args.Count -eq 0) { '--skip' } else { $args -join ' ' }
        Set-Content -LiteralPath (Join-Path $impactDir "include_$tier.txt") -Value $value -Encoding UTF8 -NoNewline
        Write-Host "anvil-impact: $tier -> $value"
    }
    Set-Content -LiteralPath $impactStateFile -Value $wantState -Encoding UTF8 -NoNewline

# Private. Resolves a tier's scope for a check recipe and echoes it on stdout.
#   1. ANVIL_IMPACT=off            -> tier default (full-workspace / run).
#   2. include_<tier>.txt present  -> its contents (a --package list or --skip).
#   3. otherwise                   -> tier default.
# Tier default: --workspace for affected/required; empty (run) for modified.
[script("pwsh")]
_anvil-impact-include tier:
    $ErrorActionPreference = 'Stop'
    $tier = '{{ tier }}'
    $default = if ($tier -eq 'modified') { '' } else { '--workspace' }
    if ($env:ANVIL_IMPACT -eq 'off') { Write-Output $default; exit 0 }
    $file = "target/anvil/impact/include_$tier.txt"
    if (Test-Path $file) {
        Write-Output ((Get-Content -Raw $file).Trim())
    } else {
        Write-Output $default
    }
