# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md

# See https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/checks.md

# Required tier. cargo-hack's feature powerset cascades through dep
# features, so the required set (workspace-internal transitive deps)
# is the right scope.

# Check feature combinations for required workspace packages.
[script("pwsh")]
anvil-cargo-hack: anvil-cargo-hack-validate-prereqs anvil-impact
    $ErrorActionPreference = 'Stop'
    $include = (& "{{ just_executable() }}" _anvil-impact-include required)
    if ($include -eq '--skip') { exit 0 }
    & cargo hack @(if ($include) { -split $include } else { '--workspace' }) --feature-powerset --depth 2 check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Install prerequisites for the `anvil-cargo-hack` recipe.
[group("anvil-setup")]
anvil-cargo-hack-setup installer="install": (anvil-tool-cargo-hack-install installer)

# Validate prerequisites for the `anvil-cargo-hack` recipe.
[group("anvil-setup")]
anvil-cargo-hack-validate-prereqs: anvil-tool-cargo-hack-validate-prereqs
