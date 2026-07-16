# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Owned by cargo-anvil; edit via `cargo anvil`.

# Run any Anvil recipe in the pinned local Linux container. With no recipe,
# open an interactive shell.
[group("anvil-container")]
[script("pwsh")]
anvil-container *recipe:
    $requested = @('{{recipe}}' -split '\s+' | Where-Object { $_ })
    if ($IsWindows) {
        & 'justfiles/anvil/container/run-in-container.ps1' @requested
    } else {
        & bash 'justfiles/anvil/container/run-in-container.sh' @requested
    }
    exit $LASTEXITCODE
