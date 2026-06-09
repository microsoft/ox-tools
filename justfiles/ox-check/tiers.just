# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/checks.md for the tier structure.

# PR tier: every check that should run on every pull request, split
# into two groups so the fast checks aren't blocked behind the slow
# ones in CI.
[group("ox-check")]
ox-check-pr: \
    ox-check-pr-fast \
    ox-check-pr-slow

# scheduled tier: full-workspace re-runs of things that can change
# without a commit (advisories, flakes) + the truly expensive
# exhaustive checks that don't fit in a PR budget. Runs on a schedule
# against `main`, not on PRs.
[group("ox-check")]
ox-check-scheduled: \
    ox-check-scheduled-test \
    ox-check-scheduled-advisories \
    ox-check-scheduled-exhaustive

# Full tier: PR + scheduled, end-to-end. Useful before tagging a release.
[group("ox-check")]
ox-check-full: \
    ox-check-pr \
    ox-check-scheduled
