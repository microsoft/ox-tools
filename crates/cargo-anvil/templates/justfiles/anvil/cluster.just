# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
# Managed by cargo-anvil. Update the corresponding template in the cargo-anvil
# crate unless the change is repository-specific.
# Update behaviour: https://github.com/microsoft/ox-tools/blob/main/crates/cargo-anvil/docs/design/updates.md
#
# Generic Kind cluster harness. Emitted only when `[container] enabled = true`
# and a `[cluster]` section is configured in anvil.toml.
#
# Every value below is baked from anvil.toml at generation time. The recipes
# implement a set of hard-won workarounds for running Kind as a sibling
# container on a host engine (docker-outside-of-docker), for rootless podman,
# and for Kind node containerd inheriting an unreachable DNS proxy. Do not
# assume a plain local Kind — the ladders and repairs are deliberate.
#
# Shared recipe parameters:
#   profile / tag / registry  match the image build (see container-images.just)
#                             so the refs loaded here equal the refs built.
#
# Extension hooks: when a hook name is configured it is invoked as a plain
# `just <name>` recipe at the relevant point, so a repository can inject
# bespoke wiring (issuer discovery, extra --set flags, …) without anvil ever
# modelling what it does. Absent hook = no invocation.
#
# The `_anvil-container-engine` helper is provided by container.just.

anvil_cluster_name := "__CLUSTER_NAME__"

# Create the cluster if absent, then repair the apiserver endpoint. Idempotent.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-up profile="debug" tag="dev" registry="local":
    $ErrorActionPreference = 'Stop'
    $cluster = '{{anvil_cluster_name}}'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($engine -eq 'podman') { $env:KIND_EXPERIMENTAL_PROVIDER = 'podman' }

    function Test-ClusterExists($engine, $cluster) {
        $names = @(kind get clusters 2>$null)
        if ($LASTEXITCODE -eq 0 -and ($names -contains $cluster)) { return $true }
        # Label fallback: rootless / podman cannot always be listed by `kind
        # get clusters`, so look for the node containers by Kind's own label.
        $ids = @(& $engine ps -a --filter "label=io.x-k8s.kind.cluster=$cluster" -q 2>$null)
        return $ids.Count -gt 0
    }

    if (Test-ClusterExists $engine $cluster) {
        Write-Output "anvil: cluster '$cluster' already exists"
    } else {
        $nodeImage = '__NODE_IMAGE__'
        $configLines = @('kind: Cluster', 'apiVersion: kind.x-k8s.io/v1alpha4', 'nodes:', '  - role: control-plane')
        if ($nodeImage -ne '') { $configLines += "    image: $nodeImage" }
        for ($i = 0; $i -lt __WORKERS__; $i++) {
            $configLines += '  - role: worker'
            if ($nodeImage -ne '') { $configLines += "    image: $nodeImage" }
        }
        $configPath = Join-Path ([System.IO.Path]::GetTempPath()) "anvil-kind-$cluster.yaml"
        ($configLines -join "`n") | Set-Content -Path $configPath -Encoding ascii
        kind create cluster --name $cluster --config $configPath
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }

    # Docker-outside-of-docker apiserver repair: when we are a sibling
    # container on the host engine, the default kubeconfig endpoint
    # (127.0.0.1:PORT) is unreachable. Connect ourselves to the `kind`
    # network and re-export the kubeconfig with the container-internal
    # endpoint.
    kubectl cluster-info --context "kind-$cluster" *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Output "anvil: apiserver endpoint unreachable; attempting docker-outside-of-docker repair"
        $self = $env:HOSTNAME
        if ([string]::IsNullOrEmpty($self)) { $self = [System.Net.Dns]::GetHostName() }
        & $engine network connect kind $self *> $null
        kind export kubeconfig --name $cluster --internal
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    Write-Output "anvil: cluster '$cluster' is up"
    exit 0

# Delete the cluster. Idempotent — a missing cluster is not an error.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-down:
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($engine -eq 'podman') { $env:KIND_EXPERIMENTAL_PROVIDER = 'podman' }
    kind delete cluster --name '{{anvil_cluster_name}}'
    exit 0

# Load configured and dependency images into the cluster via the load ladder.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-load profile="debug" tag="dev" registry="local":
    $ErrorActionPreference = 'Stop'
    $cluster = '{{anvil_cluster_name}}'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($engine -eq 'podman') { $env:KIND_EXPERIMENTAL_PROVIDER = 'podman' }

    function Resolve-Ref($engine, $registry, $name, $tag) {
        $ref = "${registry}/${name}:${tag}"
        # podman qualifies an unqualified "local/..." ref as "localhost/local/...".
        if ($engine -eq 'podman' -and $registry -notmatch '[.:/]' -and $registry -ne 'localhost') {
            $ref = "localhost/$ref"
        }
        return $ref
    }

    # Image load ladder: (1) kind load docker-image; on failure (2) save to an
    # OCI archive and kind load image-archive; on failure (3) copy the archive
    # to each node and ctr import under the k8s.io namespace.
    #
    # `--platform` matters on rung 2: with the containerd image store a pulled
    # multi-arch image is kept as an index carrying attestation manifests whose
    # content is not all present locally, and the resulting archive fails to
    # import with "content digest not found". Selecting one platform produces a
    # flat, importable archive. Engines that predate the flag fall back to a
    # plain save.
    function Import-Image($engine, $cluster, $ref) {
        kind load docker-image --name $cluster $ref
        if ($LASTEXITCODE -eq 0) { return }
        Write-Warning "kind load docker-image failed for $ref; falling back to image-archive"
        $safe = ($ref -replace '[^A-Za-z0-9]', '_')
        $archive = Join-Path ([System.IO.Path]::GetTempPath()) "anvil-$safe.tar"
        & $engine save --platform linux/amd64 -o $archive $ref 2>$null
        if ($LASTEXITCODE -ne 0) { & $engine save -o $archive $ref }
        if ($LASTEXITCODE -eq 0 -and (Test-Path $archive)) {
            kind load image-archive --name $cluster $archive
            if ($LASTEXITCODE -eq 0) { Remove-Item -Force $archive -ErrorAction SilentlyContinue; return }
        }
        if (-not (Test-Path $archive)) {
            throw "cannot load ${ref}: '$engine save' produced no archive, so there is nothing to import into the nodes"
        }
        Write-Warning "kind load image-archive failed for $ref; falling back to per-node ctr import"
        $nodes = @(kind get nodes --name $cluster)
        if ($LASTEXITCODE -ne 0 -or $nodes.Count -eq 0) {
            throw "cannot load ${ref}: no nodes found for cluster '$cluster', so the image would silently not be imported"
        }
        foreach ($node in $nodes) {
            & $engine cp $archive "${node}:/tmp/anvil-image.tar"
            if ($LASTEXITCODE -ne 0) { throw "failed to copy $ref archive into node $node" }
            & $engine exec $node ctr --namespace=k8s.io images import /tmp/anvil-image.tar
            if ($LASTEXITCODE -ne 0) { throw "failed to import $ref into node $node" }
        }
        Remove-Item -Force $archive -ErrorAction SilentlyContinue
    }

    foreach ($name in __LOAD_IMAGES_ARRAY__) {
        $ref = Resolve-Ref $engine '{{registry}}' $name '{{tag}}'
        Write-Output "anvil: loading $ref"
        Import-Image $engine $cluster $ref
    }

    # Dependency preload images can inherit an unreachable WSL DNS proxy inside
    # Kind node containerd, so pull them on the host and kind load them.
    # Pull one platform explicitly: a multi-arch pull lands as an index that
    # `kind load docker-image` cannot consume, forcing the slower archive path.
    foreach ($dep in __DEPENDENCIES_PWSH__) {
        foreach ($img in $dep.preload) {
            Write-Output "anvil: pre-pulling $img for dependency $($dep.name)"
            & $engine pull --platform linux/amd64 $img 2>$null
            if ($LASTEXITCODE -ne 0) { & $engine pull $img }
            if ($LASTEXITCODE -ne 0) { Write-Warning "pull failed for $img (continuing)"; continue }
            Import-Image $engine $cluster $img
        }
    }
    exit 0

# Apply dependencies then install the repo's charts. CRDs are applied
# server-side BEFORE `helm upgrade --install` because Helm skips CRDs on
# upgrade. Invokes the pre-install and post-install hooks around the charts.

# Apply dependencies and install the repo's charts, CRDs first.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-deploy profile="debug" tag="dev" registry="local":
    $ErrorActionPreference = 'Stop'
    $repoRoot = '{{justfile_directory()}}'
    $cluster = '{{anvil_cluster_name}}'
    $ctx = "kind-$cluster"
    $profile = '{{profile}}'
    $tag = '{{tag}}'
    $registry = '{{registry}}'
    # `{registry}` matters here as much as the other two: a chart that pins an
    # image reference has to name the registry the images were actually loaded
    # under, or the pod pulls a reference nothing built.
    function Expand-Tokens($s) { $s.Replace('{profile}', $profile).Replace('{tag}', $tag).Replace('{registry}', $registry) }
    function Wait-Targets($ctx, $targets, $namespace) {
        foreach ($t in $targets) {
            $nsArgs = @(); if ($namespace -ne '') { $nsArgs = @('-n', $namespace) }
            kubectl --context $ctx @nsArgs rollout status $t --timeout=300s
            if ($LASTEXITCODE -ne 0) { throw "rollout wait failed for $t" }
        }
    }

    $preInstall = '__HOOK_PRE_INSTALL__'
    if ($preInstall -ne '') {
        Write-Output "anvil: pre-install hook '$preInstall'"
        just $preInstall
        if ($LASTEXITCODE -ne 0) { throw "pre-install hook '$preInstall' failed" }
    }

    foreach ($dep in __DEPENDENCIES_PWSH__) {
        Write-Output "anvil: applying dependency $($dep.name) $($dep.version)"
        kubectl --context $ctx apply -f $dep.manifest
        if ($LASTEXITCODE -ne 0) { throw "failed to apply dependency $($dep.name)" }
        Wait-Targets $ctx $dep.wait $dep.namespace
    }

    foreach ($chart in __CHARTS_PWSH__) {
        $ns = $chart.namespace
        if ($chart.crds -ne '') {
            Write-Output "anvil: applying CRDs for chart $($chart.name)"
            kubectl --context $ctx apply --server-side -f (Join-Path $repoRoot $chart.crds)
            if ($LASTEXITCODE -ne 0) { throw "failed to apply CRDs for $($chart.name)" }
        }
        $helmArgs = @('upgrade', '--install', $chart.name, (Join-Path $repoRoot $chart.path), '--kube-context', $ctx)
        if ($ns -ne '') { $helmArgs += @('--namespace', $ns, '--create-namespace') }
        foreach ($kv in $chart.set) { $helmArgs += @('--set', "$($kv.k)=$(Expand-Tokens $kv.v)") }
        $helmArgs += '--wait'
        Write-Output "anvil: helm upgrade --install $($chart.name)"
        helm @helmArgs
        if ($LASTEXITCODE -ne 0) { throw "helm install failed for $($chart.name)" }
        Wait-Targets $ctx $chart.wait $ns
    }

    $postInstall = '__HOOK_POST_INSTALL__'
    if ($postInstall -ne '') {
        Write-Output "anvil: post-install hook '$postInstall'"
        just $postInstall
        if ($LASTEXITCODE -ne 0) { throw "post-install hook '$postInstall' failed" }
    }
    exit 0

# Dump diagnostic resources and deployment logs. Best-effort, never fails.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-diagnostics:
    $ErrorActionPreference = 'Continue'
    # Via a single-quoted assignment, not straight into a double-quoted
    # literal: the latter would make `$` and backticks in the name live.
    $cluster = '{{anvil_cluster_name}}'
    $ctx = "kind-$cluster"
    foreach ($r in __DIAG_RESOURCES_ARRAY__) {
        Write-Output "=== kubectl get $r ==="
        $parts = $r -split '\s+'
        kubectl --context $ctx get @parts
    }
    $diagNs = @(); if ('__DIAG_NAMESPACE__' -ne '') { $diagNs = @('-n', '__DIAG_NAMESPACE__') }
    foreach ($d in __DIAG_LOGS_ARRAY__) {
        Write-Output "=== logs $d ==="
        kubectl --context $ctx @diagNs logs $d --all-containers --tail=200
    }
    exit 0

# Full flow with bounded retries: up -> load -> deploy -> pre-test hook ->
# readiness re-check. On failure, run the on-failure hook and dump
# diagnostics, wait the configured delay, and retry. A retry that eventually
# passes emits a FLAKY warning.

# Full cluster flow with bounded retries and flaky reporting.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-test profile="debug" tag="dev" registry="local":
    $ErrorActionPreference = 'Stop'
    $cluster = '{{anvil_cluster_name}}'
    $attempts = __RETRY_ATTEMPTS__
    $delay = __RETRY_DELAY__
    $preTest = '__HOOK_PRE_TEST__'
    $onFailure = '__HOOK_ON_FAILURE__'
    $attempt = 0
    $passed = $false
    while ($attempt -lt $attempts) {
        $attempt++
        Write-Output "anvil: cluster test attempt $attempt of $attempts"
        try {
            just anvil-cluster-up '{{profile}}' '{{tag}}' '{{registry}}'
            if ($LASTEXITCODE -ne 0) { throw "cluster up failed" }
            just anvil-cluster-load '{{profile}}' '{{tag}}' '{{registry}}'
            if ($LASTEXITCODE -ne 0) { throw "cluster load failed" }
            just anvil-cluster-deploy '{{profile}}' '{{tag}}' '{{registry}}'
            if ($LASTEXITCODE -ne 0) { throw "cluster deploy failed" }
            if ($preTest -ne '') {
                just $preTest
                if ($LASTEXITCODE -ne 0) { throw "pre-test hook '$preTest' failed" }
            }
            kubectl --context "kind-$cluster" get nodes *> $null
            if ($LASTEXITCODE -ne 0) { throw "readiness re-check failed" }
            $passed = $true
            break
        } catch {
            Write-Warning "attempt $attempt failed: $_"
            if ($onFailure -ne '') { just $onFailure }
            just anvil-cluster-diagnostics
            if ($attempt -lt $attempts) { Start-Sleep -Seconds $delay }
        }
    }
    if (-not $passed) {
        Write-Error "cluster test failed after $attempts attempt(s)"
        exit 1
    }
    if ($attempt -gt 1) { Write-Warning "FLAKY: cluster test passed on attempt $attempt of $attempts" }
    Write-Output "anvil: cluster test passed"
    exit 0

# Tear down: `cluster` deletes the Kind cluster, `full` also removes built images.
[group("anvil-cluster")]
[script("pwsh")]
anvil-cluster-clean mode="cluster" tag="dev" registry="local":
    $ErrorActionPreference = 'Stop'
    $engine = just _anvil-container-engine
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if ($engine -eq 'podman') { $env:KIND_EXPERIMENTAL_PROVIDER = 'podman' }
    kind delete cluster --name '{{anvil_cluster_name}}'
    if ('{{mode}}' -eq 'full') {
        foreach ($name in __LOAD_IMAGES_ARRAY__) {
            $ref = "{{registry}}/${name}:{{tag}}"
            & $engine rmi -f $ref *> $null
        }
        Write-Output "anvil: removed locally built images"
    }
    exit 0
