# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/checks.md for the tier structure.

# PR tier: every fast check that should run on every pull request.
[group("ox-check")]
ox-check-pr: \
    ox-check-pr-fast \
    ox-check-pr-test \
    ox-check-pr-mutants

# scheduled tier: slow / non-deterministic checks. Runs on a schedule against
# `main`, not on PRs.
[group("ox-check")]
ox-check-scheduled: \
    ox-check-scheduled-test \
    ox-check-scheduled-advisories \
    ox-check-scheduled-runtime \
    ox-check-scheduled-exhaustive

# Full tier: PR + scheduled, end-to-end. Useful before tagging a release.
[group("ox-check")]
ox-check-full: \
    ox-check-pr \
    ox-check-scheduled
