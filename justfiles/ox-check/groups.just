# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check-update; edit via `cargo ox-check-update`.
# See ../../docs/design/checks.md for the catalog and rationale.

# Each group is one CI job. Within a group, checks run sequentially.

# PR groups
# ===========================================================================

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
    ox-check-external-types \
    ox-check-aprz

# pr-slow is the single PR-tier group for everything that takes more
# than ~30s per crate (tests, stricter runtimes, mutation testing).
# It's internally split into three sub-recipes so individual concerns
# can be invoked locally without dragging the others along:
#
#   slow1: tests + coverage (replaces the former pr-test group)
#   slow2: stricter-runtime correctness (miri, careful)
#   slow3: mutation testing
#
# CI runs pr-slow as ONE job per OS leg -- the sub-recipes run
# sequentially within. This trades per-leg wall-clock for fewer
# orchestration jobs and a flatter PR check graph. Individual sub-
# recipes are runnable on their own locally:
#
#   $ just ox-check-pr-test   # tests + coverage only
#   $ just ox-check-pr-runtime-analysis   # miri + careful only
#   $ just ox-check-pr-mutants   # mutants only
[group("ox-check")]
ox-check-pr-slow: ox-check-pr-test ox-check-pr-runtime-analysis ox-check-pr-mutants

[group("ox-check")]
ox-check-pr-test: \
    ox-check-llvm-cov \
    ox-check-doc-test \
    ox-check-examples

[group("ox-check")]
ox-check-pr-runtime-analysis: \
    ox-check-miri \
    ox-check-careful

[group("ox-check")]
ox-check-pr-mutants: ox-check-mutants-diff

# Scheduled groups
# ===========================================================================

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
    ox-check-clippy

[group("ox-check")]
ox-check-scheduled-exhaustive: \
    ox-check-mutants-full \
    ox-check-cargo-hack \
    ox-check-bench
# Group-level setup + validate-prereqs
# ===========================================================================
#
# Per-group recipes that fan out to the per-check setup/validate-prereqs
# from checks.just. Setup recipes accept `installer="install"|"binstall"`;
# validate-prereqs recipes take no parameters.

[group("ox-check-setup")]
ox-check-pr-fast-setup installer="install": \
    (ox-check-fmt-setup installer) \
    (ox-check-clippy-setup installer) \
    (ox-check-cargo-sort-setup installer) \
    (ox-check-license-headers-setup installer) \
    (ox-check-ensure-no-cyclic-deps-setup installer) \
    (ox-check-ensure-no-default-features-setup installer) \
    (ox-check-doc-build-setup installer) \
    (ox-check-readme-check-setup installer) \
    (ox-check-spellcheck-setup installer) \
    (ox-check-pr-title-setup installer) \
    (ox-check-deny-setup installer) \
    (ox-check-audit-setup installer) \
    (ox-check-udeps-setup installer) \
    (ox-check-semver-check-setup installer) \
    (ox-check-external-types-setup installer) \
    (ox-check-aprz-setup installer)

[group("ox-check-setup")]
ox-check-pr-fast-validate-prereqs: \
    ox-check-fmt-validate-prereqs \
    ox-check-clippy-validate-prereqs \
    ox-check-cargo-sort-validate-prereqs \
    ox-check-license-headers-validate-prereqs \
    ox-check-ensure-no-cyclic-deps-validate-prereqs \
    ox-check-ensure-no-default-features-validate-prereqs \
    ox-check-doc-build-validate-prereqs \
    ox-check-readme-check-validate-prereqs \
    ox-check-spellcheck-validate-prereqs \
    ox-check-pr-title-validate-prereqs \
    ox-check-deny-validate-prereqs \
    ox-check-audit-validate-prereqs \
    ox-check-udeps-validate-prereqs \
    ox-check-semver-check-validate-prereqs \
    ox-check-external-types-validate-prereqs \
    ox-check-aprz-validate-prereqs

[group("ox-check-setup")]
ox-check-pr-slow-setup installer="install": \
    (ox-check-pr-test-setup installer) \
    (ox-check-pr-runtime-analysis-setup installer) \
    (ox-check-pr-mutants-setup installer)

[group("ox-check-setup")]
ox-check-pr-slow-validate-prereqs: \
    ox-check-pr-test-validate-prereqs \
    ox-check-pr-runtime-analysis-validate-prereqs \
    ox-check-pr-mutants-validate-prereqs

[group("ox-check-setup")]
ox-check-pr-test-setup installer="install": \
    (ox-check-llvm-cov-setup installer) \
    (ox-check-doc-test-setup installer) \
    (ox-check-examples-setup installer)

[group("ox-check-setup")]
ox-check-pr-test-validate-prereqs: \
    ox-check-llvm-cov-validate-prereqs \
    ox-check-doc-test-validate-prereqs \
    ox-check-examples-validate-prereqs

[group("ox-check-setup")]
ox-check-pr-runtime-analysis-setup installer="install": \
    (ox-check-miri-setup installer) \
    (ox-check-careful-setup installer)

[group("ox-check-setup")]
ox-check-pr-runtime-analysis-validate-prereqs: \
    ox-check-miri-validate-prereqs \
    ox-check-careful-validate-prereqs

[group("ox-check-setup")]
ox-check-pr-mutants-setup installer="install": (ox-check-mutants-diff-setup installer)

[group("ox-check-setup")]
ox-check-pr-mutants-validate-prereqs: ox-check-mutants-diff-validate-prereqs

[group("ox-check-setup")]
ox-check-scheduled-test-setup installer="install": \
    (ox-check-llvm-cov-setup installer) \
    (ox-check-doc-test-setup installer) \
    (ox-check-examples-setup installer)

[group("ox-check-setup")]
ox-check-scheduled-test-validate-prereqs: \
    ox-check-llvm-cov-validate-prereqs \
    ox-check-doc-test-validate-prereqs \
    ox-check-examples-validate-prereqs

[group("ox-check-setup")]
ox-check-scheduled-advisories-setup installer="install": \
    (ox-check-deny-setup installer) \
    (ox-check-audit-setup installer) \
    (ox-check-aprz-setup installer) \
    (ox-check-clippy-setup installer)

[group("ox-check-setup")]
ox-check-scheduled-advisories-validate-prereqs: \
    ox-check-deny-validate-prereqs \
    ox-check-audit-validate-prereqs \
    ox-check-aprz-validate-prereqs \
    ox-check-clippy-validate-prereqs

[group("ox-check-setup")]
ox-check-scheduled-exhaustive-setup installer="install": \
    (ox-check-mutants-full-setup installer) \
    (ox-check-cargo-hack-setup installer) \
    (ox-check-bench-setup installer)

[group("ox-check-setup")]
ox-check-scheduled-exhaustive-validate-prereqs: \
    ox-check-mutants-full-validate-prereqs \
    ox-check-cargo-hack-validate-prereqs \
    ox-check-bench-validate-prereqs