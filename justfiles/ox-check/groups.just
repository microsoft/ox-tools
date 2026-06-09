# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/checks.md for the catalog and rationale.

# Each group is one CI job. Within a group, checks run sequentially.

# === PR groups ==========================================================

[group("ox-check")]
ox-check-pr-fast: \
    ox-check-fmt \
    ox-check-clippy \
    ox-check-cargo-sort \
    ox-check-license-headers \
    ox-check-ensure-no-cyclic-deps \
    ox-check-ensure-no-default-features \
    ox-check-doc-build \
    ox-check-readme-check \
    ox-check-spellcheck \
    ox-check-pr-title \
    ox-check-deny \
    ox-check-audit \
    ox-check-udeps \
    ox-check-semver-check \
    ox-check-aprz

[group("ox-check")]
ox-check-pr-test: \
    ox-check-llvm-cov \
    ox-check-doc-test \
    ox-check-examples

[group("ox-check")]
ox-check-pr-mutants: ox-check-mutants

# === Scheduled groups =====================================================

[group("ox-check")]
ox-check-scheduled-test: \
    ox-check-llvm-cov \
    ox-check-doc-test \
    ox-check-examples

[group("ox-check")]
ox-check-scheduled-advisories: \
    ox-check-deny \
    ox-check-audit \
    ox-check-aprz \
    ox-check-clippy \
    ox-check-external-types

[group("ox-check")]
ox-check-scheduled-runtime: \
    ox-check-miri \
    ox-check-careful

[group("ox-check")]
ox-check-scheduled-exhaustive: \
    ox-check-mutants-full \
    ox-check-cargo-hack \
    ox-check-bench
