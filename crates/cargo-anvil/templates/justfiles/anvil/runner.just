# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Route public tier entry points through the configured execution environment.
# ANVIL_IN_CONTAINER always wins to prevent recursive container launches.
[private]
[script("pwsh")]
_anvil-run tier runner:
    $just = '{{just_executable()}}'
    $nativeTier = '_anvil-{{tier}}'
    if ($env:ANVIL_IN_CONTAINER) {
        & $just $nativeTier
    } elseif ('{{runner}}' -eq 'container') {
        & $just anvil-container $nativeTier
    } else {
        & $just $nativeTier
    }
    exit $LASTEXITCODE
