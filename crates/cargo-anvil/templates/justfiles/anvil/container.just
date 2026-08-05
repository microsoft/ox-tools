# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md
#
# Container execution shim. Emitted only when `[container] enabled = true` in
# anvil.toml. Every value below is baked from that file at generation time, so
# the shim and the devcontainer can never drift from the configured settings.
#
# The tier and group recipes carry a re-entry guard: when ANVIL_CONTAINER is
# "1" (exported below) and ANVIL_IN_CONTAINER is not yet set, the outermost
# recipe re-invokes itself through `_anvil-container-run`, which runs the same
# `just` target inside the container. Inner group invocations then see
# ANVIL_IN_CONTAINER=1 and run natively, so the work happens exactly once.

# Fully-resolved container settings (from anvil.toml + catalog defaults).
anvil_container_image := "__IMAGE__"
anvil_container_engine := "__ENGINE__"
anvil_container_workdir := "__WORKDIR__"
anvil_container_repo := "__REPO__"

# Turns the re-entry guard on. The source of truth is anvil.toml; users never
# set this by hand.
export ANVIL_CONTAINER := "1"

# Resolve the container engine: honor an explicit `engine`, else auto-detect
# docker then podman. Prints the resolved engine name on stdout.
[group("anvil-container")]
[script("pwsh")]
_anvil-container-engine:
    $ErrorActionPreference = 'Stop'
    $configured = '{{anvil_container_engine}}'
    if ($configured -ne 'auto') {
        if (-not (Get-Command $configured -ErrorAction SilentlyContinue)) {
            Write-Error "configured container engine '$configured' was not found on PATH"
            exit 1
        }
        Write-Output $configured
        exit 0
    }
    foreach ($candidate in @('docker', 'podman')) {
        if (Get-Command $candidate -ErrorAction SilentlyContinue) {
            Write-Output $candidate
            exit 0
        }
    }
    Write-Error "no container engine found (looked for docker then podman); install one or set [container] engine in anvil.toml"
    exit 1

# THE single indirection point. Runs `just TARGET` inside the container with
# the repo root mounted at the configured workdir, the caller's working
# directory mapped to its in-container equivalent, the configured cache
# volumes mounted, and the configured host env vars forwarded. Exit code is
# propagated faithfully.
[group("anvil-container")]
[script("pwsh")]
_anvil-container-run target:
    $ErrorActionPreference = 'Stop'

    # native-when: if the host matches, run natively instead of containerizing.
    # ANVIL_IN_CONTAINER is set first so the re-invoked target's guard treats
    # this as already-inside and does not recurse back here.
    $nativeWhen = __NATIVE_WHEN_HASHTABLE__
    if ($nativeWhen.Count -gt 0 -and (Test-Path '/etc/os-release')) {
        $osr = @{}
        foreach ($line in Get-Content '/etc/os-release') {
            if ($line -match '^\s*([A-Za-z0-9_]+)=(.*)$') {
                $osr[$matches[1]] = $matches[2].Trim().Trim('"')
            }
        }
        $matched = $true
        foreach ($key in $nativeWhen.Keys) {
            if ($osr[$key] -ne $nativeWhen[$key]) { $matched = $false; break }
        }
        if ($matched) {
            $env:ANVIL_IN_CONTAINER = '1'
            just {{target}}
            exit $LASTEXITCODE
        }
    }

    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $repoRoot = '{{justfile_directory()}}'
    $cwd = '{{invocation_directory()}}'
    $rel = [System.IO.Path]::GetRelativePath($repoRoot, $cwd) -replace '\\', '/'
    if ($rel -eq '.' -or [string]::IsNullOrEmpty($rel)) {
        $containerCwd = '{{anvil_container_workdir}}'
    } else {
        $containerCwd = '{{anvil_container_workdir}}/' + $rel
    }

    $runArgs = @('run', '--rm', '-i')
    $runArgs += @('-v', "${repoRoot}:{{anvil_container_workdir}}")
    foreach ($vol in __CACHE_MOUNTS_ARRAY__) {
        $runArgs += @('-v', $vol)
    }

    $runArgs += @('-e', 'ANVIL_IN_CONTAINER=1')
    $patterns = __FORWARD_ENV_ARRAY__
    if ($patterns.Count -gt 0) {
        foreach ($entry in Get-ChildItem Env:) {
            foreach ($pattern in $patterns) {
                if ($entry.Name -like $pattern) {
                    $runArgs += @('-e', "$($entry.Name)=$($entry.Value)")
                    break
                }
            }
        }
    }

    $runArgs += @('-w', $containerCwd, '{{anvil_container_image}}', 'just', '{{target}}')
    & $engine @runArgs
    exit $LASTEXITCODE

# Pull the configured image so a subsequent run starts warm.
[group("anvil-container")]
[script("pwsh")]
anvil-container-up:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & $engine pull '{{anvil_container_image}}'
    exit $LASTEXITCODE

# Remove the named cache volumes for this repo. The image is left in place.
[group("anvil-container")]
[script("pwsh")]
anvil-container-down:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    foreach ($vol in __CACHE_VOLUME_NAMES_ARRAY__) {
        & $engine volume rm -f $vol
    }
    exit 0

# Open an interactive shell in the image, with the same repo and cache mounts
# `_anvil-container-run` uses.
[group("anvil-container")]
[script("pwsh")]
anvil-container-shell:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $repoRoot = '{{justfile_directory()}}'
    $runArgs = @('run', '--rm', '-it')
    $runArgs += @('-v', "${repoRoot}:{{anvil_container_workdir}}")
    foreach ($vol in __CACHE_MOUNTS_ARRAY__) {
        $runArgs += @('-v', $vol)
    }
    $runArgs += @('-e', 'ANVIL_IN_CONTAINER=1', '-w', '{{anvil_container_workdir}}', '{{anvil_container_image}}')
    & $engine @runArgs
    exit $LASTEXITCODE

# Report the resolved engine, configured image, and whether the image is
# present locally.
[group("anvil-container")]
[script("pwsh")]
anvil-container-status:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Output "engine:  $engine"
    Write-Output "image:   {{anvil_container_image}}"
    Write-Output "workdir: {{anvil_container_workdir}}"
    $present = & $engine image inspect '{{anvil_container_image}}' 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Output "status:  image present locally"
    } else {
        Write-Output "status:  image not pulled (run 'just anvil-container-up')"
    }
    exit 0

# Force a fresh pull of the configured image.
[group("anvil-container")]
[script("pwsh")]
anvil-container-rebuild:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    & $engine pull '{{anvil_container_image}}'
    exit $LASTEXITCODE
