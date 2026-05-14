# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# >>> ox-check-managed: ox-check-tools
# Tool-version gate. Owned by cargo-ox-check; edit via `cargo ox-check update`.
# See ../../docs/design/local.md for the policy.

# Public: assert that every tool the catalog uses is present at or above its
# minimum version. Called by every check group as a dependency.
[group("ox-check")]
ox-check-tools-check:
    @just _ox-check-require "cargo"

# Public: install (or upgrade) every catalog tool to its minimum version.
# Idempotent — `cargo install --locked` is a no-op if the version is already
# present. Tools added to the catalog show up here automatically on the next
# `cargo ox-check update`.
[group("ox-check")]
ox-check-tools-install:
    @echo "ox-check: nothing to install yet (catalog is being built up across commits)"

# Internal: assert that `name` is installed. Currently only checks presence —
# future commits parse `cargo install --list` for version comparison.
_ox-check-require name:
    @if ! command -v {{name}} >/dev/null 2>&1 && ! cargo --list 2>/dev/null | grep -q " {{name}}$"; then \
        echo "ox-check: required tool '{{name}}' not found." >&2 ; \
        echo "Install it with: cargo install --locked {{name}}" >&2 ; \
        exit 1 ; \
    fi
# <<< ox-check-managed: ox-check-tools
