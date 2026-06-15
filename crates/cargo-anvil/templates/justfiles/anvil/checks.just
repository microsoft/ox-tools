# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See ../../docs/design/checks.md for the catalog and rationale.
#
# Each check belongs to one of four buckets, which determines how it
# interprets the impact env vars emitted by the cargo-delta impact step
# in cloud workflows:
#
#   - "modified": only run when at least one package's source files
#     changed in the diff. The check's underlying tool is workspace-wide
#     or directory-scoped (cargo fmt --all, cargo heather, cargo
#     spellcheck), so it doesn't take --package; we short-circuit on
#     the ANVIL_INCLUDE_MODIFIED == "--skip" sentinel.
#
#   - "affected": run on the affected set (modified ∪ reverse-deps
#     within the workspace). The check's underlying tool takes
#     --package; we splice ANVIL_INCLUDE_AFFECTED into the cargo
#     invocation, defaulting to --workspace for local invocations where
#     no env var is set.
#
#   - "required": run on the required set (affected ∪ workspace-internal
#     transitive deps). Same splice/default pattern as affected, but
#     keyed on ANVIL_INCLUDE_REQUIRED. Used for checks whose tool
#     resolves through the dep graph (cargo doc → intra-doc links;
#     cargo hack → feature powerset; cargo udeps → unused-deps).
#
#   - "unscoped": always run, no env var reference. External-input
#     checks (deny, audit, aprz) and PR-context checks (pr-title) live
#     here. Scheduled-exhaustive recipes (mutants-full) are also unscoped
#     by design.
#
# Local invocation (no impact wiring): all three env vars are unset
# (recipes use the `?? "--workspace"` null-coalescing fallback below);
# modified-tier recipes simply skip the splice and run their
# workspace-wide tool; affected/required-tier recipes splat
# "--workspace" when the env var is unset.
#
# Preparation contract: when a recipe reaches the cargo call, the
# env var is one of:
#
#   * unset                       - local run; the recipe substitutes
#                                    "--workspace" via `?? "--workspace"`
#   * "--package A --package B"   - emitted by the cloud-workflow impact step when
#                                    the tier has members
#   * "--skip"                    - emitted by the cloud-workflow impact step when
#                                    the tier is empty (recipe exits 0)
#
# This lets the simple recipes splat the var directly with
#   & cargo X @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace")) ...
# and reduces the per-recipe boilerplate to a single one-line skip
# guard plus the cargo invocation.
#
# Modified-tier recipes never splice the env var into cargo (their
# tools are workspace-wide); they only check the skip sentinel.
#
# Every recipe whose body uses multi-line conditionals or env-var
# splicing is annotated with [script("pwsh")]. pwsh is preinstalled on
# Windows (since Windows 10), on GH/ADO hosted Linux + Windows
# runners, and installable on macOS via Homebrew or the upstream
# installer. We chose pwsh over bash because just's shebang dispatch
# requires `cygpath` on Windows (only on PATH from inside Git Bash),
# while [script("pwsh")] works from plain PowerShell with no PATH
# augmentation. The `??` null-coalescing operator used in the splat
# requires pwsh 7+, which is the floor we already require via
# _anvil-require pwsh.
#
# Single-command recipes (cargo deny check, cargo audit) are plain
# just recipes that inherit the adopter's default shell — they're
# shell-agnostic.
#
# Note: [script(...)] requires `set unstable`. The adopter's root
# justfile must declare it (typically as a top-level line). anvil's
# mod.just does NOT redeclare it, to avoid conflicting with adopters
# who already have it.

# === pr-fast members ====================================================

# Modified tier. cargo-fmt is a rustup component. We invoke it via the
# pinned nightly (see versions.just) because rustfmt.toml uses
# unstable_features = true (imports_granularity, group_imports,
# format_code_in_doc_comments). Floating nightly would mean
# format-drift breaking cloud workflows on rustup updates — the same trap we
# explicitly avoid for udeps/miri/careful/external-types.
[script("pwsh")]
anvil-fmt: anvil-fmt-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') { exit 0 }
    cargo '+{{ rust_nightly }}' fmt --all --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier. Per project policy, clippy runs on the affected set
# rather than the modified set: a change in a crate can introduce a
# clippy issue in a dependent crate (e.g., trait-bound or
# obviously-truthy-condition lints that key off the changed type), so
# we want downstream rev-deps to lint as well. cargo-clippy is a
# rustup component; same reasoning as fmt for the require.
[script("pwsh")]
anvil-clippy: anvil-clippy-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
    & cargo clippy @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace")) --all-targets --all-features --locked "--" '-D' 'warnings'
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
anvil-cargo-sort: anvil-cargo-sort-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') { exit 0 }
    cargo sort --workspace --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
anvil-license-headers: anvil-license-headers-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') { exit 0 }
    cargo heather
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
anvil-ensure-no-cyclic-deps: anvil-ensure-no-cyclic-deps-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') { exit 0 }
    cargo ensure-no-cyclic-deps
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
[script("pwsh")]
anvil-ensure-no-default-features: anvil-ensure-no-default-features-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') { exit 0 }
    cargo ensure-no-default-features
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Required tier. cargo doc resolves intra-doc links through the dep
# graph, so a dep changing its public API can break doc-build in a
# crate that wasn't itself modified.
[script("pwsh")]
anvil-doc-build: anvil-doc-build-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_REQUIRED -eq '--skip') { exit 0 }
    $env:RUSTDOCFLAGS = '-D warnings'
    & cargo doc @(-split ($env:ANVIL_INCLUDE_REQUIRED ?? "--workspace")) --all-features --no-deps
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Modified tier.
#
# cargo-doc2readme regenerates a crate's README.md from its rustdoc.
# Two extension points users frequently need:
#   * Workspace-level template (`crates/README.j2` or `README.j2` at repo
#     root): a Tera template applied to every crate's README. anvil
#     auto-detects it and passes `--template` to the per-crate runs.
#   * Per-crate opt-out: hand-crafted READMEs (e.g. a tool crate whose
#     README is more freeform than the lib docs) opt out by adding
#     `[package.metadata.ox-gen-readme]\ndisable = true` to their
#     Cargo.toml. anvil skips those crates.
#
# Bin-only crates have no library rustdoc to base a README on, so they
# are skipped as well (cargo doc2readme requires a library target).
# TODO(anvil-runner): even after a `cargo ox-run` helper absorbs the
# skip/splat preamble, this recipe stays multi-step: per-crate
# iteration over library targets, cargo-metadata-driven opt-outs
# (publish=false, [package.metadata.ox-gen-readme] disable), per-crate
# Push-Location into the crate dir (cargo-doc2readme is CWD-sensitive
# rather than --manifest-path-driven), and per-crate template-path
# resolution. The ANVIL_INCLUDE_MODIFIED value would still need to
# be intersected with the lib-crate set rather than splatted into cargo.
[script("pwsh")]
anvil-readme-check: anvil-readme-check-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'anvil-readme-check: no modified packages; skipping'
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
    $pkg = @(if ($env:ANVIL_INCLUDE_MODIFIED) { -split $env:ANVIL_INCLUDE_MODIFIED } else { '--workspace' })
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
            Write-Host "anvil-readme-check: $name (skipped: publish = false)"
            continue
        }
        $disabled = $false
        if ($p.metadata -and $p.metadata.'ox-gen-readme' -and $p.metadata.'ox-gen-readme'.disable) {
            $disabled = $true
        }
        if ($disabled) {
            Write-Host "anvil-readme-check: $name (opted out via [package.metadata.ox-gen-readme])"
            continue
        }
        Write-Host "anvil-readme-check: $name"
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
anvil-spellcheck: anvil-spellcheck-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_MODIFIED -eq '--skip') {
        Write-Host 'anvil-spellcheck: no modified packages; skipping'
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
#
# Validates that $env:PR_TITLE matches Conventional Commits when set;
# no-op when unset. Cloud workflows inject PR_TITLE explicitly (GH Actions:
# ${{ github.event.pull_request.title }}; ADO: $(System.PullRequest.Title))
# so the check has authoritative input there. Locally it skips silently --
# there is no reliable way to recover the PR title for the ADO backend
# (no equivalent of `gh pr view`), so the recipe stays simple and
# defers the check to cloud workflows.
[script("pwsh")]
anvil-pr-title: anvil-pr-title-validate-prereqs
    $title = $env:PR_TITLE
    if (-not $title) {
        Write-Host 'anvil-pr-title: PR_TITLE env var not set; skipping (check runs in cloud workflows)'
        exit 0
    }
    if ($title -notmatch '^(feat|fix|chore|docs|refactor|test|build|cloud workflows|perf|revert)(\([^)]+\))?!?: .+') {
        Write-Error "PR title '$title' does not match Conventional Commits"
        exit 1
    }

# Unscoped (consults external advisory DB; reads Cargo.lock, not
# workspace members). Single command — inherits adopter's default shell.
anvil-deny: anvil-deny-validate-prereqs
    cargo deny check

# Unscoped (consults external advisory DB; reads Cargo.lock).
anvil-audit: anvil-audit-validate-prereqs
    cargo audit

# Required tier. cargo-udeps detects unused dependencies by resolving
# the full crate graph and seeing which deps are referenced; that's
# precisely what the required tier is for. Pinned to the general
# nightly defined in versions.just.
#
# Deliberately omits `--all-targets`: with `--all-targets`, a dep
# that's listed in BOTH `[dependencies]` and `[dev-dependencies]` and
# used only by tests is reported as "all used" because the dev-deps
# target satisfies the lookup, masking the unused entry in main
# `[dependencies]`. Restricting to the default targets (lib + bins)
# matches main repo cloud workflows' check and surfaces the real bug.
[script("pwsh")]
anvil-udeps: anvil-udeps-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_REQUIRED -eq '--skip') { exit 0 }
    & cargo '+{{ rust_nightly }}' udeps @(-split ($env:ANVIL_INCLUDE_REQUIRED ?? "--workspace")) --all-features
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
#
# Findings policy: this recipe is *advisory*. Real SemVer findings do
# NOT fail the recipe -- breaking changes between unreleased commits
# are normal (the major-version bump happens at release time, not on
# every PR). Instead, when there are findings we write a markdown
# advisory body to `target/anvil/comments/semver.md`; when the
# tree is clean we remove that file. cloud-workflow wiring (GH:
# marocchino/sticky-pull-request-comment; ADO: pwsh + REST API)
# inspects the file after the recipe and upserts / clears a sticky
# PR comment accordingly. Local invocation gets the same file
# written under target/ for inspection.
#
# TODO(anvil-runner): even after a `cargo ox-run` helper absorbs the
# skip/splat preamble, this recipe stays multi-step: cargo-metadata
# filter to library crates (cargo-semver-checks --workspace fails on
# bin-only workspaces), intersect ANVIL_INCLUDE_AFFECTED with that
# set, then per-crate invocation with selective error tolerance for
# unpublished crates ("not found in registry") and bin->bin+lib
# transitions ("no library targets found").
[script("pwsh")]
anvil-semver-check: anvil-semver-check-validate-prereqs
    $ErrorActionPreference = 'Stop'
    $commentFile = 'target/anvil/comments/semver.md'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'anvil-semver-check: no affected packages; skipping'
        Remove-Item -LiteralPath $commentFile -ErrorAction SilentlyContinue
        exit 0
    }
    $pkg = @(if ($env:ANVIL_INCLUDE_AFFECTED) { -split $env:ANVIL_INCLUDE_AFFECTED } else { '--workspace' })
    # Build the candidate package list. Always iterate per-package over
    # library crates only -- cargo-semver-checks --workspace would fail
    # on workspaces that contain bin-only crates ("no library targets
    # found"), and we want the same tolerance for unpublished / bin->lib-
    # transition crates regardless of whether we got here via impact-
    # scoping (cloud workflows) or full-workspace fallback (local).
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
        Write-Host 'anvil-semver-check: no affected library crates; skipping'
        Remove-Item -LiteralPath $commentFile -ErrorAction SilentlyContinue
        exit 0
    }
    $findings = New-Object System.Collections.Generic.List[string]
    foreach ($p in $packages) {
        Write-Host "anvil-semver-check: $p"
        $output = (& cargo semver-checks --package $p 2>&1) | Out-String
        if ($LASTEXITCODE -ne 0) {
            if ($output -match 'not found in registry|no library targets found') {
                Write-Host "  $p has no comparable baseline; skipping (likely unpublished or bin->lib transition)" -ForegroundColor Yellow
            } else {
                Write-Host $output
                # Append a per-crate findings block. Using one-line-at-a-time
                # appends keeps the markdown free of pwsh backtick-escape
                # gymnastics (single-quoted literals + the natural `n join
                # produce clean LF newlines and unambiguous triple-backticks).
                $findings.Add('### `' + $p + '`') | Out-Null
                $findings.Add('') | Out-Null
                $findings.Add('```') | Out-Null
                foreach ($line in ($output.TrimEnd() -split "`r?`n")) {
                    $findings.Add($line.TrimEnd()) | Out-Null
                }
                $findings.Add('```') | Out-Null
                $findings.Add('') | Out-Null
            }
        }
    }
    [System.IO.Directory]::CreateDirectory((Split-Path -Parent $commentFile)) | Out-Null
    if ($findings.Count -gt 0) {
        # Body starts with an HTML-comment marker so the ADO wiring can
        # locate the existing thread on subsequent runs (ADO has no
        # native "sticky comment header"; the marker is invisible to
        # human readers). Marocchino on GH uses its own `header:` input
        # and ignores the marker, but having it in the body keeps a
        # single source of truth across backends.
        $lines = New-Object System.Collections.Generic.List[string]
        $lines.Add('<!-- anvil-semver -->') | Out-Null
        $lines.Add('## :warning: Potential breaking changes detected') | Out-Null
        $lines.Add('') | Out-Null
        $lines.Add('`cargo semver-checks` flagged the following on this PR. This is **informational** -- breaking changes between commits are expected; the major-version bump happens at release time, not on every PR.') | Out-Null
        $lines.Add('') | Out-Null
        foreach ($f in $findings) { $lines.Add($f) | Out-Null }
        $body = ($lines -join "`n") + "`n"
        Set-Content -LiteralPath $commentFile -Value $body -Encoding UTF8 -NoNewline
        Write-Host ''
        Write-Host "anvil-semver-check: wrote advisory comment to $commentFile (recipe exits 0; cloud workflows will upsert a PR comment)" -ForegroundColor Yellow
    } else {
        Remove-Item -LiteralPath $commentFile -ErrorAction SilentlyContinue
    }
    # Advisory: always exit 0. cloud-workflow wiring posts/clears the PR comment.
    exit 0

# Affected tier (lints public API of changed crates and rev-deps).
#
# cargo-check-external-types is per-manifest: no --package/--workspace,
# only --manifest-path. Iterate the affected library crates and run
# the tool once each, pointing at the crate's Cargo.toml. Bin-only
# crates have no public API surface and are skipped.
# TODO(anvil-runner): even after a `cargo ox-run` helper absorbs the
# skip/splat preamble, this recipe stays multi-step: cargo-check-
# external-types is per-manifest (no --package/--workspace), so we
# build a name->manifest map from cargo metadata, filter to lib crates,
# intersect with ANVIL_INCLUDE_AFFECTED, and call the tool once per
# crate. Hard-fails on errors (no tolerance, unlike semver-check).
[script("pwsh")]
anvil-external-types: anvil-external-types-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'anvil-external-types: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:ANVIL_INCLUDE_AFFECTED) { -split $env:ANVIL_INCLUDE_AFFECTED } else { '--workspace' })
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
        Write-Host 'anvil-external-types: no affected library crates; skipping'
        exit 0
    }
    $failed = $false
    foreach ($p in $packages) {
        Write-Host "anvil-external-types: $p"
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
anvil-aprz: anvil-aprz-validate-prereqs
    cargo aprz deps --error-if-high-risk --console appraisal

# === pr-test members ====================================================

# Affected tier.
[script("pwsh")]
anvil-llvm-cov: anvil-llvm-cov-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'anvil-llvm-cov: no affected packages; skipping'
        exit 0
    }
    $pkg = @(if ($env:ANVIL_INCLUDE_AFFECTED) { -split $env:ANVIL_INCLUDE_AFFECTED } else { '--workspace' })
    # cargo-llvm-cov doesn't work on aarch64-pc-windows-msvc: the
    # llvm-profdata that ships with the rust toolchain there fails
    # to merge the .profraw set ("no profile can be merged"). Fall
    # back to plain `cargo nextest run` on that target so we still
    # get test execution; coverage data from this leg wouldn't have
    # been used anyway (coverage upload is gated on Linux only).
    if ($IsWindows -and ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64')) {
        Write-Host 'anvil-llvm-cov: aarch64-pc-windows-msvc -- skipping coverage; running plain nextest'
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
    # Note: nextest's exit-4 ("no tests to run") IS treated as a
    # failure here -- a llvm-cov run that finds no tests almost
    # always means a config mistake (wrong package filter, missing
    # test target, etc.), not a legitimate empty set. anvil-miri
    # is the exception (see its comment): miri-skipped tests are an
    # expected design point for FS-heavy crates.
    & cargo llvm-cov nextest @pkg --all-features --locked --no-report
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    # lcov.info feeds Codecov on GitHub; cobertura.xml feeds
    # PublishCodeCoverageResults@2 on Azure DevOps.
    cargo llvm-cov report --lcov      --output-path target/coverage/lcov.info
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo llvm-cov report --cobertura --output-path target/coverage/cobertura.xml
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    # Local-only HTML viewer (no cloud-workflow consumer); cheap once the data exists.
    cargo llvm-cov report --html      --output-dir target/coverage/html
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier.
[script("pwsh")]
anvil-doc-test: anvil-doc-test-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
    & cargo test --doc @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace")) --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier.
[script("pwsh")]
anvil-examples: anvil-examples-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
    & cargo build @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace")) --examples --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === pr-mutants member ====================================================

# Affected tier. cargo-mutants does its own diff-scoping via --in-diff;
# the affected-tier guard is the wiring-layer's coarse filter (if no
# affected packages exist, the entire mutants run is pointless).
#
# Skip on aarch64-pc-windows-msvc: cargo-mutants doesn't build there
# (upstream winapi incompatibility), so `_anvil-require cargo-mutants`
# would fail. The merged pr-slow group runs on all four OS legs; mutants
# is the only sub-recipe that can't follow, so it bails out early on the
# affected leg. Coverage on the other three legs is unchanged.
[script("pwsh")]
anvil-mutants-diff: anvil-mutants-diff-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($IsWindows -and ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64' -or $env:PROCESSOR_ARCHITEW6432 -eq 'ARM64')) {
        Write-Host 'anvil-mutants-diff: anvil-mutants-diff-validate-prereqs aarch64-pc-windows-msvc -- cargo-mutants does not build here (winapi); skipping'
        exit 0
    }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') {
        Write-Host 'anvil-mutants-diff: anvil-mutants-diff-validate-prereqs no affected packages; skipping'
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
        Write-Error 'anvil-mutants-diff: anvil-mutants-diff-validate-prereqs no BASE_REF set and neither origin/main nor origin/master is available. Set BASE_REF to the branch to diff against.'
        exit 1
    }
    # cargo-mutants --in-diff takes a FILE path containing a unified
    # diff, not a git revision range. Write the diff to a temp file
    # first. RUNNER_TEMP (GH) and AGENT_TEMPDIRECTORY (ADO) point at
    # the job's scratch dir; fall back to the system temp dir locally.
    $tmp_dir = $env:RUNNER_TEMP
    if (-not $tmp_dir) { $tmp_dir = $env:AGENT_TEMPDIRECTORY }
    if (-not $tmp_dir) { $tmp_dir = [System.IO.Path]::GetTempPath() }
    $diff_path = Join-Path $tmp_dir 'anvil-mutants-diff.diff'
    git diff "$base..HEAD" --output=$diff_path
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo mutants --in-diff $diff_path --no-shuffle --jobs 0
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === nightly-runtime members ============================================

# Nightly checks always run full-workspace; impact env vars aren't set
# by the scheduled workflow, so the affected-tier default (--workspace)
# applies. Skip guards are still included for local diff-scoped runs.

[script("pwsh")]
anvil-miri: anvil-miri-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
    # `--no-tests=pass`: miri-only exception. Tests that touch the
    # filesystem, spawn subprocesses, or use other miri-incompatible
    # APIs commonly carry `#[cfg_attr(miri, ignore)]` (this is the
    # canonical opt-out for build-tooling / CLI crates). A crate
    # whose test set ends up entirely-skipped under miri legitimately
    # produces zero runnable tests; nextest's default exit-4 ("no
    # tests to run") would fail the recipe in that case. We treat
    # empty test runs as success for miri only. Other nextest-using
    # recipes (llvm-cov) keep exit-4 as a failure because zero tests
    # there almost always indicates a config mistake.
    & cargo '+{{ rust_nightly }}' miri nextest run --no-tests=pass @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace"))
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

[script("pwsh")]
anvil-careful: anvil-careful-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
    & cargo '+{{ rust_nightly }}' careful test @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace")) --all-features --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# === nightly-exhaustive members =========================================

# Unscoped. Scheduled-exhaustive deliberately runs over the whole
# workspace regardless of diff.
anvil-mutants-full: anvil-mutants-full-validate-prereqs
    cargo mutants --workspace --no-shuffle --jobs 0

# Required tier. cargo-hack's feature powerset cascades through dep
# features, so the required set (workspace-internal transitive deps)
# is the right scope.
[script("pwsh")]
anvil-cargo-hack: anvil-cargo-hack-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_REQUIRED -eq '--skip') { exit 0 }
    & cargo hack @(-split ($env:ANVIL_INCLUDE_REQUIRED ?? "--workspace")) --feature-powerset --depth 2 check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Affected tier.
[script("pwsh")]
anvil-bench: anvil-bench-validate-prereqs
    $ErrorActionPreference = 'Stop'
    if ($env:ANVIL_INCLUDE_AFFECTED -eq '--skip') { exit 0 }
    & cargo bench @(-split ($env:ANVIL_INCLUDE_AFFECTED ?? "--workspace")) --all-features --no-run
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# ============================================================================
# Per-check setup + validate-prereqs
# ============================================================================
#
# Each check has matching `*-setup` and `*-validate-prereqs` recipes
# that install / verify the tools and components it needs. The setup
# recipes accept an `installer=install|install` parameter that
# forwards to the underlying tool-install recipes; the validate-prereqs
# recipes take no parameters.
#
# These are the building blocks for `anvil-<group>-setup`
# (groups.just) and `anvil-<tier>-setup` (tiers.just) ΓÇö each
# group/tier-level recipe is just a fan-out over the per-check
# setup/validate-prereqs of its members.

# --- pr-fast members ---

[group("anvil-setup")]
anvil-fmt-setup installer="install": anvil-component-nightly-rustfmt-install

[group("anvil-setup")]
anvil-fmt-validate-prereqs: anvil-component-nightly-rustfmt-validate-prereqs

[group("anvil-setup")]
anvil-clippy-setup installer="install": anvil-component-default-clippy-install

[group("anvil-setup")]
anvil-clippy-validate-prereqs: anvil-component-default-clippy-validate-prereqs

[group("anvil-setup")]
anvil-cargo-sort-setup installer="install": (anvil-tool-cargo-sort-install installer)

[group("anvil-setup")]
anvil-cargo-sort-validate-prereqs: anvil-tool-cargo-sort-validate-prereqs

[group("anvil-setup")]
anvil-license-headers-setup installer="install": (anvil-tool-cargo-heather-install installer)

[group("anvil-setup")]
anvil-license-headers-validate-prereqs: anvil-tool-cargo-heather-validate-prereqs

[group("anvil-setup")]
anvil-ensure-no-cyclic-deps-setup installer="install": (anvil-tool-cargo-ensure-no-cyclic-deps-install installer)

[group("anvil-setup")]
anvil-ensure-no-cyclic-deps-validate-prereqs: anvil-tool-cargo-ensure-no-cyclic-deps-validate-prereqs

[group("anvil-setup")]
anvil-ensure-no-default-features-setup installer="install": (anvil-tool-cargo-ensure-no-default-features-install installer)

[group("anvil-setup")]
anvil-ensure-no-default-features-validate-prereqs: anvil-tool-cargo-ensure-no-default-features-validate-prereqs

# doc-build, examples and doc-test are pure cargo built-ins; the rust
# toolchain (rustc + cargo) is the only prerequisite, and we already
# rely on it being present everywhere anvil runs.
[group("anvil-setup")]
anvil-doc-build-setup installer="install": anvil-tool-rustc-validate-prereqs

[group("anvil-setup")]
anvil-doc-build-validate-prereqs: anvil-tool-rustc-validate-prereqs

[group("anvil-setup")]
anvil-readme-check-setup installer="install": (anvil-tool-cargo-doc2readme-install installer)

[group("anvil-setup")]
anvil-readme-check-validate-prereqs: anvil-tool-cargo-doc2readme-validate-prereqs

# cargo-spellcheck has a build-time libclang dependency; the system
# deps check runs first so adopters get a clear hint instead of a
# cryptic clang-sys build error mid-install.
[group("anvil-setup")]
anvil-spellcheck-setup installer="install": anvil-system-deps-check (anvil-tool-cargo-spellcheck-install installer)

[group("anvil-setup")]
anvil-spellcheck-validate-prereqs: anvil-tool-cargo-spellcheck-validate-prereqs

# pr-title is a pwsh script; no cargo tool to install.
[group("anvil-setup")]
anvil-pr-title-setup installer="install": anvil-tool-pwsh-validate-prereqs

[group("anvil-setup")]
anvil-pr-title-validate-prereqs: anvil-tool-pwsh-validate-prereqs

[group("anvil-setup")]
anvil-deny-setup installer="install": (anvil-tool-cargo-deny-install installer)

[group("anvil-setup")]
anvil-deny-validate-prereqs: anvil-tool-cargo-deny-validate-prereqs

[group("anvil-setup")]
anvil-audit-setup installer="install": (anvil-tool-cargo-audit-install installer)

[group("anvil-setup")]
anvil-audit-validate-prereqs: anvil-tool-cargo-audit-validate-prereqs

[group("anvil-setup")]
anvil-udeps-setup installer="install": anvil-toolchain-nightly-install (anvil-tool-cargo-udeps-install installer)

[group("anvil-setup")]
anvil-udeps-validate-prereqs: anvil-toolchain-nightly-validate-prereqs anvil-tool-cargo-udeps-validate-prereqs

[group("anvil-setup")]
anvil-semver-check-setup installer="install": (anvil-tool-cargo-semver-checks-install installer)

[group("anvil-setup")]
anvil-semver-check-validate-prereqs: anvil-tool-cargo-semver-checks-validate-prereqs

[group("anvil-setup")]
anvil-external-types-setup installer="install": anvil-toolchain-nightly-external-types-install (anvil-tool-cargo-check-external-types-install installer)

[group("anvil-setup")]
anvil-external-types-validate-prereqs: anvil-toolchain-nightly-external-types-validate-prereqs anvil-tool-cargo-check-external-types-validate-prereqs

[group("anvil-setup")]
anvil-aprz-setup installer="install": (anvil-tool-cargo-aprz-install installer)

[group("anvil-setup")]
anvil-aprz-validate-prereqs: anvil-tool-cargo-aprz-validate-prereqs

# --- pr-test members (shared with scheduled-test) ---

[group("anvil-setup")]
anvil-llvm-cov-setup installer="install": (anvil-tool-cargo-llvm-cov-install installer) (anvil-tool-cargo-nextest-install installer)

[group("anvil-setup")]
anvil-llvm-cov-validate-prereqs: anvil-tool-cargo-llvm-cov-validate-prereqs anvil-tool-cargo-nextest-validate-prereqs

[group("anvil-setup")]
anvil-doc-test-setup installer="install": anvil-tool-rustc-validate-prereqs

[group("anvil-setup")]
anvil-doc-test-validate-prereqs: anvil-tool-rustc-validate-prereqs

[group("anvil-setup")]
anvil-examples-setup installer="install": anvil-tool-rustc-validate-prereqs

[group("anvil-setup")]
anvil-examples-validate-prereqs: anvil-tool-rustc-validate-prereqs

# --- pr-runtime-analysis members ---

[group("anvil-setup")]
anvil-miri-setup installer="install": anvil-component-nightly-miri-install anvil-component-nightly-rust-src-install (anvil-tool-cargo-nextest-install installer)

[group("anvil-setup")]
anvil-miri-validate-prereqs: anvil-component-nightly-miri-validate-prereqs anvil-component-nightly-rust-src-validate-prereqs anvil-tool-cargo-nextest-validate-prereqs

[group("anvil-setup")]
anvil-careful-setup installer="install": anvil-component-nightly-rust-src-install (anvil-tool-cargo-careful-install installer)

[group("anvil-setup")]
anvil-careful-validate-prereqs: anvil-component-nightly-rust-src-validate-prereqs anvil-tool-cargo-careful-validate-prereqs

# --- pr-mutants members ---

[group("anvil-setup")]
anvil-mutants-diff-setup installer="install": (anvil-tool-cargo-mutants-install installer)

[group("anvil-setup")]
anvil-mutants-diff-validate-prereqs: anvil-tool-cargo-mutants-validate-prereqs

# --- scheduled-exhaustive members (mutants-full reuses cargo-mutants;
#     cargo-hack and bench are dedicated tools) ---

[group("anvil-setup")]
anvil-mutants-full-setup installer="install": (anvil-tool-cargo-mutants-install installer)

[group("anvil-setup")]
anvil-mutants-full-validate-prereqs: anvil-tool-cargo-mutants-validate-prereqs

[group("anvil-setup")]
anvil-cargo-hack-setup installer="install": (anvil-tool-cargo-hack-install installer)

[group("anvil-setup")]
anvil-cargo-hack-validate-prereqs: anvil-tool-cargo-hack-validate-prereqs

# bench uses cargo-built-ins (cargo bench --no-run + plain bench runs);
# no extra tool install needed beyond the rust toolchain.
[group("anvil-setup")]
anvil-bench-setup installer="install": anvil-tool-rustc-validate-prereqs

[group("anvil-setup")]
anvil-bench-validate-prereqs: anvil-tool-rustc-validate-prereqs