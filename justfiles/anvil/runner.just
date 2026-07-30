# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Route public tier entry points through the configured execution environment.
# ANVIL_IN_CONTAINER always wins to prevent recursive container launches.
#
# `impact` selects the tier's impact-scoping mode: the default "on" leaves
# scoping enabled (PR tier), while "off" exports ANVIL_IMPACT=off before
# invoking the native tier so every check runs full-workspace -- the
# scheduled/full backstop for PR-tier impact scoping. Setting it here (rather
# than in a dep-only tier recipe) ensures the private `_anvil-<tier>` recipe's
# own dependencies, which run before any recipe body, inherit the mode.
[private]
[no-exit-message]
[windows]
[script("pwsh")]
_anvil-run tier runner impact="on":
    if ('{{ replace(impact, "'", "''") }}' -ceq 'off') { $env:ANVIL_IMPACT = 'off' }
    $just = '{{ replace(just_executable(), "'", "''") }}'
    $justfile = '{{ replace(justfile(), "'", "''") }}'
    $nativeTier = '_anvil-{{ replace(tier, "'", "''") }}'
    if ($env:ANVIL_IN_CONTAINER) {
        & $just --justfile $justfile $nativeTier
    } elseif ('{{ replace(runner, "'", "''") }}' -ceq 'container') {
        & $just --justfile $justfile anvil-container $nativeTier
    } elseif ('{{ replace(runner, "'", "''") }}' -ceq 'native') {
        & $just --justfile $justfile $nativeTier
    } else {
        [Console]::Error.WriteLine("anvil-runner: expected 'native' or 'container', got '{{ replace(runner, "'", "''") }}'.")
        exit 2
    }
    exit $LASTEXITCODE

[private]
[no-exit-message]
[unix]
[script("bash")]
_anvil-run tier runner impact="on":
    just_path={{ quote(just_executable()) }}
    justfile={{ quote(justfile()) }}
    tier={{ quote(tier) }}
    runner={{ quote(runner) }}
    impact={{ quote(impact) }}
    if [[ "$impact" == "off" ]]; then
        export ANVIL_IMPACT=off
    fi
    native_tier="_anvil-$tier"
    if [[ -n "${ANVIL_IN_CONTAINER:-}" ]]; then
        exec "$just_path" --justfile "$justfile" "$native_tier"
    elif [[ "$runner" == "container" ]]; then
        exec "$just_path" --justfile "$justfile" anvil-container "$native_tier"
    elif [[ "$runner" == "native" ]]; then
        exec "$just_path" --justfile "$justfile" "$native_tier"
    else
        echo "anvil-runner: expected 'native' or 'container', got '$runner'." >&2
        exit 2
    fi
