# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/checks.md for the catalog and rationale.
#
# Each check belongs to one of four buckets, which determines how it
# interprets the impact env vars emitted by the cargo-delta impact step
# in CI:
#
#   - "modified": only run when at least one package's source files
#     changed in the diff. The check's underlying tool is workspace-wide
#     or directory-scoped (cargo fmt --all, cargo heather, cargo
#     spellcheck), so it doesn't take --package; we short-circuit on
#     the OX_CHECK_INCLUDE_MODIFIED == "--skip" sentinel.
#
#   - "affected": run on the affected set (modified ∪ reverse-deps
#     within the workspace). The check's underlying tool takes
#     --package; we splice OX_CHECK_INCLUDE_AFFECTED into the cargo
#     invocation, defaulting to --workspace for local invocations where
#     no env var is set.
#
#   - "required": run on the required set (affected ∪ workspace-internal
#     transitive deps). Same splice/default pattern as affected, but
#     keyed on OX_CHECK_INCLUDE_REQUIRED. Used for checks whose tool
#     resolves through the dep graph (cargo doc → intra-doc links;
#     cargo hack → feature powerset; cargo udeps → unused-deps).
#
#   - "unscoped": always run, no env var reference. External-input
#     checks (deny, audit, aprz) and PR-context checks (pr-title) live
#     here. Scheduled-exhaustive recipes (mutants-full) are also unscoped
#     by design.
#
# Local invocation (no impact wiring): all three env vars are unset;
# modified recipes run unconditionally and affected/required recipes
# default to --workspace.
#
# Every recipe whose body uses multi-line conditionals or env-var
# splicing is annotated with [script("pwsh")]. pwsh is preinstalled on
# Windows (since Windows 10), on GH/ADO hosted Linux + Windows
# runners, and installable on macOS via Homebrew or the upstream
# installer. We chose pwsh over bash because just's shebang dispatch
# requires `cygpath` on Windows (only on PATH from inside Git Bash),
# while [script("pwsh")] works from plain PowerShell with no PATH
# augmentation. The existing _ox-check-require pwsh precedent already
# established pwsh as a tools-floor requirement.
#
# Single-command recipes (cargo deny check, cargo audit) are plain
# just recipes that inherit the adopter's default shell — they're
# shell-agnostic.
#
# Note: [script(...)] requires `set unstable`. The adopter's root
# justfile must declare it (typically as a top-level line). ox-check's
# mod.just does NOT redeclare it, to avoid conflicting with adopters
# who already have it.

# === pr-fast members ====================================================

# Modified tier. cargo-fmt is a rustup component. We invoke it via the
# pinned nightly (see versions.just) because rustfmt.toml uses
# unstable_features = true (imports_granularity, group_imports,
# format_code_in_doc_comments). Floating nightly would mean
# format-drift breaking CI on rustup updates — the same trap we
# explicitly avoid for udeps/miri/careful/external-types.
[script("pwsh")]
ox-check-fmt:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-fmt: no modified packages; skipping'
        exit 0
    }
    cargo '+{{ rust_nightly }}' fmt --all --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier. Per project policy, clippy runs on the affected set
# rather than the modified set: a change in a crate can introduce a
# clippy issue in a dependent crate (e.g., trait-bound or
# obviously-truthy-condition lints that key off the changed type), so
# we want downstream rev-deps to lint as well. cargo-clippy is a
# rustup component; same reasoning as fmt for the require.
[script("pwsh")]
ox-check-clippy:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-clippy: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    & cargo clippy @pkg --all-targets --all-features --locked "--" '-D' 'warnings'
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-cargo-sort: (_ox-check-require "cargo-sort")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-cargo-sort: no modified packages; skipping'
        exit 0
    }
    cargo sort --workspace --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-license-headers: (_ox-check-require "cargo-heather")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-license-headers: no modified packages; skipping'
        exit 0
    }
    cargo heather
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-ensure-no-cyclic-deps: (_ox-check-require "cargo-ensure-no-cyclic-deps")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-ensure-no-cyclic-deps: no modified packages; skipping'
        exit 0
    }
    cargo ensure-no-cyclic-deps
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-ensure-no-default-features: (_ox-check-require "cargo-ensure-no-default-features")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-ensure-no-default-features: no modified packages; skipping'
        exit 0
    }
    cargo ensure-no-default-features
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Required tier. cargo doc resolves intra-doc links through the dep
# graph, so a dep changing its public API can break doc-build in a
# crate that wasn't itself modified.
[script("pwsh")]
ox-check-doc-build:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_REQUIRED -eq '--skip') {
        Write-Host 'ox-check-doc-build: no required packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_REQUIRED) { -split $env:OX_CHECK_INCLUDE_REQUIRED } else { '--workspace' })
    $env:RUSTDOCFLAGS = '-D warnings'
    & cargo doc @pkg --all-features --no-deps
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
#
# cargo-doc2readme regenerates a crate's README.md from its rustdoc.
# Two extension points users frequently need:
#   * Workspace-level template (`crates/README.j2` or `README.j2` at repo
#     root): a Tera template applied to every crate's README. ox-check
#     auto-detects it and passes `--template` to the per-crate runs.
#   * Per-crate opt-out: hand-crafted READMEs (e.g. a tool crate whose
#     README is more freeform than the lib docs) opt out by adding
#     `[package.metadata.ox-gen-readme]\ndisable = true` to their
#     Cargo.toml. ox-check skips those crates.
#
# Bin-only crates have no library rustdoc to base a README on, so they
# are skipped as well (cargo doc2readme requires a library target).
[script("pwsh")]
ox-check-readme-check: (_ox-check-require "cargo-doc2readme")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-readme-check: no modified packages; skipping'
        exit 0
    }
    # Detect a workspace-level README template. Two conventional
    # locations: crates/README.j2 (cargo-workspaces idiom) or
    # README.j2 at repo root.
    $template = $null
    foreach ($candidate in 'crates/README.j2', 'README.j2') {
        if (Test-Path $candidate) { $template = (Resolve-Path $candidate).Path; break }
    }
    # Iterate library crates. Filter by impact set when set, then drop
    # bin-only crates and opt-outs.
    $pkg = @(if ($env:OX_CHECK_INCLUDE_MODIFIED) { -split $env:OX_CHECK_INCLUDE_MODIFIED } else { '--workspace' })
    $meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    $byName = @{}
    foreach ($p in $meta.packages) { $byName[$p.name] = $p }
    $candidates = if ($pkg -contains '--workspace') {
        @($meta.packages | ForEach-Object { $_.name })
    } else {
        $names = @()
        for ($i = 0; $i -lt $pkg.Count; $i++) {
            if ($pkg[$i] -eq '--package' -and ($i + 1) -lt $pkg.Count) {
                $names += $pkg[$i + 1]; $i++
            }
        }
        $names
    }
    $hadFailure = $false
    foreach ($name in $candidates) {
        $p = $byName[$name]
        if (-not $p) { continue }
        if (-not ($p.targets | Where-Object { $_.kind -contains 'lib' })) { continue }
        # Skip private crates (publish = false). They aren't released
        # and rarely have a polished README. Mirrors the
        # `cargo workspaces exec --ignore-private` idiom adopters
        # commonly use.
        if ($p.publish -is [array] -and $p.publish.Count -eq 0) {
            Write-Host "ox-check-readme-check: $name (skipped: publish = false)"
            continue
        }
        $disabled = $false
        if ($p.metadata -and $p.metadata.'ox-gen-readme' -and $p.metadata.'ox-gen-readme'.disable) {
            $disabled = $true
        }
        if ($disabled) {
            Write-Host "ox-check-readme-check: $name (opted out via [package.metadata.ox-gen-readme])"
            continue
        }
        Write-Host "ox-check-readme-check: $name"
        # cargo doc2readme writes / compares relative to its CWD (not
        # --manifest-path), so chdir into the crate before invoking
        # --check. We also compute a per-crate relative path to the
        # workspace-level template so the same template file works for
        # every crate (parallels the cargo-workspaces idiom).
        $crateDir = Split-Path -Parent $p.manifest_path
        Push-Location $crateDir
        try {
            $relTemplate = if ($template) {
                Resolve-Path -Relative -LiteralPath $template
            } else {
                $null
            }
            $args = @('doc2readme', '--check')
            if ($relTemplate) { $args += @('--template', $relTemplate) }
            & cargo @args
            if ($LASTEXITCODE -ne 0) { $hadFailure = $true }
        } finally {
            Pop-Location
        }
    }
    if ($hadFailure) { exit 1 }

# Modified tier.
#
# cargo-spellcheck reads a Hunspell-compatible dictionary file at the
# path configured in spellcheck.toml (typically `extra_dictionaries =
# ["target/spelling.dic"]`). The convention used by the surveyed
# Microsoft Rust repos is to keep the *source* word list in a
# human-edited `.spelling` file at the repo root and preprocess it
# into the .dic format at check time (Hunspell .dic requires:
# alphabetical sort, blank/numeric lines removed, line-count header).
# If `.spelling` is present, we generate `target/spelling.dic` from it
# automatically; otherwise we run cargo-spellcheck against whatever
# the repo has already set up.
[script("pwsh")]
ox-check-spellcheck: (_ox-check-require "cargo-spellcheck")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-spellcheck: no modified packages; skipping'
        exit 0
    }
    if (Test-Path '.spelling') {
        $output_file = 'target/spelling.dic'
        $lines = Get-Content '.spelling' | Sort-Object
        $filtered_lines = $lines | Where-Object { $_ -notmatch '^\d+$' -and $_ -ne '' }
        $line_count = $filtered_lines.Count
        [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($output_file)) | Out-Null
        @($line_count) + $filtered_lines | Set-Content $output_file
    }
    # Pass --cfg explicitly when a spellcheck.toml exists at repo root,
    # otherwise cargo-spellcheck falls back to its built-in defaults and
    # ignores user-curated dictionaries (`extra_dictionaries`, custom
    # hunspell langs, etc.).
    if (Test-Path 'spellcheck.toml') {
        cargo spellcheck --cfg spellcheck.toml check --code 1
    } else {
        cargo spellcheck check --code 1
    }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Unscoped (PR title is not source-related).
[script("pwsh")]
ox-check-pr-title: (_ox-check-require "pwsh")
    $title = $env:PR_TITLE
    if (-not $title) { $title = git log -1 --pretty=%s HEAD }
    if ($title -notmatch '^(feat|fix|chore|docs|refactor|test|build|ci|perf|revert)(\([^)]+\))?!?: .+') {
        Write-Error "PR title '$title' does not match Conventional Commits"
        exit 1
    }

# Unscoped (consults external advisory DB; reads Cargo.lock, not
# workspace members). Single command — inherits adopter's default shell.
ox-check-deny: (_ox-check-require "cargo-deny")
    cargo deny check

# Unscoped (consults external advisory DB; reads Cargo.lock).
ox-check-audit: (_ox-check-require "cargo-audit")
    cargo audit

# Required tier. cargo-udeps detects unused dependencies by resolving
# the full crate graph and seeing which deps are referenced; that's
# precisely what the required tier is for. Pinned to the general
# nightly defined in versions.just.
[script("pwsh")]
ox-check-udeps: (_ox-check-require "cargo-udeps")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_REQUIRED -eq '--skip') {
        Write-Host 'ox-check-udeps: no required packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_REQUIRED) { -split $env:OX_CHECK_INCLUDE_REQUIRED } else { '--workspace' })
    & cargo '+{{ rust_nightly }}' udeps @pkg --all-targets --all-features
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier (compares published API of changed crates against the
# baseline; only changed crates' surface is at risk).
#
# Several real-world conditions produce errors that aren't actually
# SemVer violations:
#   - bin-only crates have no API to compare ("no library targets found").
#   - crates not yet published to crates.io ("not found in registry").
#   - crates where the published baseline lacks a lib target the
#     current source has (bin -> bin+lib transition).
# We pre-filter to library-bearing crates from cargo metadata, then
# run cargo-semver-checks per-package and tolerate the
# "no-comparable-baseline" failure modes.
[script("pwsh")]
ox-check-semver-check: (_ox-check-require "cargo-semver-checks")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-semver-check: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    # Build the candidate package list. Always iterate per-package over
    # library crates only -- cargo-semver-checks --workspace would fail
    # on workspaces that contain bin-only crates ("no library targets
    # found"), and we want the same tolerance for unpublished / bin->lib-
    # transition crates regardless of whether we got here via impact-
    # scoping (CI) or full-workspace fallback (local).
    $libPkgs = @{}
    $meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    foreach ($p in $meta.packages) {
        if ($p.targets | Where-Object { $_.kind -contains 'lib' }) {
            $libPkgs[$p.name] = $true
        }
    }
    if ($pkg -contains '--workspace') {
        $packages = @($libPkgs.Keys)
    } else {
        $packages = @()
        for ($i = 0; $i -lt $pkg.Count; $i++) {
            if ($pkg[$i] -eq '--package' -and ($i + 1) -lt $pkg.Count) {
                if ($libPkgs[$pkg[$i + 1]]) { $packages += $pkg[$i + 1] }
                $i++
            }
        }
    }
    if ($packages.Count -eq 0) {
        Write-Host 'ox-check-semver-check: no affected library crates; skipping'
        exit 0
    }
    $hadRealFailure = $false
    foreach ($p in $packages) {
        Write-Host "ox-check-semver-check: $p"
        $output = (& cargo semver-checks --package $p 2>&1) | Out-String
        if ($LASTEXITCODE -ne 0) {
            if ($output -match 'not found in registry|no library targets found') {
                Write-Host "  $p has no comparable baseline; skipping (likely unpublished or bin->lib transition)" -ForegroundColor Yellow
            } else {
                Write-Host $output
                $hadRealFailure = $true
            }
        }
    }
    if ($hadRealFailure) { exit 1 }

# Affected tier (lints public API of changed crates and rev-deps).
#
# cargo-check-external-types is per-manifest: no --package/--workspace,
# only --manifest-path. Iterate the affected library crates and run
# the tool once each, pointing at the crate's Cargo.toml. Bin-only
# crates have no public API surface and are skipped.
[script("pwsh")]
ox-check-external-types: (_ox-check-require "cargo-check-external-types")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-external-types: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    # Build pkg-name -> manifest-path map, restricted to library crates.
    $libPkgs = @{}
    $meta = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    foreach ($p in $meta.packages) {
        if ($p.targets | Where-Object { $_.kind -contains 'lib' }) {
            $libPkgs[$p.name] = $p.manifest_path
        }
    }
    # Decide which packages to check.
    $packages = @()
    if ($pkg -contains '--workspace') {
        $packages = $libPkgs.Keys
    } else {
        for ($i = 0; $i -lt $pkg.Count; $i++) {
            if ($pkg[$i] -eq '--package' -and ($i + 1) -lt $pkg.Count) {
                if ($libPkgs.ContainsKey($pkg[$i + 1])) { $packages += $pkg[$i + 1] }
                $i++
            }
        }
    }
    if ($packages.Count -eq 0) {
        Write-Host 'ox-check-external-types: no affected library crates; skipping'
        exit 0
    }
    $failed = $false
    foreach ($p in $packages) {
        Write-Host "ox-check-external-types: $p"
        # cargo-check-external-types requires nightly rustdoc (uses
        # unstable -Z flags) AND pins a specific rustdoc-types schema
        # version. We pin nightly narrowly to the schema this tool
        # version expects via `rust_nightly_external_types` in
        # versions.just — bump that pin alongside any cargo-check-
        # external-types upgrade. No tolerance for schema mismatches:
        # if it fails, the pin or the tool needs to move.
        & cargo '+{{ rust_nightly_external_types }}' check-external-types --manifest-path $libPkgs[$p]
        if ($LASTEXITCODE -ne 0) { $failed = $true }
    }
    if ($failed) { exit 1 }

# Unscoped (consults external risk DB).
ox-check-aprz: (_ox-check-require "cargo-aprz")
    cargo aprz deps --error-if-high-risk --console appraisal

# === pr-test members ====================================================

# Affected tier.
[script("pwsh")]
ox-check-llvm-cov: (_ox-check-require "cargo-llvm-cov") (_ox-check-require "cargo-nextest")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-llvm-cov: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    # cargo-llvm-cov doesn't work on aarch64-pc-windows-msvc: the
    # llvm-profdata that ships with the rust toolchain there fails
    # to merge the .profraw set ("no profile can be merged"). Fall
    # back to plain `cargo nextest run` on that target so we still
    # get test execution; coverage data from this leg wouldn't have
    # been used anyway (coverage upload is gated on Linux only).
    if ($IsWindows -and ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64')) {
        Write-Host 'ox-check-llvm-cov: aarch64-pc-windows-msvc — skipping coverage; running plain nextest'
        & cargo nextest run @pkg --all-features --locked
        exit $LASTEXITCODE
    }
    # cargo llvm-cov writes the .profraw set into target/llvm-cov-target/
    # but the *report* output directory (target/coverage/) is something
    # we choose and must exist before --output-path runs.
    [System.IO.Directory]::CreateDirectory('target/coverage') | Out-Null
    [System.IO.Directory]::CreateDirectory('target/coverage/html') | Out-Null
    # Wipe stale .profraw data so the report reflects only this run.
    cargo llvm-cov clean --workspace --profraw-only
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    # Instrument and run tests; defer report generation so we can emit
    # multiple formats from the same profraw set without re-running.
    & cargo llvm-cov nextest @pkg --all-features --locked --no-report
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    # lcov.info feeds Codecov on GitHub; cobertura.xml feeds
    # PublishCodeCoverageResults@2 on Azure DevOps.
    cargo llvm-cov report --lcov      --output-path target/coverage/lcov.info
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo llvm-cov report --cobertura --output-path target/coverage/cobertura.xml
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    # Local-only HTML viewer (no CI consumer); cheap once the data exists.
    cargo llvm-cov report --html      --output-dir target/coverage/html
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier.
[script("pwsh")]
ox-check-doc-test:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-doc-test: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    & cargo test --doc @pkg --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier.
[script("pwsh")]
ox-check-examples:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-examples: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    & cargo build @pkg --examples --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === pr-mutants members =================================================

# Affected tier. cargo-mutants does its own diff-scoping via --in-diff;
# the affected-tier guard is the wiring-layer's coarse filter (if no
# affected packages exist, the entire mutants run is pointless).
[script("pwsh")]
ox-check-mutants: (_ox-check-require "cargo-mutants")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-mutants: no affected packages; skipping'
        exit 0
    }
    # Resolve BASE_REF: env override > origin/main > origin/master.
    $base = $null
    if ($env:BASE_REF) {
        $base = $env:BASE_REF
    } else {
        foreach ($candidate in @('origin/main', 'origin/master')) {
            git rev-parse --verify $candidate 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) {
                $base = $candidate
                break
            }
        }
    }
    if (-not $base) {
        Write-Error 'ox-check-mutants: no BASE_REF set and neither origin/main nor origin/master is available. Set BASE_REF to the branch to diff against.'
        exit 1
    }
    # cargo-mutants --in-diff takes a FILE path containing a unified
    # diff, not a git revision range. Write the diff to a temp file
    # first. RUNNER_TEMP (GH) and AGENT_TEMPDIRECTORY (ADO) point at
    # the job's scratch dir; fall back to the system temp dir locally.
    $tmp_dir = $env:RUNNER_TEMP
    if (-not $tmp_dir) { $tmp_dir = $env:AGENT_TEMPDIRECTORY }
    if (-not $tmp_dir) { $tmp_dir = [System.IO.Path]::GetTempPath() }
    $diff_path = Join-Path $tmp_dir 'ox-check-mutants.diff'
    git diff "$base..HEAD" --output=$diff_path
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo mutants --in-diff $diff_path --no-shuffle --jobs 0
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === nightly-runtime members ============================================

# Nightly checks always run full-workspace; impact env vars aren't set
# by the scheduled workflow, so the affected-tier default (--workspace)
# applies. Skip guards are still included for local diff-scoped runs.

[script("pwsh")]
ox-check-miri:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-miri: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    & cargo '+{{ rust_nightly }}' miri nextest run @pkg
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[script("pwsh")]
ox-check-careful: (_ox-check-require "cargo-careful")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-careful: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    & cargo '+{{ rust_nightly }}' careful test @pkg --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === nightly-exhaustive members =========================================

# Unscoped. Scheduled-exhaustive deliberately runs over the whole
# workspace regardless of diff.
ox-check-mutants-full: (_ox-check-require "cargo-mutants")
    cargo mutants --workspace --no-shuffle --jobs 0

# Required tier. cargo-hack's feature powerset cascades through dep
# features, so the required set (workspace-internal transitive deps)
# is the right scope.
[script("pwsh")]
ox-check-cargo-hack: (_ox-check-require "cargo-hack")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_REQUIRED -eq '--skip') {
        Write-Host 'ox-check-cargo-hack: no required packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_REQUIRED) { -split $env:OX_CHECK_INCLUDE_REQUIRED } else { '--workspace' })
    & cargo hack @pkg --feature-powerset --depth 2 check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier.
[script("pwsh")]
ox-check-bench:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-bench: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { '--workspace' })
    & cargo bench @pkg --all-features --no-run
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
