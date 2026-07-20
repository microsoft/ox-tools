# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.
# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/checks.md

[group("anvil")]
anvil-pr-test: anvil-pr-test-validate-prereqs \
    anvil-llvm-cov \
    anvil-doc-test \
    anvil-examples

[group("anvil-setup")]
anvil-pr-test-setup installer="install": \
    (anvil-llvm-cov-setup installer) \
    (anvil-doc-test-setup installer) \
    (anvil-examples-setup installer)

[group("anvil-setup")]
anvil-pr-test-validate-prereqs: \
    anvil-llvm-cov-validate-prereqs \
    anvil-doc-test-validate-prereqs \
    anvil-examples-validate-prereqs
