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
#     here. Nightly-exhaustive recipes (mutants-full) are also unscoped
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

# Modified tier. cargo-fmt is a rustup component; if cargo is on PATH,
# fmt is too, so no require is needed.
[script("pwsh")]
ox-check-fmt:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-fmt: no modified packages; skipping'
        exit 0
    }
    cargo fmt --all --check
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
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
    & cargo clippy @pkg --all-targets --all-features --locked -- '-D' 'warnings'
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
    cargo heather --workspace
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-ensure-no-cyclic-deps: (_ox-check-require "cargo-ensure-no-cyclic-deps")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-ensure-no-cyclic-deps: no modified packages; skipping'
        exit 0
    }
    cargo ensure-no-cyclic-deps --workspace
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-ensure-no-default-features: (_ox-check-require "cargo-ensure-no-default-features")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-ensure-no-default-features: no modified packages; skipping'
        exit 0
    }
    cargo ensure-no-default-features --workspace
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
    $pkg = if ($env:OX_CHECK_INCLUDE_REQUIRED) { -split $env:OX_CHECK_INCLUDE_REQUIRED } else { @('--workspace') }
    $env:RUSTDOCFLAGS = '-D warnings'
    & cargo doc @pkg --all-features --no-deps
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-readme-check: (_ox-check-require "cargo-doc2readme")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-readme-check: no modified packages; skipping'
        exit 0
    }
    cargo doc2readme --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
ox-check-spellcheck: (_ox-check-require "cargo-spellcheck")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'ox-check-spellcheck: no modified packages; skipping'
        exit 0
    }
    cargo spellcheck check --code 1
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
# precisely what the required tier is for. Depends on the floating
# nightly toolchain.
[script("pwsh")]
ox-check-udeps: (_ox-check-require "cargo-udeps")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_REQUIRED -eq '--skip') {
        Write-Host 'ox-check-udeps: no required packages; skipping'
        exit 0
    }
    $pkg = if ($env:OX_CHECK_INCLUDE_REQUIRED) { -split $env:OX_CHECK_INCLUDE_REQUIRED } else { @('--workspace') }
    & cargo '+nightly' udeps @pkg --all-targets --all-features
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier (compares published API of changed crates against the
# baseline; only changed crates' surface is at risk).
[script("pwsh")]
ox-check-semver-check: (_ox-check-require "cargo-semver-checks")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-semver-check: no affected packages; skipping'
        exit 0
    }
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
    & cargo semver-checks @pkg
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier (lints public API of changed crates and rev-deps).
[script("pwsh")]
ox-check-external-types: (_ox-check-require "cargo-check-external-types")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-external-types: no affected packages; skipping'
        exit 0
    }
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
    & cargo check-external-types @pkg
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Unscoped (consults external risk DB).
ox-check-aprz: (_ox-check-require "cargo-aprz")
    cargo aprz check

# === pr-test members ====================================================

# Affected tier.
[script("pwsh")]
ox-check-llvm-cov: (_ox-check-require "cargo-llvm-cov") (_ox-check-require "cargo-nextest")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-llvm-cov: no affected packages; skipping'
        exit 0
    }
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
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
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
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
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
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
# by the nightly workflow, so the affected-tier default (--workspace)
# applies. Skip guards are still included for local diff-scoped runs.

[script("pwsh")]
ox-check-miri:
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-miri: no affected packages; skipping'
        exit 0
    }
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
    & cargo '+nightly' miri nextest run @pkg
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[script("pwsh")]
ox-check-careful: (_ox-check-require "cargo-careful")
    $ErrorActionPreference = 'Stop'
    if ($env:OX_CHECK_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'ox-check-careful: no affected packages; skipping'
        exit 0
    }
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
    & cargo '+nightly' careful test @pkg --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === nightly-exhaustive members =========================================

# Unscoped. Nightly-exhaustive deliberately runs over the whole
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
    $pkg = if ($env:OX_CHECK_INCLUDE_REQUIRED) { -split $env:OX_CHECK_INCLUDE_REQUIRED } else { @('--workspace') }
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
    $pkg = if ($env:OX_CHECK_INCLUDE_AFFECTED) { -split $env:OX_CHECK_INCLUDE_AFFECTED } else { @('--workspace') }
    & cargo bench @pkg --all-features --no-run
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
