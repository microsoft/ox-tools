# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md

# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/local.md
#
# Impact analysis lives in one place: the `anvil-impact` recipe. It runs
# cargo-delta against the working tree vs. the base ref and writes durable
# artifacts under target/anvil/impact/ that the per-crate check recipes
# consume via _anvil-impact-include. The SAME recipe runs locally and in
# cloud workflows: a cloud impact job runs `just anvil-impact` and uploads
# target/anvil/impact/ as an artifact; each downstream group job downloads
# it, so the checks read the cache instead of recomputing.
#
# The base-ref resolution and the impact -> `--package name@version` tier
# projection are the SHARED helpers _anvil-base-ref and _anvil-impact-format
# (helpers.just); this recipe orchestrates the snapshots + caching around
# them so the exact same projection is used locally and in CI.
#
# Escape hatch: set ANVIL_IMPACT=off in the environment that invokes `just`
# to disable scoping entirely (no git, no snapshot, no cargo-delta). Because
# `just` runs dependencies in that same environment, the guard is honored
# even when these recipes run as a `: anvil-impact` dependency of a check.
# This is how the scheduled tier stays full-workspace.
#
# Set ANVIL_IMPACT=consume when an authoritative target/anvil/impact cache is
# already present and must be trusted verbatim -- e.g. a cloud-workflow group
# job that downloaded the impact artifact the impact job produced. In that
# mode anvil-impact is a pure no-op: it neither snapshots nor recomputes (so
# it needs neither cargo-delta nor a fetched base ref, which group jobs lack),
# and the checks read the downloaded include_<tier>.txt files directly.

# Private. Produces the two cargo-delta snapshots under
# target/anvil/impact/snapshots/ with two independent cache keys:
#   * baseline.json  -- snapshot of the base ref. EXPENSIVE (needs a throwaway
#     git worktree). Keyed solely on the base commit sha (baseline.sha), so the
#     worktree is only recreated when the base actually moves.
#   * current.json   -- snapshot of the working tree. Cheap. Keyed on
#     <HEAD sha + working-tree-diff hash> (current.state).

# Snapshot the base ref and working tree for impact analysis.
[script("pwsh")]
_anvil-impact-snapshot:
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_IMPACT -eq 'off' -or $env:ANVIL_IMPACT -eq 'consume') { exit 0 }

    $impactDir = 'target/anvil/impact'
    $snapDir = Join-Path $impactDir 'snapshots'
    $baselineJson = Join-Path $snapDir 'baseline.json'
    $baselineShaFile = Join-Path $snapDir 'baseline.sha'
    $currentJson = Join-Path $snapDir 'current.json'
    $currentStateFile = Join-Path $snapDir 'current.state'
    $noWorkspaceMarker = Join-Path $snapDir 'baseline-no-workspace'

    # Working-tree state key -- git only, no cargo-delta, no base ref. Pins
    # HEAD plus the working-tree diff, so a match means the snapshots were
    # produced for exactly this tree. target/ is excluded: it holds build
    # output and the impact cache itself (and is untracked in some adopter
    # repos), so it must not perturb the key -- otherwise the state would never
    # match once target/ exists, and a downstream consumer that downloaded the
    # cache into target/anvil/impact would look "changed".
    $headSha = (git rev-parse HEAD).Trim()
    $statusLines = @(git status --porcelain | Where-Object { $_ -and ($_.Substring(3) -notmatch '^"?target/') })
    $diffText = (git diff HEAD | Out-String) + "`n" + ($statusLines -join "`n")
    $diffHash = [System.BitConverter]::ToString(
        [System.Security.Cryptography.SHA256]::HashData([System.Text.Encoding]::UTF8.GetBytes($diffText))
    ).Replace('-', '')
    $currentState = "$headSha $diffHash"

    # Consumer fast path: reuse an already-present, matching snapshot set
    # WITHOUT cargo-delta or a fetched base ref. This is what lets a
    # cloud-workflow group job -- which installs neither cargo-delta nor
    # fetches the base ref -- consume the target/anvil/impact artifact produced
    # by the impact job instead of recomputing it (and it also short-circuits
    # a local repeat run). We still detect a moved base ref when it is cheaply
    # resolvable (best effort): if the base cannot be resolved locally (e.g. a
    # shallow CI consumer), we trust the present baseline -- the base cannot
    # move within a single pipeline run.
    $currentFresh = (Test-Path $currentJson) -and (Test-Path $currentStateFile) -and `
        ((Get-Content -Raw $currentStateFile).Trim() -eq $currentState)
    $baseSha = $null
    $base = (& "{{ just_executable() }}" _anvil-base-ref 2>$null)
    if (($LASTEXITCODE -eq 0) -and $base) {
        $base = ($base | Select-Object -First 1).Trim()
        git rev-parse --verify "$base^{commit}" 2>$null | Out-Null
        if ($LASTEXITCODE -eq 0) { $baseSha = (git rev-parse $base).Trim() }
    }
    $baselineFresh = (Test-Path $baselineShaFile) -and `
        ((Test-Path $baselineJson) -or (Test-Path $noWorkspaceMarker)) -and `
        (($null -eq $baseSha) -or ((Get-Content -Raw $baselineShaFile).Trim() -eq $baseSha))
    if ($currentFresh -and $baselineFresh) {
        Write-Host 'anvil-impact: snapshots up to date'
        exit 0
    }

    # --- (Re)compute path: this is the only path that needs cargo-delta and a
    # resolvable base ref (the impact job in cloud workflows, or a local first
    # run / cache miss). ---
    # cargo-delta is validated here -- inside the recompute path, after the
    # off/consume guard and the fast path -- rather than as a hard `just`
    # dependency of `anvil-impact`. Only recompute needs the tool; the paths
    # that skip it (ANVIL_IMPACT=off/consume, or a cache hit) must be able to
    # no-op without it. Groups that can actually reach recompute declare
    # cargo-delta as their own setup prereq (see groups/*.just), so a local
    # `just anvil-<group>` still fails fast if it is missing; keeping it off
    # anvil-impact's dependency line is what lets the no-op paths stay
    # tool-agnostic.
    & "{{ just_executable() }}" anvil-tool-cargo-delta-validate-prereqs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    [System.IO.Directory]::CreateDirectory($snapDir) | Out-Null

    # Ensure a base sha. The fast path probed it best-effort (2>$null); if that
    # failed we truly need it now, so resolve with fail-fast diagnostics. We
    # deliberately do NOT `git fetch`: mutating git state as a side effect of a
    # build check is surprising and can race the user's own git operations.
    if (-not $baseSha) {
        $base = (& "{{ just_executable() }}" _anvil-base-ref)
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        $base = $base.Trim()
        git rev-parse --verify "$base^{commit}" 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            $branch = ($base -replace '^origin/', '')
            Write-Error "anvil-impact: base ref '$base' not found locally. Run ``git fetch origin $branch`` and retry."
            exit 1
        }
        $baseSha = (git rev-parse $base).Trim()
    }
    if ((git rev-parse --is-shallow-repository) -eq 'true') {
        Write-Error 'anvil-impact: shallow clone detected; cargo-delta needs full history for the base ref. Run ``git fetch --unshallow`` and retry.'
        exit 1
    }

    # --- Baseline snapshot: only when the base sha changed. ---
    $haveBaseline = (Test-Path $baselineShaFile) -and `
        ((Get-Content -Raw $baselineShaFile).Trim() -eq $baseSha) -and `
        ((Test-Path $baselineJson) -or (Test-Path $noWorkspaceMarker))
    if ($haveBaseline) {
        Write-Host "anvil-impact: baseline snapshot up to date (base $baseSha)"
    } else {
        Write-Host "anvil-impact: snapshotting baseline at $base ($baseSha)"
        Remove-Item -LiteralPath $noWorkspaceMarker -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $baselineJson -ErrorAction SilentlyContinue
        # The baseline worktree is checked out at the merge target, whose
        # rust-toolchain.toml may pin a toolchain that is NOT installed on this
        # machine (any PR that bumps rust-toolchain.toml hits this). The
        # snapshot is metadata-only (file presence + content hashes), so run it
        # under the *active* toolchain via RUSTUP_TOOLCHAIN -- resolved here, in
        # the current checkout, where it is installed -- which overrides the
        # worktree's rust-toolchain.toml. Best-effort: skip if rustup is absent.
        $activeToolchain = $null
        $rt = (rustup show active-toolchain 2>$null)
        if (($LASTEXITCODE -eq 0) -and $rt) { $activeToolchain = (($rt | Select-Object -First 1) -split '\s+')[0] }

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
            if (-not (Test-Path (Join-Path $wt 'Cargo.toml'))) {
                # First-time anvil adoption: the base predates the cargo
                # workspace (no root Cargo.toml), so there is nothing for
                # cargo-delta to snapshot. Record a marker; anvil-impact falls
                # back to full-workspace validation rather than failing.
                Write-Host "anvil-impact: base '$base' has no root Cargo.toml (first-time adoption?); falling back to full-workspace"
                New-Item -ItemType File -Path $noWorkspaceMarker -Force | Out-Null
                Set-Content -LiteralPath $baselineShaFile -Value $baseSha -Encoding UTF8 -NoNewline
            } else {
                Push-Location $wt
                try {
                    if ($activeToolchain) { $env:RUSTUP_TOOLCHAIN = $activeToolchain }
                    $baseSnap = cargo delta snapshot
                    if ($LASTEXITCODE -ne 0) { throw 'cargo delta snapshot (baseline) failed' }
                } finally {
                    Remove-Item Env:\RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue
                    Pop-Location
                }
                Set-Content -LiteralPath $baselineJson -Value $baseSnap -Encoding UTF8
                Set-Content -LiteralPath $baselineShaFile -Value $baseSha -Encoding UTF8 -NoNewline
            }
        } finally {
            git worktree remove --force $wt 2>$null | Out-Null
        }
    }

    # --- Current snapshot: only when the working tree changed
    # ($currentState was computed at the top of this recipe). ---
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
# tier into a pre-built `--package name@version ...` string (or the sentinel
# `--skip` when the tier is empty) via the shared _anvil-impact-format
# helper, written to include_<tier>.txt. Per-crate checks depend on this
# recipe and read those files via _anvil-impact-include. Cache-aware: no-ops
# when the snapshots haven't changed.

# Compute the impact set and write the target/anvil/impact cache.
[group("anvil")]
[script("pwsh")]
anvil-impact: _anvil-impact-snapshot
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_IMPACT -eq 'off' -or $env:ANVIL_IMPACT -eq 'consume') { exit 0 }

    $impactDir = 'target/anvil/impact'
    $snapDir = Join-Path $impactDir 'snapshots'
    $baselineJson = Join-Path $snapDir 'baseline.json'
    $currentJson = Join-Path $snapDir 'current.json'
    $baselineShaFile = Join-Path $snapDir 'baseline.sha'
    $currentStateFile = Join-Path $snapDir 'current.state'
    $impactJson = Join-Path $impactDir 'impact.json'
    $impactStateFile = Join-Path $impactDir 'impact.state'

    # Local safety net + warning for a dirty working tree. cargo-delta scopes
    # on the *committed* diff of HEAD against the base ref, so uncommitted
    # changes -- a crate you are actively editing but have not committed -- are
    # invisible to it and would be silently scoped out (its checks skipped).
    # When the tree has any uncommitted change (tracked edits or new,
    # non-ignored untracked files), warn and widen every tier to the full
    # workspace so work-in-progress is never skipped; commit to scope by impact
    # instead. This runs BEFORE the cache-freshness check below so the warning
    # is shown on every dirty invocation, not just the first. Cloud-workflow
    # checkouts are clean, so this only affects local runs. cargo-delta and
    # anvil write only under target/ (snapshots, include files), and target/
    # may not be gitignored in every adopter repo, so exclude it from the probe
    # -- otherwise anvil's own artifacts would make every run look dirty.
    $dirty = @(git status --porcelain | Where-Object { $_ -and ($_.Substring(3) -notmatch '^"?target/') })
    if ($dirty.Count -gt 0) {
        Write-Host "anvil-impact: warning: $($dirty.Count) uncommitted change(s) in the working tree are invisible to cargo-delta (it scopes on the committed diff vs the base ref); widening all tiers to --workspace so nothing you are editing is skipped -- commit to scope by impact, or set ANVIL_IMPACT=off to disable scoping." -ForegroundColor Yellow
        Set-Content -LiteralPath (Join-Path $impactDir 'include_modified.txt') -Value '' -Encoding UTF8 -NoNewline
        Set-Content -LiteralPath (Join-Path $impactDir 'include_affected.txt') -Value '--workspace' -Encoding UTF8 -NoNewline
        Set-Content -LiteralPath (Join-Path $impactDir 'include_required.txt') -Value '--workspace' -Encoding UTF8 -NoNewline
        Set-Content -LiteralPath $impactJson -Value '{}' -Encoding UTF8
        # Invalidate any prior clean-tree cache so committing recomputes.
        Remove-Item -LiteralPath $impactStateFile -ErrorAction SilentlyContinue
        exit 0
    }

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

    # First-time anvil adoption: the base predates the cargo workspace, so
    # there is no baseline to diff against. Fall back to full-workspace
    # validation -- modified runs unconditionally (empty), affected/required
    # default to --workspace -- rather than failing the impact computation.
    if (Test-Path (Join-Path $snapDir 'baseline-no-workspace')) {
        Write-Host 'anvil-impact: baseline has no workspace; defaulting all tiers to full-workspace'
        Set-Content -LiteralPath (Join-Path $impactDir 'include_modified.txt') -Value '' -Encoding UTF8 -NoNewline
        Set-Content -LiteralPath (Join-Path $impactDir 'include_affected.txt') -Value '--workspace' -Encoding UTF8 -NoNewline
        Set-Content -LiteralPath (Join-Path $impactDir 'include_required.txt') -Value '--workspace' -Encoding UTF8 -NoNewline
        Set-Content -LiteralPath $impactJson -Value '{}' -Encoding UTF8
        Set-Content -LiteralPath $impactStateFile -Value $wantState -Encoding UTF8 -NoNewline
        exit 0
    }

    # Full impact JSON -- the durable source of truth. cargo-delta prints
    # nothing when no files changed between baseline and current (it "quits"
    # early), so default to an empty object so impact.json always exists --
    # the freshness check above keys on its presence, and a missing file
    # would force a recompute on every invocation.
    $impactOut = (cargo delta impact --baseline $baselineJson --current $currentJson --format json | Out-String)
    if ($LASTEXITCODE -ne 0) { Write-Error 'anvil-impact: cargo delta impact failed'; exit 1 }
    if ([string]::IsNullOrWhiteSpace($impactOut)) { $impactOut = '{}' }
    Set-Content -LiteralPath $impactJson -Value $impactOut.Trim() -Encoding UTF8

    # Project each tier into its `--package name@version ...` / `--skip`
    # string via the shared formatter (version-qualified specs + lib-name ->
    # package-name translation live there, used identically by the cloud
    # impact steps).
    foreach ($tier in @('modified', 'affected', 'required')) {
        $value = (& "{{ just_executable() }}" _anvil-impact-format $tier $impactJson)
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        Set-Content -LiteralPath (Join-Path $impactDir "include_$tier.txt") -Value $value.Trim() -Encoding UTF8 -NoNewline
        Write-Host "anvil-impact: $tier -> $($value.Trim())"
    }
    Set-Content -LiteralPath $impactStateFile -Value $wantState -Encoding UTF8 -NoNewline

# Private. Resolves a tier's scope for a check recipe and echoes it on stdout.
#   1. ANVIL_IMPACT=off            -> tier default (full-workspace / run).
#   2. include_<tier>.txt present  -> its contents (a --package list or --skip).
#   3. otherwise                   -> tier default.
# Tier default: --workspace for affected/required; empty (run) for modified.
# Each scoped check calls this directly (into a local $include variable) to
# read its tier's scope from the target/anvil/impact cache -- the same cache
# both locally and in cloud workflows (where the group job downloaded it).

# Resolve a tier's package scope from the impact cache.
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