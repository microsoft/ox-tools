# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md
#
# Host bootstrap + preflight for the Kind cluster harness. Emitted only when
# `[container] enabled = true` and a `[cluster]` section is
# configured in anvil.toml.
#
# Docker-in-WSL is the supported engine. Podman may be detected best-effort by
# the shared engine abstraction, but there are deliberately no podman-specific
# recipe variants — the single `engine` knob covers it.

# Pinned, checksum-verified tool versions (Linux amd64).
kind_version := "v0.24.0"
kind_sha256 := "b89aada5a39d620da3fcd16435b7f28d858927dd53f92cbac77686b0588b600d"
kubectl_version := "v1.31.0"
kubectl_sha256 := "7c27adc64a84d1c0cc3dcf7bf4b6e916cc00f3f576a2dbac51b318d926032437"
helm_version := "v3.16.0"
helm_sha256 := "327cfbc7ddc5a3eb644039ceb0cff66394628654c4f5a76bf715ed15b893983b"

# Verify the engine/daemon and kind/kubectl/helm are present. Installs nothing.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-preflight:
    $ErrorActionPreference = 'Stop'
    $missing = @()
    $engine = $null
    foreach ($c in @('docker', 'podman')) {
        if (Get-Command $c -ErrorAction SilentlyContinue) { $engine = $c; break }
    }
    if ($null -eq $engine) {
        $missing += 'a container engine (docker or podman) — install Docker in WSL'
    } else {
        & $engine info *> $null
        if ($LASTEXITCODE -ne 0) {
            $missing += "$engine is installed but its daemon is not reachable — start the engine"
        }
    }
    foreach ($tool in @('kind', 'kubectl', 'helm')) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            $missing += "$tool not on PATH — run 'just anvil-cluster-bootstrap'"
        }
    }
    if ($missing.Count -gt 0) {
        Write-Error ("preflight failed:`n  - " + ($missing -join "`n  - "))
        exit 1
    }
    Write-Output "anvil: preflight OK (engine=$engine; kind, kubectl, helm present)"
    exit 0

# Install pinned kind/kubectl/helm and tune WSL limits. Linux/WSL only.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-bootstrap:
    $ErrorActionPreference = 'Stop'
    if ($IsWindows) {
        Write-Error "anvil-cluster-bootstrap installs Linux tooling; run it inside WSL / the Linux engine host"
        exit 1
    }
    # Prefer a directory already on PATH so the tools are usable immediately.
    # ~/.local/bin is the correct non-root fallback, but it is not on the
    # default PATH for a root shell, which silently leaves the tools invisible
    # right after a successful bootstrap.
    $bin = '/usr/local/bin'
    if (-not (Test-Path $bin) -or -not (& sh -c 'test -w /usr/local/bin && echo writable')) {
        $bin = Join-Path $HOME '.local/bin'
    }
    $onPath = ($env:PATH -split ':') -contains $bin
    New-Item -ItemType Directory -Force -Path $bin | Out-Null

    function Install-Pinned($name, $url, $sha256, $dest, $isArchive, $archiveMember) {
        if (Get-Command $name -ErrorAction SilentlyContinue) {
            Write-Output "anvil: $name already installed; skipping"
            return
        }
        $tmp = New-TemporaryFile
        Write-Output "anvil: downloading $name from $url"
        curl -sSL -o $tmp $url
        if ($LASTEXITCODE -ne 0) { throw "download failed for $name" }
        $actual = (Get-FileHash -Algorithm SHA256 -Path $tmp).Hash.ToLower()
        if ($actual -ne $sha256) { throw "$name checksum mismatch: expected $sha256, got $actual" }
        if ($isArchive) {
            $extract = Join-Path ([System.IO.Path]::GetTempPath()) "anvil-$name"
            New-Item -ItemType Directory -Force -Path $extract | Out-Null
            tar -xzf $tmp -C $extract
            Copy-Item -Force (Join-Path $extract $archiveMember) $dest
        } else {
            Copy-Item -Force $tmp $dest
        }
        chmod +x $dest
        Remove-Item -Force $tmp -ErrorAction SilentlyContinue
        Write-Output "anvil: installed $name -> $dest"
    }

    Install-Pinned 'kind' "https://github.com/kubernetes-sigs/kind/releases/download/{{kind_version}}/kind-linux-amd64" '{{kind_sha256}}' (Join-Path $bin 'kind') $false ''
    Install-Pinned 'kubectl' "https://dl.k8s.io/release/{{kubectl_version}}/bin/linux/amd64/kubectl" '{{kubectl_sha256}}' (Join-Path $bin 'kubectl') $false ''
    Install-Pinned 'helm' "https://get.helm.sh/helm-{{helm_version}}-linux-amd64.tar.gz" '{{helm_sha256}}' (Join-Path $bin 'helm') $true 'linux-amd64/helm'

    # WSL tuning: Kind needs generous inotify limits; the low WSL defaults make
    # node containers hang with 'too many open files'.
    if (Test-Path '/proc/sys/fs/inotify/max_user_instances') {
        sudo sysctl -w fs.inotify.max_user_instances=8192 *> $null
        sudo sysctl -w fs.inotify.max_user_watches=1048576 *> $null
    }
    # Kind's kube-proxy needs bridged traffic to traverse iptables.
    $bridge = (Get-Content /proc/sys/net/bridge/bridge-nf-call-iptables -ErrorAction SilentlyContinue)
    if ($bridge -ne '1') {
        Write-Warning "bridge-nf-call-iptables is not 1; load br_netfilter and set net.bridge.bridge-nf-call-iptables=1"
    }
    if ($onPath) {
        Write-Output "anvil: bootstrap complete; tools installed to $bin"
    } else {
        Write-Output "anvil: bootstrap complete; tools installed to $bin"
        Write-Output "anvil: $bin is NOT on PATH — add it with:"
        Write-Output "         export PATH=`"`$PATH:$bin`""
    }
    exit 0
