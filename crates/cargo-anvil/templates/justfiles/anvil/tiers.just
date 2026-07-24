# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md

# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/checks.md

# PR tier: every check that should run on every pull request, split
# into two groups so the fast checks aren't blocked behind the slow
# ones in cloud workflows.

# Run all pull request checks.
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
# depends on `anvil-impact` and self-populates its scope from the cache,
# the tier sets ANVIL_IMPACT=off so `_anvil-impact-include` returns each
# tier's full-workspace default and the `anvil-impact` dependency no-ops.
# A dependency-only recipe can't set env for its own deps (deps run
# before the body), so this is a thin two-stage wrapper: the body exports
# the var and re-invokes `just` on the private impl recipe, whose deps
# then inherit ANVIL_IMPACT=off.

# Run all scheduled checks.
[group("anvil")]
[script("pwsh")]
anvil-scheduled:
    $env:ANVIL_IMPACT = 'off'
    & "{{ just_executable() }}" _anvil-scheduled-impl
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

_anvil-scheduled-impl: anvil-scheduled-validate-prereqs \
    anvil-scheduled-test \
    anvil-scheduled-advisories \
    anvil-scheduled-runtime-analysis \
    anvil-scheduled-exhaustive

# Runs everything full-workspace (ANVIL_IMPACT=off), same wrapper shape
# as anvil-scheduled.

# Full tier: PR + scheduled, end-to-end. Useful before tagging a release.
[group("anvil")]
[script("pwsh")]
anvil-full:
    $env:ANVIL_IMPACT = 'off'
    & "{{ just_executable() }}" _anvil-full-impl
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

# Install prerequisites for the `anvil-pr` recipe.
[group("anvil-setup")]
anvil-pr-setup installer="install": \
    (anvil-pr-fast-setup installer) \
    (anvil-pr-slow-setup installer)

# Validate prerequisites for the `anvil-pr` recipe.
[group("anvil-setup")]
anvil-pr-validate-prereqs: \
    anvil-pr-fast-validate-prereqs \
    anvil-pr-slow-validate-prereqs

# Install prerequisites for the `anvil-scheduled` recipe.
[group("anvil-setup")]
anvil-scheduled-setup installer="install": \
    (anvil-scheduled-test-setup installer) \
    (anvil-scheduled-advisories-setup installer) \
    (anvil-scheduled-runtime-analysis-setup installer) \
    (anvil-scheduled-exhaustive-setup installer)

# Validate prerequisites for the `anvil-scheduled` recipe.
[group("anvil-setup")]
anvil-scheduled-validate-prereqs: \
    anvil-scheduled-test-validate-prereqs \
    anvil-scheduled-advisories-validate-prereqs \
    anvil-scheduled-runtime-analysis-validate-prereqs \
    anvil-scheduled-exhaustive-validate-prereqs

# Install prerequisites for the `anvil-full` recipe.
[group("anvil-setup")]
anvil-full-setup installer="install": \
    (anvil-pr-setup installer) \
    (anvil-scheduled-setup installer)

# Validate prerequisites for the `anvil-full` recipe.
[group("anvil-setup")]
anvil-full-validate-prereqs: \
    anvil-pr-validate-prereqs \
    anvil-scheduled-validate-prereqs

# Global aliases. `anvil-setup` (no suffix) installs everything the
# catalog knows about; `anvil-validate-prereqs` verifies every tool
# and component is present at or above its pinned version.
#
# cargo-delta (the impact-analysis tool) is a prerequisite of every group
# whose checks are impact-scoped: those checks depend on `anvil-impact`,
# which invokes cargo-delta when it has to (re)compute the impact set, so
# each such group's `-setup` / `-validate-prereqs` installs / verifies it
# (fail-fast, rather than discovering it missing mid-run). In a cloud
# workflow a group job normally consumes the downloaded impact artifact and
# never actually runs cargo-delta, but the tool is guaranteed present.
# `anvil-setup` also lists it explicitly as the catch-all (harmless -- just
# dedups the fan-out).

# Install every tool and component in the anvil catalog.
[group("anvil-setup")]
anvil-setup installer="install": (anvil-full-setup installer) (anvil-tool-cargo-delta-install installer)

# Validate every tool and component in the anvil catalog.
[group("anvil-setup")]
anvil-validate-prereqs: anvil-full-validate-prereqs anvil-tool-cargo-delta-validate-prereqs