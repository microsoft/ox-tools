# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
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
#   $ just ox-check-pr-slow1   # tests + coverage only
#   $ just ox-check-pr-slow2   # miri + careful only
#   $ just ox-check-pr-slow3   # mutants only
[group("ox-check")]
ox-check-pr-slow: ox-check-pr-slow1 ox-check-pr-slow2 ox-check-pr-slow3

[group("ox-check")]
ox-check-pr-slow1: \
    ox-check-llvm-cov \
    ox-check-doc-test \
    ox-check-examples

[group("ox-check")]
ox-check-pr-slow2: \
    ox-check-miri \
    ox-check-careful

[group("ox-check")]
ox-check-pr-slow3: ox-check-mutants

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