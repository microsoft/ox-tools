# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See ../../docs/design/checks.md for the tier structure.

# PR tier: every check that should run on every pull request, split
# into two groups so the fast checks aren't blocked behind the slow
# ones in cloud workflows.
[group("anvil")]
anvil-pr: anvil-pr-validate-prereqs \
    anvil-pr-fast \
    anvil-pr-slow

# scheduled tier: full-workspace re-runs of things that can change
# without a commit (advisories, flakes) + the truly expensive
# exhaustive checks that don't fit in a PR budget. Runs on a schedule
# against `main`, not on PRs.
#
# The scheduled tier is deliberately NOT impact-scoped: it is the
# catch-all that backstops PR-tier scoping. Because every check now
# depends on `anvil-impact` and resolves its scope from the cache, the
# tier sets ANVIL_IMPACT=off so `_anvil-impact-include` returns each
# tier's full-workspace default and the `anvil-impact` dependency
# no-ops. A dependency-only recipe can't set env for its own deps (deps
# run before the body), so this is a thin two-stage wrapper: the body
# exports the var and re-invokes `just` on the private impl recipe,
# whose deps then inherit ANVIL_IMPACT=off.
[group("anvil")]
[script("pwsh")]
anvil-scheduled:
    $env:ANVIL_IMPACT = 'off'
    just _anvil-scheduled-impl
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

_anvil-scheduled-impl: anvil-scheduled-validate-prereqs \
    anvil-scheduled-test \
    anvil-scheduled-advisories \
    anvil-scheduled-runtime-analysis \
    anvil-scheduled-exhaustive

# Full tier: PR + scheduled, end-to-end. Useful before tagging a release.
# Runs everything full-workspace (ANVIL_IMPACT=off), same wrapper shape
# as anvil-scheduled.
[group("anvil")]
[script("pwsh")]
anvil-full:
    $env:ANVIL_IMPACT = 'off'
    just _anvil-full-impl
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

_anvil-full-impl: anvil-full-validate-prereqs \
    anvil-pr \
    _anvil-scheduled-impl

# Tier-level + global setup + validate-prereqs
# ===========================================================================
#
# Per-tier recipes that fan out to per-group setup/validate-prereqs from
# groups.just. The global `anvil-setup` / `anvil-validate-prereqs`
# recipes are the catch-all entry points that install / verify everything.
#
# Cloud workflows typically invoke only the per-group setup it needs (e.g. the
# `anvil-pr-fast` composite action / step template runs
# `anvil-pr-fast-setup` rather than the global `anvil-setup`).
# Local users who want "install everything" run `just anvil-setup`.

[group("anvil-setup")]
anvil-pr-setup installer="install": \
    (anvil-pr-fast-setup installer) \
    (anvil-pr-slow-setup installer)

[group("anvil-setup")]
anvil-pr-validate-prereqs: \
    anvil-pr-fast-validate-prereqs \
    anvil-pr-slow-validate-prereqs

[group("anvil-setup")]
anvil-scheduled-setup installer="install": \
    (anvil-scheduled-test-setup installer) \
    (anvil-scheduled-advisories-setup installer) \
    (anvil-scheduled-runtime-analysis-setup installer) \
    (anvil-scheduled-exhaustive-setup installer)

[group("anvil-setup")]
anvil-scheduled-validate-prereqs: \
    anvil-scheduled-test-validate-prereqs \
    anvil-scheduled-advisories-validate-prereqs \
    anvil-scheduled-runtime-analysis-validate-prereqs \
    anvil-scheduled-exhaustive-validate-prereqs

[group("anvil-setup")]
anvil-full-setup installer="install": \
    (anvil-pr-setup installer) \
    (anvil-scheduled-setup installer)

[group("anvil-setup")]
anvil-full-validate-prereqs: \
    anvil-pr-validate-prereqs \
    anvil-scheduled-validate-prereqs

# Global aliases. `anvil-setup` (no suffix) installs everything the
# catalog knows about; `anvil-validate-prereqs` verifies every tool
# and component is present at or above its pinned version.
[group("anvil-setup")]
anvil-setup installer="install": (anvil-full-setup installer)

[group("anvil-setup")]
anvil-validate-prereqs: anvil-full-validate-prereqs