# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/checks.md for the catalog and rationale.

set unstable

# === pr-fast members ====================================================

ox-check-fmt: ox-check-tools-check
    cargo fmt --all --check

ox-check-clippy: ox-check-tools-check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

ox-check-cargo-sort: (_ox-check-require "cargo-sort")
    cargo sort --workspace --check

ox-check-license-headers: (_ox-check-require "cargo-heather")
    cargo heather --workspace

ox-check-ensure-no-cyclic-deps: (_ox-check-require "cargo-ensure-no-cyclic-deps")
    cargo ensure-no-cyclic-deps --workspace

ox-check-ensure-no-default-features: (_ox-check-require "cargo-ensure-no-default-features")
    cargo ensure-no-default-features --workspace

ox-check-doc-build:
    RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps

ox-check-readme-check: (_ox-check-require "cargo-doc2readme")
    cargo doc2readme --check

ox-check-spellcheck: (_ox-check-require "cargo-spellcheck")
    cargo spellcheck check --code 1

[script("pwsh")]
ox-check-pr-title:
    $title = $env:PR_TITLE
    if (-not $title) { $title = git log -1 --pretty=%s HEAD }
    if ($title -notmatch '^(feat|fix|chore|docs|refactor|test|build|ci|perf|revert)(\([^)]+\))?!?: .+') {
        Write-Error "PR title '$title' does not match Conventional Commits"
        exit 1
    }

ox-check-deny: (_ox-check-require "cargo-deny")
    cargo deny check

ox-check-audit: (_ox-check-require "cargo-audit")
    cargo audit

ox-check-udeps: (_ox-check-require "cargo-udeps")
    cargo +nightly udeps --workspace --all-targets --all-features

ox-check-semver-check: (_ox-check-require "cargo-semver-checks")
    cargo semver-checks --workspace

ox-check-external-types: (_ox-check-require "cargo-check-external-types")
    cargo check-external-types --workspace

ox-check-aprz: (_ox-check-require "cargo-aprz")
    cargo aprz check

# === pr-test members ====================================================

ox-check-llvm-cov: (_ox-check-require "cargo-llvm-cov") (_ox-check-require "cargo-nextest")
    # Wipe stale .profraw data so the report reflects only this run.
    cargo llvm-cov clean --workspace --profraw-only
    # Instrument and run tests; defer report generation so we can emit
    # multiple formats from the same profraw set without re-running the
    # tests.
    cargo llvm-cov nextest --workspace --all-features --locked --no-report
    # lcov.info feeds Codecov on GitHub; cobertura.xml feeds
    # PublishCodeCoverageResults@2 on Azure DevOps.
    cargo llvm-cov report --lcov      --output-path target/coverage/lcov.info
    cargo llvm-cov report --cobertura --output-path target/coverage/cobertura.xml

ox-check-doc-test:
    cargo test --doc --workspace --all-features --locked

ox-check-examples:
    cargo build --workspace --examples --all-features --locked

# === pr-mutants members =================================================

ox-check-mutants: (_ox-check-require "cargo-mutants")
    #!/usr/bin/env sh
    set -eu
    base="${BASE_REF:-origin/main}"
    cargo mutants --in-diff "$base"..HEAD --no-shuffle --jobs 0

# === nightly-runtime members ============================================

ox-check-miri:
    cargo +nightly miri nextest run --workspace

ox-check-careful: (_ox-check-require "cargo-careful")
    cargo +nightly careful test --workspace --all-features --locked

# === nightly-exhaustive members =========================================

ox-check-mutants-full: (_ox-check-require "cargo-mutants")
    cargo mutants --workspace --no-shuffle --jobs 0

ox-check-cargo-hack: (_ox-check-require "cargo-hack")
    cargo hack --workspace --feature-powerset --depth 2 check

ox-check-bench:
    cargo bench --workspace --all-features --no-run
