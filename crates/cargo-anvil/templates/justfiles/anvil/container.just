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

# How the exec image is obtained: "pull" a pre-built reference, or "build" it
# locally. When building, the tag is a hash of the image's declared inputs, so
# the presence of a tag *is* proof that its contents are current: a changed
# tool pin selects a tag that cannot already exist, and a build follows. There
# is no staleness check because there is no staleness to detect.
anvil_container_image_source := "__IMAGE_SOURCE__"
anvil_container_image_name := "__IMAGE_NAME__"
anvil_container_dockerfile := "__DOCKERFILE__"

# Turns the re-entry guard on. The source of truth is anvil.toml; users never
# set this by hand.
export ANVIL_CONTAINER := "1"

# Resolve the engine: honor an explicit `engine`, else auto-detect docker then podman.
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

# Resolve the exec image reference, building it first if it is not present.
#
# In "pull" mode this echoes the configured reference unchanged and does
# nothing else: the repository has opted into an image whose contents it does
# not describe, so anvil has nothing to verify.
#
# In "build" mode the tag is a SHA-256 over the image's *declared* inputs --
# the Dockerfile, the pinned toolchain, the pinned tool catalog, the resolved
# build args, and any extra `hash-inputs`. Deliberately not the whole recipe
# tree: editing an unrelated check must not cost a multi-minute rebuild. Just
# as deliberately not the build secrets, whose values must never reach a tag.
#
# Set ANVIL_CONTAINER_NO_REBUILD=1 to fail instead of building, which is how a
# cache miss is told apart from a build failure in CI.

# Resolve the exec image reference, building it if it is not already present.
[group("anvil-container")]
[script("pwsh")]
_anvil-container-image:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    # Pull mode: the reference is configured, not derived, so there is nothing
    # to hash. Still make sure it is present -- runs use --pull=never, so the
    # fetch has to happen here or a fresh machine fails with "image not found".
    if ('{{anvil_container_image_source}}' -eq 'pull') {
        & $engine image inspect '{{anvil_container_image}}' *> $null
        if ($LASTEXITCODE -ne 0) {
            if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {
                Write-Error "container image {{anvil_container_image}} is not present and ANVIL_CONTAINER_NO_REBUILD=1"
                exit 1
            }
            [Console]::Error.WriteLine("anvil: pulling {{anvil_container_image}}")
            # Relay progress to stderr line by line: this recipe's stdout is the
            # image reference, and PowerShell has no `*>&2` redirection.
            & $engine pull '{{anvil_container_image}}' | ForEach-Object { [Console]::Error.WriteLine($_) }
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        }
        Write-Output '{{anvil_container_image}}'
        exit 0
    }

    $repoRoot = '{{justfile_directory()}}'
    $buildArgs = __BUILD_ARGS_ARRAY__
    $buildSecrets = __BUILD_SECRETS_ARRAY__
    $hashInputs = __HASH_INPUTS_ARRAY__

    # The image installs its tools by running `just anvil-setup`, whose
    # dependency chain reaches the tier, group, check and tool recipes. All of
    # them therefore define what ends up in the image and must be hashed; only
    # the files that provably cannot are excluded, chiefly this driver itself
    # (hashing it would make the tag depend on the tag). For a consuming
    # repository this tree is generated, so it changes on a cargo-anvil upgrade
    # -- which is exactly when the image should be rebuilt.
    $excluded = __HASH_EXCLUDE_ARRAY__
    $recipeRoot = Join-Path $repoRoot 'justfiles/anvil'
    if (Test-Path -LiteralPath $recipeRoot) {
        foreach ($file in Get-ChildItem -LiteralPath $recipeRoot -Recurse -File -Filter '*.just') {
            $rel = [System.IO.Path]::GetRelativePath($repoRoot, $file.FullName) -replace '\\', '/'
            if ($excluded -cnotcontains $rel) { $hashInputs += $rel }
        }
    }

    # An override must be digest-pinned. A floating tag can change underneath a
    # tag that claims to name fixed content, which would make every cached
    # image a potential lie. The Dockerfile's own default needs no such check:
    # it is hashed as file content.
    $baseImage = $env:ANVIL_CONTAINER_BASE_IMAGE
    if ($baseImage) {
        if ($baseImage -notmatch '@sha256:[0-9a-fA-F]{64}$') {
            Write-Error "ANVIL_CONTAINER_BASE_IMAGE must be digest-pinned (image@sha256:<64 hex>), got '$baseImage'"
            exit 1
        }
        $buildArgs += "BASE_IMAGE=$baseImage"
    }

    # Hash a tagged stream rather than raw concatenation, so no rearrangement
    # of names and contents can collide. Line endings are normalized once, here,
    # so a CRLF checkout and an LF checkout agree on the tag.
    $stream = [System.Text.StringBuilder]::new()
    foreach ($arg in $buildArgs) {
        [void]$stream.Append("arg`n").Append($arg).Append("`n")
    }
    # Ordinal sort and dedup: `Sort-Object -Unique` compares case-insensitively,
    # which would silently drop one of two inputs differing only in case on the
    # case-sensitive filesystem where the image is actually built.
    $ordered = [System.Collections.Generic.SortedSet[string]]::new(
        [string[]]$hashInputs, [System.StringComparer]::Ordinal)
    foreach ($rel in $ordered) {
        $path = Join-Path $repoRoot $rel
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Write-Error "container image input is missing: $rel"
            exit 1
        }
        $text = [System.IO.File]::ReadAllText($path) -replace "`r`n", "`n"
        [void]$stream.Append("file`n").Append($rel).Append("`n").Append($text).Append("`n")
    }
    $digest = [System.Security.Cryptography.SHA256]::HashData(
        [System.Text.Encoding]::UTF8.GetBytes($stream.ToString()))
    # 16 hex characters (64 bits) is far past any practical collision risk for
    # a local image set, and keeps `docker images` readable.
    $imageId = -join ($digest[0..7] | ForEach-Object { $_.ToString('x2') })
    $image = '{{anvil_container_image_name}}' + ':' + $imageId

    # ANVIL_CONTAINER_NO_CACHE forces a rebuild of a tag that already resolves,
    # for the cases a content hash cannot see: a moved upstream package or a
    # base layer that changed behind its digest.
    $noCache = $env:ANVIL_CONTAINER_NO_CACHE -eq '1'
    if (-not $noCache) {
        & $engine image inspect $image *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Output $image
            exit 0
        }
        if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {
            # Still report the reference: a caller that asked not to build is
            # usually asking *which* image is missing.
            Write-Output $image
            [Console]::Error.WriteLine("anvil: $image is not present and ANVIL_CONTAINER_NO_REBUILD=1")
            exit 1
        }
    }

    # Progress goes to stderr: callers capture this recipe's stdout to learn the
    # image reference, so anything else written there becomes part of the value.
    [Console]::Error.WriteLine("anvil: building $image (inputs changed or first run)")
    $buildCmd = @('build', '--file', (Join-Path $repoRoot '{{anvil_container_dockerfile}}'), '--tag', $image)
    if ($noCache) { $buildCmd += '--no-cache' }
    foreach ($arg in $buildArgs) { $buildCmd += @('--build-arg', $arg) }
    foreach ($secret in $buildSecrets) { $buildCmd += @('--secret', $secret) }
    $buildCmd += $repoRoot
    # BuildKit is required for --secret; docker enables it by default from 23.0
    # but an older daemon silently ignores the flag, so ask for it explicitly.
    $env:DOCKER_BUILDKIT = '1'
    & $engine @buildCmd | ForEach-Object { [Console]::Error.WriteLine($_) }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Output $image

# THE single indirection point. Runs `just TARGET` inside the container with
# the repo root mounted at the configured workdir, the caller's working
# directory mapped to its in-container equivalent, the configured cache
# volumes mounted, and the configured host env vars forwarded. Exit code is
# propagated faithfully.

# Run `just TARGET` inside the container. The single indirection point.
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

    $image = (just _anvil-container-image) | Select-Object -Last 1
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

    # --pull=never: the tag names locally-built content, so a miss must be a
    # bug here rather than an invitation to fetch something unrelated.
    $runArgs += @('--pull=never', '-w', $containerCwd, $image, 'just', '{{target}}')
    & $engine @runArgs
    exit $LASTEXITCODE

# Make the exec image available so a subsequent run starts warm: build it when
# it is missing, or pull it when the repository configured a pre-built image.

# Make the exec image available so a subsequent run starts warm.
[group("anvil-container")]
[script("pwsh")]
anvil-container-up:
    $ErrorActionPreference = 'Stop'
    if ('{{anvil_container_image_source}}' -eq 'pull') {
        $engine = just _anvil-container-engine
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        & $engine pull '{{anvil_container_image}}'
        exit $LASTEXITCODE
    }
    $image = (just _anvil-container-image) | Select-Object -Last 1
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Output "image ready: $image"
    exit 0

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

# Open an interactive shell in the image, with the same mounts a recipe run uses.
[group("anvil-container")]
[script("pwsh")]
anvil-container-shell:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $image = (just _anvil-container-image) | Select-Object -Last 1
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $repoRoot = '{{justfile_directory()}}'
    $runArgs = @('run', '--rm', '-it')
    $runArgs += @('-v', "${repoRoot}:{{anvil_container_workdir}}")
    foreach ($vol in __CACHE_MOUNTS_ARRAY__) {
        $runArgs += @('-v', $vol)
    }
    $runArgs += @('-e', 'ANVIL_IN_CONTAINER=1', '--pull=never', '-w', '{{anvil_container_workdir}}', $image)
    & $engine @runArgs
    exit $LASTEXITCODE

# Report the resolved engine, the exec image reference, and whether it is
# present. In build mode the reference embeds the hash of the image's inputs,
# so "absent" and "out of date" are the same condition and are reported as one.

# Report the engine, the exec image, and whether it is present and current.
[group("anvil-container")]
[script("pwsh")]
anvil-container-status:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Output "engine:  $engine"
    Write-Output "source:  {{anvil_container_image_source}}"
    Write-Output "workdir: {{anvil_container_workdir}}"

    if ('{{anvil_container_image_source}}' -eq 'pull') {
        Write-Output "image:   {{anvil_container_image}}"
        & $engine image inspect '{{anvil_container_image}}' *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Output "status:  present locally"
        } else {
            Write-Output "status:  not pulled (run 'just anvil-container-up')"
        }
        exit 0
    }

    Write-Output "dockerfile: {{anvil_container_dockerfile}}"
    # NO_REBUILD turns the resolve into a pure query: report the state instead
    # of silently spending several minutes building from a status command.
    $env:ANVIL_CONTAINER_NO_REBUILD = '1'
    $image = (just _anvil-container-image 2>$null) | Select-Object -Last 1
    $present = $LASTEXITCODE -eq 0
    if ($image) { Write-Output "image:   $image" }
    if ($present) {
        Write-Output "status:  present and current"
    } else {
        Write-Output "status:  no image matches the current inputs (run 'just anvil-container-up')"
    }
    exit 0

# Rebuild the exec image from scratch, ignoring every cached layer.
#
# The ordinary path already rebuilds whenever an input changes, so this is for
# the cases a content hash cannot see: a moved upstream package, a stale base
# layer, or a build that is suspected of being wrong.

# Rebuild the exec image from scratch, ignoring every cached layer.
[group("anvil-container")]
[script("pwsh")]
anvil-container-rebuild:
    $ErrorActionPreference = 'Stop'
    if ('{{anvil_container_image_source}}' -eq 'pull') {
        $engine = just _anvil-container-engine
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        & $engine pull '{{anvil_container_image}}'
        exit $LASTEXITCODE
    }
    $env:ANVIL_CONTAINER_NO_CACHE = '1'
    $image = (just _anvil-container-image) | Select-Object -Last 1
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Output "image rebuilt: $image"
    exit 0
