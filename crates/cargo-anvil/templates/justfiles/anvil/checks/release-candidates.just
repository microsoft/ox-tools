# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Emit publishable new or version-changed workspace packages as JSON.
[group("anvil")]
[script("pwsh", "-NoProfile")]
anvil-release-candidates:
    $ErrorActionPreference = 'Stop'
    $PSNativeCommandUseErrorActionPreference = $false
    Set-Location -LiteralPath '{{ replace(justfile_directory(), "'", "''") }}'
    $base = '{{ replace(anvil_base_ref, "'", "''") }}'
    if (-not ($env:BASE_REF -or $env:SYSTEM_PULLREQUEST_TARGETBRANCH -or $env:GITHUB_BASE_REF)) {
        git show-ref --verify --quiet refs/remotes/origin/main
        if ($LASTEXITCODE -eq 1) { $base = 'origin/master' }
    }
    $baseCommit = git rev-parse --verify --end-of-options "$base^{commit}"
    if ($LASTEXITCODE -ne 0) { throw "Cannot resolve baseline '$base'. Set BASE_REF to an available commit or fetch the required history." }
    $repository = git rev-parse --show-toplevel
    if ($LASTEXITCODE -ne 0) { throw 'Cannot locate the Git checkout.' }

    $stableArgs = @({{ anvil_stable_toolchain_arg }})
    $sysroot = & rustc @stableArgs --print sysroot
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $env:RUSTUP_TOOLCHAIN = $sysroot.Trim()
    $metadataArgs = @('metadata', '--no-deps', '--format-version', '1', '--locked')
    $currentJson = cargo @metadataArgs
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read current workspace metadata.' }
    $current = $currentJson | ConvertFrom-Json
    $manifest = [IO.Path]::GetRelativePath($repository, (Join-Path $current.workspace_root 'Cargo.toml'))
    if ([IO.Path]::IsPathRooted($manifest) -or $manifest.StartsWith("..$([IO.Path]::DirectorySeparatorChar)")) {
        throw "The workspace manifest must be inside the Git checkout '$repository'."
    }

    $tmpRoot = $env:RUNNER_TEMP
    if (-not $tmpRoot) { $tmpRoot = $env:AGENT_TEMPDIRECTORY }
    if (-not $tmpRoot) { $tmpRoot = [IO.Path]::GetTempPath() }
    $worktree = Join-Path $tmpRoot "anvil-release-$([Guid]::NewGuid().ToString('N'))"
    $baselineManifest = Join-Path $worktree $manifest
    $env:GIT_LFS_SKIP_SMUDGE = '1'
    $oldVersions = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
    $added = $false
    $readBaseline = $false
    try {
        git -C $repository worktree add --quiet --detach $worktree $baseCommit | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Cannot check out baseline '$base' at '$worktree'." }
        $added = $true
        if (Test-Path -LiteralPath $baselineManifest -PathType Leaf) {
            Push-Location -LiteralPath (Split-Path -Parent $baselineManifest)
            try {
                $baselineJson = cargo @metadataArgs
                if ($LASTEXITCODE -ne 0) { throw "Cannot read workspace metadata at baseline '$base'." }
                foreach ($package in ($baselineJson | ConvertFrom-Json).packages) {
                    $oldVersions.Add($package.name, $package.version)
                }
            } finally {
                Pop-Location
            }
        }
        $readBaseline = $true
    } finally {
        try {
            if (-not $added) {
                # Checkout can fail after Git registers the worktree.
                $listed = git -C $repository worktree list --porcelain -z
                if ($LASTEXITCODE -ne 0) { throw 'Cannot list Git worktrees.' }
                $added = ($listed -join "`n").Split([char] 0) -ccontains "worktree $($worktree.Replace('\', '/'))"
            }
            if ($added) {
                git -C $repository worktree remove --force --force $worktree | Out-Null
                if ($LASTEXITCODE -ne 0) { throw 'git worktree remove failed.' }
            } elseif ([IO.Directory]::Exists($worktree)) {
                [IO.Directory]::Delete($worktree, $true)
            }
        } catch {
            $message = "Cannot clean up baseline worktree '$worktree': $($_.Exception.Message)"
            if ($readBaseline) { throw $message }
            [Console]::Error.WriteLine($message)
        }
    }

    $candidates = @(foreach ($package in $current.packages) {
        if ($null -ne $package.publish -and $package.publish.Count -eq 0) { continue }
        $oldVersion = $null
        if ($oldVersions.ContainsKey($package.name)) {
            $oldVersion = $oldVersions[$package.name]
            $old = ($oldVersion -split '\+', 2)[0]
            $new = ($package.version -split '\+', 2)[0]
            if ($old -ceq $new) { continue }

            $oldCoreText, $oldPrerelease = $old -split '-', 2
            $newCoreText, $newPrerelease = $new -split '-', 2
            $hasPrerelease = $oldPrerelease -or $newPrerelease
            $oldCore = $oldCoreText.Split('.')
            $newCore = $newCoreText.Split('.')
            $bump = 'prerelease'
            $order = 0
            for ($i = 0; $i -lt 3; $i++) {
                $order = ([uint64] $newCore[$i]).CompareTo([uint64] $oldCore[$i])
                if ($order -ne 0) {
                    $bump = @('major', 'minor', 'patch')[$i]
                    break
                }
            }
            if ($order -lt 0 -and -not $hasPrerelease) {
                throw "Package '$($package.name)' decreases from $oldVersion to $($package.version) against baseline '$base'. Increase the version or choose the intended BASE_REF."
            }
            $breaking = $hasPrerelease -or $bump -eq 'major' -or
                ($oldCore[0] -eq '0' -and ($bump -eq 'minor' -or $oldCore[1] -eq '0'))
            $classification = if ($breaking) { 'breaking' } else { 'compatible' }
        } else {
            $classification = 'new'
        }
        [pscustomobject] @{
            name = $package.name
            manifest_path = [IO.Path]::GetRelativePath($current.workspace_root, $package.manifest_path).Replace('\', '/')
            old_version = $oldVersion
            new_version = $package.version
            classification = $classification
        }
    })
    ConvertTo-Json -InputObject $candidates
