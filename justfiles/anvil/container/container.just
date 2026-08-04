# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Run any Anvil recipe in the pinned local Linux container. With no recipe,
# open an interactive shell.
[windows]
[group("anvil-container")]
[script("pwsh")]
anvil-container *recipe:
    $requested = @('{{ replace(recipe, "'", "''") }}' -split '\s+' | Where-Object { $_ })
    & '.anvil/container/run-in-container.ps1' @requested
    exit $LASTEXITCODE

[windows]
[group("anvil-container")]
anvil-container-doctor:
    pwsh -NoProfile -File '.anvil/container/setup-docker-in-wsl.ps1' -Doctor

[windows]
[group("anvil-container")]
anvil-container-bootstrap:
    pwsh -NoProfile -File '.anvil/container/setup-docker-in-wsl.ps1'

[unix]
[group("anvil-container")]
[script("bash")]
anvil-container *recipe:
    requested={{ quote(recipe) }}
    if [[ -z "$requested" ]]; then
        exec bash '.anvil/container/run-in-container.sh'
    fi
    read -r -a requested_args <<<"$requested"
    exec bash '.anvil/container/run-in-container.sh' "${requested_args[@]}"
