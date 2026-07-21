# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Run any Anvil recipe in the pinned local Linux container. With no recipe,
# open an interactive shell.
[windows]
[group("anvil-container")]
[script("pwsh")]
anvil-container *recipe:
    $requested = @('{{recipe}}' -split '\s+' | Where-Object { $_ })
    & 'justfiles/anvil/container/run-in-container.ps1' @requested
    exit $LASTEXITCODE

[unix]
[group("anvil-container")]
[script("bash")]
anvil-container *recipe:
    requested={{ quote(recipe) }}
    if [[ -z "$requested" ]]; then
        exec bash 'justfiles/anvil/container/run-in-container.sh'
    fi
    read -r -a requested_args <<<"$requested"
    exec bash 'justfiles/anvil/container/run-in-container.sh' "${requested_args[@]}"
