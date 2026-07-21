# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/checks.md

# Run the pull request mutation tests.
[group("anvil")]
anvil-pr-mutants: anvil-pr-mutants-validate-prereqs anvil-mutants-diff

# Install prerequisites for the `anvil-pr-mutants` recipe.
[group("anvil-setup")]
anvil-pr-mutants-setup installer="install": (anvil-mutants-diff-setup installer)

# Validate prerequisites for the `anvil-pr-mutants` recipe.
[group("anvil-setup")]
anvil-pr-mutants-validate-prereqs: anvil-mutants-diff-validate-prereqs
