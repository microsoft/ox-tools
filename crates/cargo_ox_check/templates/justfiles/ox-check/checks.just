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

# === pr-fast members ====================================================

# Modified tier. cargo-fmt is a rustup component; if cargo is on PATH,
# fmt is too, so no require is needed.
ox-check-fmt:
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-fmt: no modified packages; skipping"; exit 0; \
    fi; \
    cargo fmt --all --check

# Affected tier. Per project policy, clippy runs on the affected set
# rather than the modified set: a change in a crate can introduce a
# clippy issue in a dependent crate (e.g., trait-bound or
# obviously-truthy-condition lints that key off the changed type), so
# we want downstream rev-deps to lint as well. cargo-clippy is a
# rustup component; same reasoning as fmt for the require.
ox-check-clippy:
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-clippy: no affected packages; skipping"; exit 0; \
    fi; \
    cargo clippy ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --all-targets --all-features --locked -- -D warnings

# Modified tier.
ox-check-cargo-sort: (_ox-check-require "cargo-sort")
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-cargo-sort: no modified packages; skipping"; exit 0; \
    fi; \
    cargo sort --workspace --check

# Modified tier.
ox-check-license-headers: (_ox-check-require "cargo-heather")
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-license-headers: no modified packages; skipping"; exit 0; \
    fi; \
    cargo heather --workspace

# Modified tier.
ox-check-ensure-no-cyclic-deps: (_ox-check-require "cargo-ensure-no-cyclic-deps")
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-ensure-no-cyclic-deps: no modified packages; skipping"; exit 0; \
    fi; \
    cargo ensure-no-cyclic-deps --workspace

# Modified tier.
ox-check-ensure-no-default-features: (_ox-check-require "cargo-ensure-no-default-features")
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-ensure-no-default-features: no modified packages; skipping"; exit 0; \
    fi; \
    cargo ensure-no-default-features --workspace

# Required tier. cargo doc resolves intra-doc links through the dep
# graph, so a dep changing its public API can break doc-build in a
# crate that wasn't itself modified.
ox-check-doc-build:
    @if [ "$OX_CHECK_INCLUDE_REQUIRED" = "--skip" ]; then \
        echo "ox-check-doc-build: no required packages; skipping"; exit 0; \
    fi; \
    RUSTDOCFLAGS='-D warnings' cargo doc ${OX_CHECK_INCLUDE_REQUIRED:---workspace} --all-features --no-deps

# Modified tier.
ox-check-readme-check: (_ox-check-require "cargo-doc2readme")
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-readme-check: no modified packages; skipping"; exit 0; \
    fi; \
    cargo doc2readme --check

# Modified tier.
ox-check-spellcheck: (_ox-check-require "cargo-spellcheck")
    @if [ "$OX_CHECK_INCLUDE_MODIFIED" = "--skip" ]; then \
        echo "ox-check-spellcheck: no modified packages; skipping"; exit 0; \
    fi; \
    cargo spellcheck check --code 1

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
# workspace members).
ox-check-deny: (_ox-check-require "cargo-deny")
    cargo deny check

# Unscoped (consults external advisory DB; reads Cargo.lock).
ox-check-audit: (_ox-check-require "cargo-audit")
    cargo audit

# Required tier. cargo-udeps detects unused dependencies by resolving
# the full crate graph and seeing which deps are referenced; that's
# precisely what the required tier is for. Depends on the floating
# nightly toolchain.
ox-check-udeps: (_ox-check-require "cargo-udeps")
    @if [ "$OX_CHECK_INCLUDE_REQUIRED" = "--skip" ]; then \
        echo "ox-check-udeps: no required packages; skipping"; exit 0; \
    fi; \
    cargo +nightly udeps ${OX_CHECK_INCLUDE_REQUIRED:---workspace} --all-targets --all-features

# Affected tier (compares published API of changed crates against the
# baseline; only changed crates' surface is at risk).
ox-check-semver-check: (_ox-check-require "cargo-semver-checks")
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-semver-check: no affected packages; skipping"; exit 0; \
    fi; \
    cargo semver-checks ${OX_CHECK_INCLUDE_AFFECTED:---workspace}

# Affected tier (lints public API of changed crates and rev-deps).
ox-check-external-types: (_ox-check-require "cargo-check-external-types")
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-external-types: no affected packages; skipping"; exit 0; \
    fi; \
    cargo check-external-types ${OX_CHECK_INCLUDE_AFFECTED:---workspace}

# Unscoped (consults external risk DB).
ox-check-aprz: (_ox-check-require "cargo-aprz")
    cargo aprz check

# === pr-test members ====================================================

# Affected tier.
ox-check-llvm-cov: (_ox-check-require "cargo-llvm-cov") (_ox-check-require "cargo-nextest")
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-llvm-cov: no affected packages; skipping"; exit 0; \
    fi
    # Wipe stale .profraw data so the report reflects only this run.
    cargo llvm-cov clean --workspace --profraw-only
    # Instrument and run tests; defer report generation so we can emit
    # multiple formats from the same profraw set without re-running.
    cargo llvm-cov nextest ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --all-features --locked --no-report
    # lcov.info feeds Codecov on GitHub; cobertura.xml feeds
    # PublishCodeCoverageResults@2 on Azure DevOps.
    cargo llvm-cov report --lcov      --output-path target/coverage/lcov.info
    cargo llvm-cov report --cobertura --output-path target/coverage/cobertura.xml
    # Local-only HTML viewer (no CI consumer); cheap once the data exists.
    cargo llvm-cov report --html      --output-dir target/coverage/html

# Affected tier.
ox-check-doc-test:
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-doc-test: no affected packages; skipping"; exit 0; \
    fi; \
    cargo test --doc ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --all-features --locked

# Affected tier.
ox-check-examples:
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-examples: no affected packages; skipping"; exit 0; \
    fi; \
    cargo build ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --examples --all-features --locked

# === pr-mutants members =================================================

# Affected tier. cargo-mutants does its own diff-scoping via --in-diff;
# the affected-tier guard is the wiring-layer's coarse filter (if no
# affected packages exist, the entire mutants run is pointless).
ox-check-mutants: (_ox-check-require "cargo-mutants")
    #!/usr/bin/env sh
    set -eu
    if [ "${OX_CHECK_INCLUDE_AFFECTED:-}" = "--skip" ]; then
        echo "ox-check-mutants: no affected packages; skipping"
        exit 0
    fi
    # Resolve BASE_REF: env override > origin/main > origin/master.
    if [ -n "${BASE_REF:-}" ]; then
        base="$BASE_REF"
    elif git rev-parse --verify origin/main >/dev/null 2>&1; then
        base="origin/main"
    elif git rev-parse --verify origin/master >/dev/null 2>&1; then
        base="origin/master"
    else
        echo "ox-check-mutants: no BASE_REF set and neither origin/main nor origin/master is available." >&2
        echo "Set BASE_REF to the branch to diff against." >&2
        exit 1
    fi
    cargo mutants --in-diff "$base"..HEAD --no-shuffle --jobs 0

# === nightly-runtime members ============================================

# Nightly checks always run full-workspace; impact env vars aren't set
# by the nightly workflow, so the affected-tier default (--workspace)
# applies. Skip guards are still included for local diff-scoped runs.

ox-check-miri:
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-miri: no affected packages; skipping"; exit 0; \
    fi; \
    cargo +nightly miri nextest run ${OX_CHECK_INCLUDE_AFFECTED:---workspace}

ox-check-careful: (_ox-check-require "cargo-careful")
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-careful: no affected packages; skipping"; exit 0; \
    fi; \
    cargo +nightly careful test ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --all-features --locked

# === nightly-exhaustive members =========================================

# Unscoped. Nightly-exhaustive deliberately runs over the whole
# workspace regardless of diff.
ox-check-mutants-full: (_ox-check-require "cargo-mutants")
    cargo mutants --workspace --no-shuffle --jobs 0

# Required tier. cargo-hack's feature powerset cascades through dep
# features, so the required set (workspace-internal transitive deps)
# is the right scope.
ox-check-cargo-hack: (_ox-check-require "cargo-hack")
    @if [ "$OX_CHECK_INCLUDE_REQUIRED" = "--skip" ]; then \
        echo "ox-check-cargo-hack: no required packages; skipping"; exit 0; \
    fi; \
    cargo hack ${OX_CHECK_INCLUDE_REQUIRED:---workspace} --feature-powerset --depth 2 check

# Affected tier.
ox-check-bench:
    @if [ "$OX_CHECK_INCLUDE_AFFECTED" = "--skip" ]; then \
        echo "ox-check-bench: no affected packages; skipping"; exit 0; \
    fi; \
    cargo bench ${OX_CHECK_INCLUDE_AFFECTED:---workspace} --all-features --no-run
