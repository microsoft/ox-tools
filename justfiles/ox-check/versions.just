# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-ox-check; edit via `cargo ox-check update`.
#
# Pinned toolchain versions used by the ox-check check tree.
#
# These are deliberately pinned (not floating). The cost of floating
# nightly is "rustup update on Tuesday breaks CI on Wednesday" — we'd
# rather move the pin deliberately than wake up to surprise red builds.
# See docs/design/local.md#nightly-pinning for the bump policy.
#
# Both ox-check recipes (via the `{{ var }}` interpolation below) and
# the setup-action / setup.yml composites (via `just --evaluate`) read
# these constants. There is exactly one source of truth.

# General nightly used by udeps, miri, careful, and any future
# nightly-dependent check. Bumped on a regular cadence (monthly is a
# reasonable default) when an adopter has time to absorb formatting /
# lint / API-surface drift. Pin only — do not use bare `nightly` here.
rust_nightly := "nightly-2026-02-10"

# Pinned narrowly to the rustdoc JSON schema version that the currently
# selected cargo-check-external-types release accepts. cargo-check-
# external-types embeds a specific rustdoc-types crate version; if the
# nightly's emitted JSON format_version drifts past it, every run fails
# with "produces JSON format version X, but this tool requires
# format version Y" — which is a tooling-incompat, not a real API
# violation. Bump this alongside any cargo-check-external-types upgrade.
rust_nightly_external_types := "nightly-2025-10-18"
