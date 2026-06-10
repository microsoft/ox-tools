# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check-update; edit via `cargo ox-check-update`.
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

# Tier-level + global setup + validate-prereqs
# ===========================================================================
#
# Per-tier recipes that fan out to per-group setup/validate-prereqs from
# groups.just. The global `ox-check-setup` / `ox-check-validate-prereqs`
# recipes are the catch-all entry points that install / verify everything.
#
# CI typically invokes only the per-group setup it needs (e.g. the
# `ox-check-pr-fast` composite action / step template runs
# `ox-check-pr-fast-setup` rather than the global `ox-check-setup`).
# Local users who want "install everything" run `just ox-check-setup`.

[group("ox-check-setup")]
ox-check-pr-setup installer="install": \
    (ox-check-pr-fast-setup installer) \
    (ox-check-pr-slow-setup installer)

[group("ox-check-setup")]
ox-check-pr-validate-prereqs: \
    ox-check-pr-fast-validate-prereqs \
    ox-check-pr-slow-validate-prereqs

[group("ox-check-setup")]
ox-check-scheduled-setup installer="install": \
    (ox-check-scheduled-test-setup installer) \
    (ox-check-scheduled-advisories-setup installer) \
    (ox-check-scheduled-exhaustive-setup installer)

[group("ox-check-setup")]
ox-check-scheduled-validate-prereqs: \
    ox-check-scheduled-test-validate-prereqs \
    ox-check-scheduled-advisories-validate-prereqs \
    ox-check-scheduled-exhaustive-validate-prereqs

[group("ox-check-setup")]
ox-check-full-setup installer="install": \
    (ox-check-pr-setup installer) \
    (ox-check-scheduled-setup installer)

[group("ox-check-setup")]
ox-check-full-validate-prereqs: \
    ox-check-pr-validate-prereqs \
    ox-check-scheduled-validate-prereqs

# Global aliases. `ox-check-setup` (no suffix) installs everything the
# catalog knows about; `ox-check-validate-prereqs` verifies every tool
# and component is present at or above its pinned version.
[group("ox-check-setup")]
ox-check-setup installer="install": (ox-check-full-setup installer)

[group("ox-check-setup")]
ox-check-validate-prereqs: ox-check-full-validate-prereqs