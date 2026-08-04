# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md

# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/benchmarks.md

# scheduled-benchmarks holds the one check whose verdict depends on state
# carried between runs. Keeping it in its own group isolates that history
# round-trip and its fail-on-regression semantics from the rest of the
# scheduled work, so a red build names the regression unambiguously.

# Run the scheduled benchmark regression detection.
[group("anvil")]
anvil-scheduled-benchmarks: anvil-scheduled-benchmarks-validate-prereqs \
    anvil-bench-history

# Install prerequisites for the `anvil-scheduled-benchmarks` recipe.
[group("anvil-setup")]
anvil-scheduled-benchmarks-setup installer="install": \
    (anvil-bench-history-setup installer)

# Validate prerequisites for the `anvil-scheduled-benchmarks` recipe.
[group("anvil-setup")]
anvil-scheduled-benchmarks-validate-prereqs: \
    anvil-bench-history-validate-prereqs
