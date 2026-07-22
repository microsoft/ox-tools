# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Route public tier entry points through the configured execution environment.
# ANVIL_IN_CONTAINER always wins to prevent recursive container launches.
[private]
[windows]
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

[private]
[unix]
[script("bash")]
_anvil-run tier runner:
    just_path={{ quote(just_executable()) }}
    tier={{ quote(tier) }}
    runner={{ quote(runner) }}
    native_tier="_anvil-$tier"
    if [[ -n "${ANVIL_IN_CONTAINER:-}" ]]; then
        exec "$just_path" "$native_tier"
    elif [[ "$runner" == "container" ]]; then
        exec "$just_path" anvil-container "$native_tier"
    else
        exec "$just_path" "$native_tier"
    fi
