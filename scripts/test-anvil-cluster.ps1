# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

<#
.SYNOPSIS
    End-to-end test for the `[cluster]` Kind harness.

.DESCRIPTION
    Walks the path a repository takes to run integration tests against a real
    throwaway Kubernetes cluster:

      1. clean up      -- delete any cluster left by a previous run
      2. a repository  -- a crate, an image, a Helm chart, and an anvil.toml
                          declaring [[image]] and [cluster]
      3. generate      -- cargo anvil
      4. preflight     -- the host has kind, kubectl and helm
      5. cluster up    -- a real Kind cluster, and again to prove it is a no-op
      6. load          -- a locally built image is loaded into the cluster
      7. deploy        -- a chart runs a pod FROM THAT IMAGE with
                          imagePullPolicy: Never, so it can only start if the
                          load actually worked
      8. diagnostics   -- dumps cluster state without a namespace guess
      9. cluster down  -- the cluster is gone

    Step 7 is the claim worth testing. A pod with `imagePullPolicy: Never`
    cannot fall back to a registry, so it starts only if the image really is in
    the node's containerd image store -- which is what the load has to achieve
    and what silently regressed before.

    This is the slowest of the anvil e2e scripts: creating a Kind cluster takes
    a few minutes.

.PARAMETER WorkspacePath
    Where to create the throwaway repository.

.PARAMETER Keep
    Leave the workspace and the cluster in place for inspection.
#>

param(
    [Parameter(Mandatory = $false)]
    [string]$WorkspacePath = (Join-Path ([System.IO.Path]::GetTempPath()) "anvil-cluster-e2e"),

    [Parameter(Mandatory = $false)]
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
# Native command failures are handled explicitly below; do not fail fast.
$PSNativeCommandUseErrorActionPreference = $false

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$clusterName = 'anvil-e2e-kind'
$registry = 'anvile2e'
$script:failures = @()
$script:checks = 0

function Write-Step([string]$Message) {
    Write-Host ''
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Assert-That([object]$Condition, [string]$Message) {
    $script:checks++
    $passed = ($Condition -is [bool]) ? $Condition : [bool]$Condition
    if ($passed) {
        Write-Host "  [ok]   $Message"
    }
    else {
        Write-Host "  [FAIL] $Message"
        $script:failures += $Message
    }
}

# kind, kubectl and helm live wherever the container engine does. On a Windows
# host that means inside WSL, so the recipes run there against the /mnt path.
$script:engineMode = 'native'
$script:wslWorkspace = $null

function Initialize-EngineMode {
    if ((Get-Command 'docker' -ErrorAction SilentlyContinue) -and (Get-Command 'kind' -ErrorAction SilentlyContinue)) {
        & docker version *> $null
        if ($LASTEXITCODE -eq 0) { $script:engineMode = 'native'; return }
    }
    if ($IsWindows -and (Get-Command 'wsl' -ErrorAction SilentlyContinue)) {
        & wsl -e docker version *> $null
        if ($LASTEXITCODE -eq 0) {
            $script:engineMode = 'wsl'
            Write-Host '  container engine found inside WSL; recipes will run there'
            return
        }
    }
    throw 'no reachable container engine (looked for docker + kind on PATH, then inside WSL)'
}

function Get-WslWorkspace {
    if (-not $script:wslWorkspace) {
        $script:wslWorkspace = (& wsl -e wslpath -a $WorkspacePath.Replace('\', '/')).Trim()
    }
    return $script:wslWorkspace
}

function Invoke-Host {
    # Run a command where the engine lives, for the out-of-band assertions --
    # the recipes are what is under test, so verification must not go through
    # them.
    param([string]$Command)
    if ($script:engineMode -eq 'wsl') { return & wsl -e bash -lc $Command 2>$null }
    return & bash -lc $Command 2>$null
}

function Invoke-Just {
    param([string[]]$Arguments)

    $captured = $null
    if ($script:engineMode -eq 'wsl') {
        $joined = ($Arguments | ForEach-Object { "'$_'" }) -join ' '
        & wsl -e bash -lc "cd '$(Get-WslWorkspace)' && just $joined 2>&1" |
            Tee-Object -Variable captured |
            ForEach-Object { Write-Host "    $_" }
    }
    else {
        Push-Location $WorkspacePath
        try {
            & just @Arguments 2>&1 |
                Tee-Object -Variable captured |
                ForEach-Object { Write-Host "    $_" }
        }
        finally { Pop-Location }
    }
    return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = (@($captured) -join "`n") }
}

function Test-ClusterExists {
    $names = @(Invoke-Host 'kind get clusters')
    return ($names -contains $clusterName)
}

function Remove-TestCluster {
    if (Test-ClusterExists) {
        Invoke-Host "kind delete cluster --name $clusterName" | Out-Null
    }
    $ids = @(Invoke-Host "docker images '$registry/probe' -q") | Where-Object { $_ }
    if ($ids) { Invoke-Host "docker image rm -f $($ids -join ' ')" | Out-Null }
}

# ---------------------------------------------------------------------------
# 1. Clean up
# ---------------------------------------------------------------------------

Write-Step 'Cleaning up anything left by a previous run'

foreach ($tool in @('cargo', 'just', 'git')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "$tool is required but was not found on PATH"
    }
}
Initialize-EngineMode

if (Test-Path -LiteralPath $WorkspacePath) {
    Remove-Item -LiteralPath $WorkspacePath -Recurse -Force
    Write-Host "  removed $WorkspacePath"
}
Remove-TestCluster
Assert-That (-not (Test-ClusterExists)) 'starting with no cluster, as a new adopter would'

# ---------------------------------------------------------------------------
# 2. A repository with an image and a chart
# ---------------------------------------------------------------------------

Write-Step 'Creating a repository with an image and a chart'

& cargo build --quiet --package cargo-anvil --manifest-path (Join-Path $repoRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'failed to build cargo-anvil' }
$exe = Join-Path $repoRoot 'target/debug/cargo-anvil'
if (-not (Test-Path -LiteralPath $exe)) { $exe = "$exe.exe" }
if (-not (Test-Path -LiteralPath $exe)) { throw 'cargo-anvil binary not found after build' }

function Write-RepoFile([string]$RelativePath, [string]$Content) {
    # Always LF, always a trailing newline: this all runs on Linux.
    $normalized = ($Content -replace "`r`n", "`n")
    if (-not $normalized.EndsWith("`n")) { $normalized += "`n" }
    $path = Join-Path $WorkspacePath $RelativePath
    New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force | Out-Null
    [System.IO.File]::WriteAllText($path, $normalized)
}

$license = "# Copyright (c) Microsoft Corporation.`n# Licensed under the MIT License.`n`n"

Write-RepoFile 'Cargo.toml' ($license + @'
[package]
name = "anvil-cluster-e2e"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
'@)

Write-RepoFile 'src/lib.rs' @'
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Not the subject of the test; the cluster harness is.

/// Adds two numbers.
#[must_use]
pub const fn add(left: i32, right: i32) -> i32 {
    left + right
}
'@

Write-RepoFile 'Justfile' ($license + "import 'justfiles/anvil/mod.just'`n")

# The image the chart runs. It sleeps, so the pod stays Ready long enough to
# be waited on.
Write-RepoFile 'containers/probe/Dockerfile' @'
FROM docker.io/library/busybox:1.37.0
RUN printf 'anvil-cluster-e2e\n' > /etc/anvil-probe
ENTRYPOINT ["/bin/sh", "-c", "sleep 3600"]
'@

# A minimal Helm chart. `imagePullPolicy: Never` is the point: the pod cannot
# reach a registry, so it starts only if the image was really loaded into the
# node.
Write-RepoFile 'charts/probe/Chart.yaml' @'
apiVersion: v2
name: probe
description: A pod that can only start from a locally loaded image.
type: application
version: 0.1.0
appVersion: "0.1.0"
'@

Write-RepoFile 'charts/probe/values.yaml' @'
image:
  reference: anvile2e/probe:dev
'@

Write-RepoFile 'charts/probe/templates/deployment.yaml' @'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: probe
  labels:
    app: probe
spec:
  replicas: 1
  selector:
    matchLabels:
      app: probe
  template:
    metadata:
      labels:
        app: probe
    spec:
      containers:
        - name: probe
          image: {{ .Values.image.reference }}
          imagePullPolicy: Never
'@

Write-RepoFile 'anvil.toml' ($license + @"
image-output-dir = "out"

[container]
enabled = true
name = "anvil-cluster-e2e"

[[image]]
name = "probe"
dockerfile = "containers/probe/Dockerfile"
context = "out/probe"

[cluster]
name = "$clusterName"
load-images = ["probe"]

[[cluster.chart]]
name = "probe"
path = "charts/probe"
namespace = "anvil-e2e"
wait = ["deployment/probe"]

[cluster.diagnostics]
namespace = "anvil-e2e"
resources = ["pods -n anvil-e2e", "deployments -n anvil-e2e"]
logs = ["deployment/probe"]
"@)

Push-Location $WorkspacePath
try {
    & git init --quiet
    Write-Step 'Generating the anvil recipes'
    & $exe anvil --no-backends | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'cargo anvil failed' }
}
finally {
    Pop-Location
}

Assert-That (Test-Path -LiteralPath (Join-Path $WorkspacePath 'justfiles/anvil/cluster.just')) `
    'the cluster recipes were generated'
Assert-That (Test-Path -LiteralPath (Join-Path $WorkspacePath 'justfiles/anvil/cluster-bootstrap.just')) `
    'the cluster bootstrap recipes were generated'

# ---------------------------------------------------------------------------
# 3. Preflight
# ---------------------------------------------------------------------------

Write-Step 'Running preflight (expect the host tooling to be found)'

$preflight = Invoke-Just -Arguments @('anvil-cluster-preflight')
Assert-That ($preflight.ExitCode -eq 0) 'preflight passes on a host with kind, kubectl and helm'

Write-Step 'Running bootstrap (expect the host limits to be tuned)'

# Not optional on WSL. Kind node containers need far more inotify instances
# than the WSL default of 128, and without this the control plane comes up
# unhealthy with an error that blames cgroups rather than the real cause.
$bootstrap = Invoke-Just -Arguments @('anvil-cluster-bootstrap')
Assert-That ($bootstrap.ExitCode -eq 0) 'bootstrap succeeds'

$instances = [int](((Invoke-Host 'cat /proc/sys/fs/inotify/max_user_instances') -join '').Trim())
Assert-That ($instances -ge 1024) "bootstrap raised the inotify instance limit (now $instances)"

# ---------------------------------------------------------------------------
# 4. Bring the cluster up
# ---------------------------------------------------------------------------

Write-Step 'Creating the cluster (expect a real Kind cluster)'

$up = Invoke-Just -Arguments @('anvil-cluster-up')
Assert-That ($up.ExitCode -eq 0) 'the cluster comes up'
Assert-That (Test-ClusterExists) 'kind reports the cluster exists'

# `kind create` returns once the API server answers; the node reports Ready a
# little later, so wait rather than sampling and calling it a failure.
Invoke-Host "kubectl --context kind-$clusterName wait --for=condition=Ready node --all --timeout=180s" | Out-Null
$nodeReady = (Invoke-Host "kubectl --context kind-$clusterName get nodes --no-headers") -join "`n"
Assert-That ($nodeReady -match '\bReady\b') 'the control-plane node reaches Ready'

Write-Step 'Creating it again (expect a no-op, not a second cluster)'

$again = Invoke-Just -Arguments @('anvil-cluster-up')
Assert-That ($again.ExitCode -eq 0) 'bringing an existing cluster up succeeds'
Assert-That ($again.Output -match 'already exists') 'the second attempt reports the cluster already exists'

# ---------------------------------------------------------------------------
# 5. Load a locally built image
# ---------------------------------------------------------------------------

Write-Step 'Building and loading the image (expect it in the node image store)'

$built = Invoke-Just -Arguments @('anvil-image', 'probe', 'debug', 'dev', $registry)
Assert-That ($built.ExitCode -eq 0) 'the image builds'

$load = Invoke-Just -Arguments @('anvil-cluster-load', 'debug', 'dev', $registry)
Assert-That ($load.ExitCode -eq 0) 'loading the image into the cluster succeeds'

# Ask containerd directly. `kind load` writing to the wrong image store is the
# regression this guards, and it is invisible from the host's docker.
$inNode = (Invoke-Host "docker exec ${clusterName}-control-plane ctr --namespace=k8s.io images ls -q") -join "`n"
Assert-That ($inNode -match 'probe') 'the image is in the node containerd k8s.io namespace'

# ---------------------------------------------------------------------------
# 6. Deploy the chart
# ---------------------------------------------------------------------------

Write-Step 'Deploying the chart (expect a pod running the loaded image)'

$deploy = Invoke-Just -Arguments @('anvil-cluster-deploy', 'debug', 'dev', $registry)
Assert-That ($deploy.ExitCode -eq 0) 'the chart deploys and its wait target resolves'

$pods = (Invoke-Host "kubectl --context kind-$clusterName -n anvil-e2e get pods --no-headers") -join "`n"
Assert-That ($pods -match '\bRunning\b') 'the pod is Running'

# imagePullPolicy: Never means this can only be true if the load worked.
Assert-That ($pods -notmatch 'ErrImageNeverPull|ImagePullBackOff') `
    'the pod started from the loaded image rather than failing to pull'

# The chart declared a namespace; a release that ignored it would have landed
# in `default` and the wait target would have resolved against the wrong one.
$release = (Invoke-Host "helm --kube-context kind-$clusterName -n anvil-e2e list --short") -join "`n"
Assert-That ($release -match 'probe') 'the release landed in the namespace the chart declared'

# ---------------------------------------------------------------------------
# 7. Diagnostics
# ---------------------------------------------------------------------------

Write-Step 'Dumping diagnostics (expect cluster state, not a namespace guess)'

$diagnostics = Invoke-Just -Arguments @('anvil-cluster-diagnostics')
Assert-That ($diagnostics.ExitCode -eq 0) 'diagnostics succeed'
Assert-That ($diagnostics.Output -match 'kubectl get pods') 'diagnostics dump the configured resources'
Assert-That ($diagnostics.Output -match 'probe') 'diagnostics mention the deployed workload'
# The logs target is a bare `deployment/probe`; it resolves only because the
# configured namespace is applied to it. Without that it would be looked up in
# `default` and the dump would be empty.
Assert-That ($diagnostics.Output -match 'logs deployment/probe') `
    'the log target resolved against the configured namespace'

# ---------------------------------------------------------------------------
# 8. Tear the cluster down
# ---------------------------------------------------------------------------

Write-Step 'Deleting the cluster (expect it to be gone)'

$down = Invoke-Just -Arguments @('anvil-cluster-down')
Assert-That ($down.ExitCode -eq 0) 'the cluster comes down'
Assert-That (-not (Test-ClusterExists)) 'kind no longer reports the cluster'

$downAgain = Invoke-Just -Arguments @('anvil-cluster-down')
Assert-That ($downAgain.ExitCode -eq 0) 'deleting an absent cluster is not an error'

# ---------------------------------------------------------------------------
# 9. Clean up
# ---------------------------------------------------------------------------

if (-not $Keep) {
    Write-Step 'Cleaning up'
    Remove-TestCluster
    if (Test-Path -LiteralPath $WorkspacePath) {
        Remove-Item -LiteralPath $WorkspacePath -Recurse -Force
    }
    Write-Host '  removed the workspace, the cluster and the image'
}
else {
    Write-Step "Leaving $WorkspacePath and the cluster in place"
}

Write-Host ''
if ($script:failures.Count -gt 0) {
    Write-Host "$($script:failures.Count) of $($script:checks) checks FAILED:" -ForegroundColor Red
    $script:failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host "all $($script:checks) checks passed" -ForegroundColor Green
exit 0
