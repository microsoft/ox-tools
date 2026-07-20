# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/checks.md

[group("anvil")]
anvil-pr-mutants: anvil-pr-mutants-validate-prereqs anvil-mutants-diff

[group("anvil-setup")]
anvil-pr-mutants-setup installer="install": (anvil-mutants-diff-setup installer)

[group("anvil-setup")]
anvil-pr-mutants-validate-prereqs: anvil-mutants-diff-validate-prereqs
