# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Route public tier entry points through the configured execution environment.
# ANVIL_IN_CONTAINER always wins to prevent recursive container launches.
[private]
[no-exit-message]
[windows]
[script("pwsh")]
_anvil-run tier runner:
    $just = '{{ replace(just_executable(), "'", "''") }}'
    $justfile = '{{ replace(justfile(), "'", "''") }}'
    $nativeTier = '_anvil-{{ replace(tier, "'", "''") }}'
    if ($env:ANVIL_IN_CONTAINER) {
        & $just --justfile $justfile $nativeTier
    } elseif ('{{ replace(runner, "'", "''") }}' -eq 'container') {
        & $just --justfile $justfile anvil-container $nativeTier
    } else {
        & $just --justfile $justfile $nativeTier
    }
    exit $LASTEXITCODE

[private]
[no-exit-message]
[unix]
[script("bash")]
_anvil-run tier runner:
    just_path={{ quote(just_executable()) }}
    justfile={{ quote(justfile()) }}
    tier={{ quote(tier) }}
    runner={{ quote(runner) }}
    native_tier="_anvil-$tier"
    if [[ -n "${ANVIL_IN_CONTAINER:-}" ]]; then
        exec "$just_path" --justfile "$justfile" "$native_tier"
    elif [[ "$runner" == "container" ]]; then
        exec "$just_path" --justfile "$justfile" anvil-container "$native_tier"
    else
        exec "$just_path" --justfile "$justfile" "$native_tier"
    fi
