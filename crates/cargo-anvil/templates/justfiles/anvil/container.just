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

# How the exec image is obtained: "pull" a pre-built reference, "build" one
# locally, or "extend" -- build anvil's image and then layer the repository's
# own on top of it. When building, the tag is a hash of the image's declared
# inputs, so the presence of a tag *is* proof that its contents are current: a
# changed tool pin selects a tag that cannot already exist, and a build
# follows. There is no staleness check because there is no staleness to detect.
#
# Extending splits that into two tags. The base carries the toolchain and the
# tool catalog and changes rarely; the extension carries the repository's own
# additions and changes often. A change to the cheap half therefore rebuilds
# only the cheap half, instead of the whole stack.
anvil_container_image_source := "__IMAGE_SOURCE__"
anvil_container_image_name := "__IMAGE_NAME__"
anvil_container_dockerfile := "__DOCKERFILE__"
anvil_container_ext_image_name := "__EXT_IMAGE_NAME__"
anvil_container_ext_dockerfile := "__EXT_DOCKERFILE__"

# Turns the re-entry guard on. The source of truth is anvil.toml; users never
# set this by hand.
export ANVIL_CONTAINER := "1"

# Resolve the engine: honor an explicit `engine`, else auto-detect docker then podman.
[group("anvil-container")]
[script("pwsh", "-NoProfile")]
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
# In "extend" mode that happens twice: anvil's base image first, then the
# repository's own layered on top with the resolved base injected as the
# ANVIL_BASE_IMAGE build arg. The base tag is pinned into the extension's hash,
# so a rebuilt base always renames the extension, while a change confined to
# the extension leaves the expensive base cached.
#
# Set ANVIL_CONTAINER_NO_REBUILD=1 to fail instead of building, which is how a
# cache miss is told apart from a build failure in CI.

# Resolve the exec image reference, building it if it is not already present.
[group("anvil-container")]
[script("pwsh", "-NoProfile")]
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
    $userBuildArgs = __BUILD_ARGS_ARRAY__
    $userBuildSecrets = __BUILD_SECRETS_ARRAY__
    $hashInputs = __HASH_INPUTS_ARRAY__
    $extending = '{{anvil_container_image_source}}' -eq 'extend'

    # A declared secret whose source is missing must stop the build.
    #
    # BuildKit does not: `--secret id=x,env=UNSET` succeeds and mounts an empty
    # file, so a Dockerfile without `required=true` installs a reduced tool set
    # and exits 0. That degraded image is then tagged with the *same* content
    # hash a fully-credentialed build would produce -- secret values are
    # deliberately not hashed -- so every later run resolves the tag and reuses
    # the broken image. Failing here keeps "the tag is present" meaning "the
    # contents are current".
    foreach ($secret in $userBuildSecrets) {
        foreach ($field in ($secret -split ',')) {
            $pair = $field.Trim()
            if ($pair -match '^env=(.+)$') {
                if (-not [Environment]::GetEnvironmentVariable($Matches[1])) {
                    Write-Error "anvil: build secret '$secret' names environment variable '$($Matches[1])', which is unset or empty"
                    exit 1
                }
            } elseif ($pair -match '^src=(.+)$') {
                $path = $Matches[1]
                if (-not [System.IO.Path]::IsPathRooted($path)) { $path = Join-Path $repoRoot $path }
                if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or (Get-Item -LiteralPath $path).Length -eq 0) {
                    Write-Error "anvil: build secret '$secret' names file '$($Matches[1])', which is missing or empty"
                    exit 1
                }
            }
        }
    }

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
    $baseArgs = @()
    $baseImage = $env:ANVIL_CONTAINER_BASE_IMAGE
    if ($baseImage) {
        if ($baseImage -notmatch '@sha256:[0-9a-fA-F]{64}$') {
            Write-Error "ANVIL_CONTAINER_BASE_IMAGE must be digest-pinned (image@sha256:<64 hex>), got '$baseImage'"
            exit 1
        }
        $baseArgs += "BASE_IMAGE=$baseImage"
    }

    # ANVIL_CONTAINER_NO_CACHE forces a rebuild of a tag that already resolves,
    # for the cases a content hash cannot see: a moved upstream package or a
    # base layer that changed behind its digest.
    $noCache = $env:ANVIL_CONTAINER_NO_CACHE -eq '1'

    # One image: hash its inputs, reuse the tag if it resolves, build if not.
    # Used once for a plain build, and twice when extending -- the base and the
    # layer on top follow identical rules, and `$pinned` is what ties them
    # together: the extension folds the resolved base tag into its own hash, so
    # a rebuilt base necessarily renames the extension.
    function Resolve-AnvilImage {
        param(
            [string]$Name, [string]$Dockerfile, [string[]]$Inputs,
            [string[]]$BuildArgs, [string[]]$Secrets, [string[]]$Pinned
        )

        # Hash a tagged stream rather than raw concatenation, so no rearrangement
        # of names and contents can collide. Line endings are normalized once,
        # here, so a CRLF checkout and an LF checkout agree on the tag.
        $stream = [System.Text.StringBuilder]::new()
        foreach ($value in $Pinned) {
            [void]$stream.Append("pin`n").Append($value).Append("`n")
        }
        foreach ($arg in $BuildArgs) {
            [void]$stream.Append("arg`n").Append($arg).Append("`n")
        }
        # Ordinal sort and dedup: `Sort-Object -Unique` compares case-insensitively,
        # which would silently drop one of two inputs differing only in case on the
        # case-sensitive filesystem where the image is actually built.
        $ordered = [System.Collections.Generic.SortedSet[string]]::new(
            [string[]]$Inputs, [System.StringComparer]::Ordinal)
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
        $image = $Name + ':' + $imageId

        if (-not $noCache) {
            & $engine image inspect $image *> $null
            if ($LASTEXITCODE -eq 0) { return $image }
            if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {
                # Still report the reference: a caller that asked not to build is
                # usually asking *which* image is missing.
                Write-Output $image
                [Console]::Error.WriteLine("anvil: $image is not present and ANVIL_CONTAINER_NO_REBUILD=1")
                exit 1
            }
        }

        # Progress goes to stderr: callers capture this recipe's stdout to learn
        # the image reference, so anything else written there becomes part of it.
        [Console]::Error.WriteLine("anvil: building $image (inputs changed or first run)")
        # Pinned, not inferred from the host. The Dockerfile installs amd64
        # toolchains and verifies amd64 checksums, so an arm host would resolve the
        # multi-arch base to arm64 and fail late with an exec-format error. It also
        # keeps the identity scheme honest: without this, two hosts of different
        # architecture compute the same tag for genuinely different images.
        $buildCmd = @('build', '--platform', 'linux/amd64', '--file', (Join-Path $repoRoot $Dockerfile), '--tag', $image)
        if ($noCache) { $buildCmd += '--no-cache' }
        foreach ($arg in $BuildArgs) { $buildCmd += @('--build-arg', $arg) }
        foreach ($secret in $Secrets) { $buildCmd += @('--secret', $secret) }
        $buildCmd += $repoRoot
        # BuildKit is required for --secret; docker enables it by default from
        # 23.0 but an older daemon silently ignores the flag, so ask explicitly.
        $env:DOCKER_BUILDKIT = '1'
        & $engine @buildCmd | ForEach-Object { [Console]::Error.WriteLine($_) }
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        return $image
    }

    # When extending, the repository's build inputs describe the layer it owns,
    # not anvil's base underneath: applying them to the base would rebuild the
    # expensive half for changes that only concern the cheap one.
    $image = Resolve-AnvilImage `
        -Name '{{anvil_container_image_name}}' `
        -Dockerfile '{{anvil_container_dockerfile}}' `
        -Inputs $hashInputs `
        -BuildArgs ($extending ? $baseArgs : ($baseArgs + $userBuildArgs)) `
        -Secrets ($extending ? @() : $userBuildSecrets)

    if ($extending) {
        # The base reference is not knowable until it is resolved, so it is
        # injected rather than written into the Dockerfile, and pinned into the
        # extension's hash so the two can never drift apart.
        $image = Resolve-AnvilImage `
            -Name '{{anvil_container_ext_image_name}}' `
            -Dockerfile '{{anvil_container_ext_dockerfile}}' `
            -Inputs __EXT_HASH_INPUTS_ARRAY__ `
            -BuildArgs (@("ANVIL_BASE_IMAGE=$image") + $userBuildArgs) `
            -Secrets $userBuildSecrets `
            -Pinned @($image)
    }

    Write-Output $image

# THE single indirection point. Runs `just TARGET` inside the container with
# the repo root mounted at the configured workdir, the caller's working
# directory mapped to its in-container equivalent, the configured cache
# volumes mounted, and the configured host env vars forwarded. Exit code is
# propagated faithfully.

# Run `just TARGET` inside the container. The single indirection point.
[group("anvil-container")]
[script("pwsh", "-NoProfile")]
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

    $runArgs = @('run', '--rm', '-i', '--platform', 'linux/amd64')
    $runArgs += @('-v', "${repoRoot}:{{anvil_container_workdir}}")
    # Match the caller's uid/gid on Linux. Without this everything the run
    # writes under the bind mount -- target/, generated files -- lands as root
    # on the host, and the next native cargo build or git clean fails with
    # EACCES a long way from the cause. Docker Desktop on Windows and macOS
    # already maps ownership, and id is not there to ask.
    if (-not $IsWindows -and -not $IsMacOS) {
        $hostUid = (id -u); $hostGid = (id -g)
        if ($LASTEXITCODE -eq 0 -and $hostUid -ne '0') { $runArgs += @('--user', "${hostUid}:${hostGid}") }
    }
    foreach ($vol in __CACHE_MOUNTS_ARRAY__) {
        $runArgs += @('-v', $vol)
    }

    $runArgs += @('-e', 'ANVIL_IN_CONTAINER=1')
    # Forward by NAME, never as NAME=VALUE: the engine copies the value from the
    # environment it inherits, so a credential never appears in the host's
    # process command line, where endpoint telemetry records and retains it for
    # far longer than a short-lived token is meant to live.
    #
    # The matched names are reported because these are globs with no deny-list:
    # a broad pattern silently hands extra variables to a process that runs
    # third-party build scripts and proc macros. Names only -- never values.
    $patterns = __FORWARD_ENV_ARRAY__
    if ($patterns.Count -gt 0) {
        $forwarded = @()
        foreach ($entry in Get-ChildItem Env:) {
            foreach ($pattern in $patterns) {
                if ($entry.Name -like $pattern) {
                    $runArgs += @('-e', $entry.Name)
                    $forwarded += $entry.Name
                    break
                }
            }
        }
        if ($forwarded.Count -gt 0) {
            [Console]::Error.WriteLine("anvil: forwarding env: $($forwarded -join ', ')")
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
[script("pwsh", "-NoProfile")]
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
[script("pwsh", "-NoProfile")]
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
[script("pwsh", "-NoProfile")]
anvil-container-shell:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $image = (just _anvil-container-image) | Select-Object -Last 1
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $repoRoot = '{{justfile_directory()}}'
    $runArgs = @('run', '--rm', '-it', '--platform', 'linux/amd64')
    $runArgs += @('-v', "${repoRoot}:{{anvil_container_workdir}}")
    # Match the caller's uid/gid on Linux. Without this everything the run
    # writes under the bind mount -- target/, generated files -- lands as root
    # on the host, and the next native cargo build or git clean fails with
    # EACCES a long way from the cause. Docker Desktop on Windows and macOS
    # already maps ownership, and id is not there to ask.
    if (-not $IsWindows -and -not $IsMacOS) {
        $hostUid = (id -u); $hostGid = (id -g)
        if ($LASTEXITCODE -eq 0 -and $hostUid -ne '0') { $runArgs += @('--user', "${hostUid}:${hostGid}") }
    }
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
[script("pwsh", "-NoProfile")]
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
    if ('{{anvil_container_image_source}}' -eq 'extend') {
        Write-Output "extends:    {{anvil_container_ext_dockerfile}}"
    }
    # NO_REBUILD turns the resolve into a pure query: report the state instead
    # of silently spending several minutes building from a status command.
    $env:ANVIL_CONTAINER_NO_REBUILD = '1'
    $image = (just _anvil-container-image 2>$null) | Select-Object -Last 1
    $present = $LASTEXITCODE -eq 0
    if ($image) { Write-Output "image:   $image" }
    if ($present) {
        Write-Output "status:  present and current"
    } elseif ('{{anvil_container_image_source}}' -eq 'extend') {
        # Which layer is missing decides how long the next run takes, so say
        # so: a missing base is a full toolchain build, a missing extension is
        # only the delta on top of one that is already cached.
        $layer = $image.StartsWith('{{anvil_container_ext_image_name}}:') ? 'extension' : 'base'
        Write-Output "status:  $layer image missing (run 'just anvil-container-up')"
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
[script("pwsh", "-NoProfile")]
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
