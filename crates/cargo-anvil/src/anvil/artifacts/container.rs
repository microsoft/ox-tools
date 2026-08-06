// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Plan-time rendering for the opt-in container-execution surface.
//!
//! When a repository enables container mode in `anvil.toml`, the generated
//! recipe tree gains a shim (`justfiles/anvil/container.just`) and the tier
//! and group recipes gain a re-entry guard that runs the same `just` target
//! inside the container. All of this is applied at plan time from the
//! [`ResolvedContainer`] settings, so the artifact bodies stored in the
//! catalog stay repository-independent (the catalog checksum is stable) and
//! an absent / disabled config is byte-for-byte identical to a no-container
//! build.
//!
//! The token-substitution approach mirrors
//! [`super::github::render_group_action`]: templates carry `__TOKEN__`
//! placeholders that are replaced here with the resolved settings.

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::catalog::Artifact;
use crate::config::{ClusterChart, ClusterConfig, ClusterDependency, ImageSpec, NativeWhen, ResolvedContainer};

/// Repo-root-relative path of the container shim recipe file.
pub(crate) const CONTAINER_JUST_PATH: &str = "justfiles/anvil/container.just";

/// Repo-root-relative path of the emitted devcontainer descriptor.
pub(crate) const DEVCONTAINER_PATH: &str = ".devcontainer/devcontainer.json";

/// Repo-root-relative path of the generic OCI image build recipes (pillar 2).
pub(crate) const CONTAINER_IMAGES_JUST_PATH: &str = "justfiles/anvil/container-images.just";

/// Repo-root-relative path of the Kind cluster harness recipes (pillar 3).
pub(crate) const CLUSTER_JUST_PATH: &str = "justfiles/anvil/cluster.just";

/// Repo-root-relative path of the cluster host bootstrap + preflight recipes.
pub(crate) const CLUSTER_BOOTSTRAP_JUST_PATH: &str = "justfiles/anvil/cluster-bootstrap.just";

/// Path of the recipe-tree entry point (kept in sync with
/// [`super::justfile`]).
const MOD_JUST_PATH: &str = "justfiles/anvil/mod.just";

/// Path of the tier aggregator file.
const TIERS_JUST_PATH: &str = "justfiles/anvil/tiers.just";

/// Prefix common to every group recipe file.
const GROUPS_DIR_PREFIX: &str = "justfiles/anvil/groups/";

/// GitHub reusable-workflow files (job-level `container:` targets).
const GH_PR_IMPL_PATH: &str = ".github/workflows/anvil-pr-impl.yml";
const GH_SCHEDULED_IMPL_PATH: &str = ".github/workflows/anvil-scheduled-impl.yml";

/// GitHub root workflows (forward `container_image` into the reusable call).
const GH_PR_ROOT_PATH: &str = ".github/workflows/anvil-pr.yml";
const GH_SCHEDULED_ROOT_PATH: &str = ".github/workflows/anvil-scheduled.yml";

/// ADO job wrapper (adds a `container` parameter + `ANVIL_IN_CONTAINER`).
const ADO_JOB_PATH: &str = ".pipelines/anvil/steps/job.yml";

/// ADO root pipelines (declare the `resources.containers` entry).
const ADO_PR_ROOT_PATH: &str = ".pipelines/anvil-pr.yml";
const ADO_SCHEDULED_ROOT_PATH: &str = ".pipelines/anvil-scheduled.yml";

/// The three tier recipes that receive the re-entry guard.
const TIER_RECIPES: &[&str] = &["anvil-pr", "anvil-scheduled", "anvil-full"];

/// Embedded body of the container shim (with `__TOKEN__` placeholders).
const CONTAINER_JUST: &str = include_str!("../../../templates/justfiles/anvil/container.just");

/// Embedded body of the devcontainer descriptor (with `__TOKEN__`
/// placeholders).
const DEVCONTAINER_JSON: &str = include_str!("../../../templates/devcontainer/devcontainer.json");

/// Embedded body of the OCI image build recipes (with the `__IMAGE_RECIPES__`
/// generation marker).
const CONTAINER_IMAGES_JUST: &str = include_str!("../../../templates/justfiles/anvil/container-images.just");

/// Embedded body of the Kind cluster harness (with `__TOKEN__` placeholders).
const CLUSTER_JUST: &str = include_str!("../../../templates/justfiles/anvil/cluster.just");

/// Embedded body of the cluster host bootstrap + preflight (fully static —
/// pinned, checksum-verified tooling; no placeholders).
const CLUSTER_BOOTSTRAP_JUST: &str = include_str!("../../../templates/justfiles/anvil/cluster-bootstrap.just");

/// `justfiles/anvil/container.just` — the container shim.
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact)
/// so it is emitted only when container mode is enabled. The body is a
/// template; [`render_owned_body`] fills in the resolved settings at plan
/// time.
#[must_use]
pub fn container_just() -> Artifact {
    Artifact::owned_file(CONTAINER_JUST_PATH, CONTAINER_JUST)
}

/// `.devcontainer/devcontainer.json` — the devcontainer descriptor.
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact);
/// it is additionally suppressed unless `devcontainer = true` (see
/// [`emits_devcontainer`]).
#[must_use]
pub fn devcontainer() -> Artifact {
    Artifact::owned_file(DEVCONTAINER_PATH, DEVCONTAINER_JSON)
}

/// Whether the devcontainer descriptor should be emitted: container mode on
/// **and** `devcontainer = true`. Container gating alone cannot express the
/// extra flag, so `build_plan` consults this for that one path.
#[must_use]
pub(crate) fn emits_devcontainer(container: &ResolvedContainer) -> bool {
    container.enabled && container.devcontainer
}

/// `justfiles/anvil/container-images.just` — the generic OCI image build
/// recipes (pillar 2).
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact).
/// It is additionally suppressed unless at least one `[[image]]` is
/// configured (see [`secondary_gate_open`]).
#[must_use]
pub fn container_images_just() -> Artifact {
    Artifact::owned_file(CONTAINER_IMAGES_JUST_PATH, CONTAINER_IMAGES_JUST)
}

/// `justfiles/anvil/cluster.just` — the generic Kind cluster harness
/// (pillar 3).
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact).
/// It is additionally suppressed unless a `[cluster]` section is
/// configured (see [`secondary_gate_open`]).
#[must_use]
pub fn cluster_just() -> Artifact {
    Artifact::owned_file(CLUSTER_JUST_PATH, CLUSTER_JUST)
}

/// `justfiles/anvil/cluster-bootstrap.just` — host bootstrap + preflight for
/// the cluster harness.
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact).
/// Suppressed unless a `[cluster]` section is configured.
#[must_use]
pub fn cluster_bootstrap_just() -> Artifact {
    Artifact::owned_file(CLUSTER_BOOTSTRAP_JUST_PATH, CLUSTER_BOOTSTRAP_JUST)
}

/// Secondary emission gate for container-gated artifacts whose emission
/// depends on more than the container flag: the devcontainer descriptor needs
/// `devcontainer = true`, the image recipes need at least one
/// `[[image]]`, and the cluster files need a `[cluster]`
/// section. Every other path is unconstrained (`true`).
#[must_use]
pub(crate) fn secondary_gate_open(path: &str, container: &ResolvedContainer) -> bool {
    match path {
        DEVCONTAINER_PATH => emits_devcontainer(container),
        CONTAINER_IMAGES_JUST_PATH => !container.images.is_empty(),
        CLUSTER_JUST_PATH | CLUSTER_BOOTSTRAP_JUST_PATH => container.cluster.is_some(),
        _ => true,
    }
}

/// Apply container-aware rendering to one owned-file body at plan time.
///
/// Returns the body unchanged (borrowed) for every path that container mode
/// does not touch, and — crucially — for **all** paths when container mode is
/// disabled, so a disabled build is byte-identical to a no-container build.
#[must_use]
pub(crate) fn render_owned_body<'a>(path: &str, body: &'a str, container: &ResolvedContainer) -> Cow<'a, str> {
    match path {
        // The shim and devcontainer descriptor are container-gated, so they
        // only reach this function when container mode is enabled; always
        // fill in their templates.
        CONTAINER_JUST_PATH => Cow::Owned(render_container_just(body, container)),
        DEVCONTAINER_PATH => Cow::Owned(render_devcontainer(body, container)),
        // The image recipes are generated from `[[image]]`; the
        // cluster harness is filled from `[cluster]`; the bootstrap
        // file is fully static. All three are container-gated and additionally
        // secondary-gated, so they only reach here when enabled + configured.
        CONTAINER_IMAGES_JUST_PATH => Cow::Owned(render_container_images(body, container)),
        CLUSTER_JUST_PATH => match &container.cluster {
            Some(cluster) => Cow::Owned(render_cluster(body, cluster, container)),
            None => Cow::Borrowed(body),
        },
        CLUSTER_BOOTSTRAP_JUST_PATH => Cow::Borrowed(body),
        // Everything else is untouched unless container mode is enabled.
        _ if !container.enabled => Cow::Borrowed(body),
        MOD_JUST_PATH => Cow::Owned(add_container_imports(body, container)),
        TIERS_JUST_PATH => {
            let mut out = body.to_owned();
            for recipe in TIER_RECIPES {
                out = containerize_recipe(&out, recipe);
            }
            Cow::Owned(out)
        }
        p if p.starts_with(GROUPS_DIR_PREFIX) => {
            if let Some(group) = p.strip_prefix(GROUPS_DIR_PREFIX).and_then(|s| s.strip_suffix(".just")) {
                Cow::Owned(containerize_recipe(body, &format!("anvil-{group}")))
            } else {
                Cow::Borrowed(body)
            }
        }
        GH_PR_IMPL_PATH | GH_SCHEDULED_IMPL_PATH => Cow::Owned(inject_github_impl(body)),
        GH_PR_ROOT_PATH | GH_SCHEDULED_ROOT_PATH => Cow::Owned(inject_github_root(body, container)),
        ADO_JOB_PATH => Cow::Owned(inject_ado_job(body)),
        ADO_PR_ROOT_PATH | ADO_SCHEDULED_ROOT_PATH => Cow::Owned(inject_ado_root(body, container)),
        _ => Cow::Borrowed(body),
    }
}

/// Fill in the container shim template from the resolved settings.
fn render_container_just(template: &str, container: &ResolvedContainer) -> String {
    let mounts: Vec<String> = container
        .cache_volumes
        .iter()
        .map(|name| {
            format!(
                "{}:{}",
                volume_name(&container.repo_name, name),
                cache_target(name, &container.workdir)
            )
        })
        .collect();
    let volume_names: Vec<String> = container
        .cache_volumes
        .iter()
        .map(|name| volume_name(&container.repo_name, name))
        .collect();

    template
        .replace("__IMAGE__", &container.image)
        .replace("__ENGINE__", container.engine.as_str())
        .replace("__WORKDIR__", &container.workdir)
        .replace("__REPO__", &container.repo_name)
        .replace("__NATIVE_WHEN_HASHTABLE__", &native_when_hashtable(container.native_when.as_ref()))
        .replace("__CACHE_MOUNTS_ARRAY__", &ps_array(&mounts))
        .replace("__CACHE_VOLUME_NAMES_ARRAY__", &ps_array(&volume_names))
        .replace("__FORWARD_ENV_ARRAY__", &ps_array(&container.forward_env))
}

/// Fill in the devcontainer descriptor template from the resolved settings.
fn render_devcontainer(template: &str, container: &ResolvedContainer) -> String {
    template
        .replace("__IMAGE__", &container.image)
        .replace("__WORKDIR__", &container.workdir)
        .replace("__MOUNTS_JSON__", &devcontainer_mounts_json(container))
}

// ---------------------------------------------------------------------------
// Pillar 2 — generic OCI image build recipes
// ---------------------------------------------------------------------------

/// Fill in `container-images.just` by generating one self-contained pwsh
/// recipe per `[[image]]` plus an `anvil-images` aggregate that
/// drives them in deterministic dependency order.
fn render_container_images(template: &str, container: &ResolvedContainer) -> String {
    template.replace("__IMAGE_RECIPES__", &image_recipes(container))
}

/// Generate every per-image recipe followed by the `anvil-images` aggregate.
fn image_recipes(container: &ResolvedContainer) -> String {
    let mut out = String::new();
    for image in &container.images {
        out.push_str(&image_recipe(image, &container.image_output_dir));
        out.push('\n');
    }
    out.push_str(&images_aggregate(&container.image_build_order));
    out
}

/// A single self-contained `anvil-image-<name>` recipe: stage the prebuilt
/// artifacts into the context, guard the context path, then build the image.
fn image_recipe(image: &ImageSpec, output_dir: &str) -> String {
    let stages = if image.stage_artifacts.is_empty() {
        "@()".to_owned()
    } else {
        let items: Vec<String> = image
            .stage_artifacts
            .iter()
            .map(|s| format!("@{{ from = {}; to = {} }}", ps_lit(&s.from), ps_lit(&s.to)))
            .collect();
        format!("@({})", items.join(", "))
    };
    let build_args = if image.build_args.is_empty() {
        "@()".to_owned()
    } else {
        let items: Vec<String> = image
            .build_args
            .iter()
            .map(|(k, v)| format!("@{{ name = {}; value = {} }}", ps_lit(k), ps_lit(v)))
            .collect();
        format!("@({})", items.join(", "))
    };

    let mut out = String::new();
    let _ = writeln!(out, "[group(\"anvil-image\")]");
    let _ = writeln!(out, "[script(\"pwsh\")]");
    let _ = writeln!(out, "anvil-image-{} profile=\"debug\" tag=\"dev\" registry=\"local\":", image.name);
    let _ = writeln!(out, "    $ErrorActionPreference = 'Stop'");
    let _ = writeln!(out, "    $repoRoot = '{{{{justfile_directory()}}}}'");
    let _ = writeln!(out, "    $profile = '{{{{profile}}}}'");
    let _ = writeln!(out, "    $tag = '{{{{tag}}}}'");
    let _ = writeln!(out, "    $registry = '{{{{registry}}}}'");
    let _ = writeln!(
        out,
        "    function Expand-Tokens($s) {{ $s.Replace('{{profile}}', $profile).Replace('{{tag}}', $tag) }}"
    );
    let _ = writeln!(
        out,
        "    $outFull = [System.IO.Path]::GetFullPath((Join-Path $repoRoot {}))",
        ps_lit(output_dir)
    );
    let _ = writeln!(out, "    $context = Expand-Tokens {}", ps_lit(&image.context));
    let _ = writeln!(out, "    $ctxFull = [System.IO.Path]::GetFullPath((Join-Path $repoRoot $context))");
    let _ = writeln!(out, "    $sep = [System.IO.Path]::DirectorySeparatorChar");
    let _ = writeln!(
        out,
        "    if ($ctxFull -ne $outFull -and -not $ctxFull.StartsWith($outFull + $sep)) {{"
    );
    let _ = writeln!(
        out,
        "        Write-Error \"anvil: image context '$context' is outside the image output dir {}\"",
        ps_dq(output_dir)
    );
    let _ = writeln!(out, "        exit 1");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    New-Item -ItemType Directory -Force -Path $ctxFull | Out-Null");
    let _ = writeln!(out, "    $stages = {stages}");
    let _ = writeln!(out, "    foreach ($s in $stages) {{");
    let _ = writeln!(out, "        $src = Join-Path $repoRoot (Expand-Tokens $s.from)");
    let _ = writeln!(out, "        $dst = Join-Path $ctxFull (Expand-Tokens $s.to)");
    let _ = writeln!(
        out,
        "        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dst) | Out-Null"
    );
    let _ = writeln!(out, "        Copy-Item -Recurse -Force -Path $src -Destination $dst");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    $engine = just _anvil-container-engine");
    let _ = writeln!(out, "    if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}");
    let _ = writeln!(out, "    $ref = \"${{registry}}/{}:${{tag}}\"", image.name);
    let _ = writeln!(
        out,
        "    if ($engine -eq 'podman' -and $registry -notmatch '[.:/]' -and $registry -ne 'localhost') {{ $ref = \"localhost/$ref\" }}"
    );
    let _ = writeln!(out, "    $buildArgs = {build_args}");
    let _ = writeln!(
        out,
        "    $cmd = @('build', '-f', (Join-Path $repoRoot {}), '-t', $ref)",
        ps_lit(&image.dockerfile)
    );
    if let Some(target) = &image.target {
        let _ = writeln!(out, "    $cmd += @('--target', {})", ps_lit(target));
    }
    let _ = writeln!(
        out,
        "    foreach ($b in $buildArgs) {{ $cmd += @('--build-arg', \"$($b.name)=$(Expand-Tokens $b.value)\") }}"
    );
    let _ = writeln!(out, "    $cmd += $ctxFull");
    // A BuildKit build defaults to attaching a provenance attestation, which
    // turns the result into an index whose attestation content `kind load`
    // cannot resolve ("content digest not found" under ctr --all-platforms).
    // Images built here exist to be loaded into a local cluster, so suppress
    // the attestation on engines that understand the flag.
    let _ = writeln!(
        out,
        "    if ($engine -eq 'docker') {{ $cmd = @($cmd[0]) + @('--provenance=false') + $cmd[1..($cmd.Length - 1)] }}"
    );
    let _ = writeln!(out, "    Write-Output \"anvil: building $ref\"");
    let _ = writeln!(out, "    & $engine @cmd");
    let _ = writeln!(out, "    exit $LASTEXITCODE");
    out
}

/// The `anvil-images` aggregate: build every image in dependency order.
fn images_aggregate(order: &[String]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "[group(\"anvil-image\")]");
    let _ = writeln!(out, "[script(\"pwsh\")]");
    let _ = writeln!(out, "anvil-images profile=\"debug\" tag=\"dev\" registry=\"local\":");
    let _ = writeln!(out, "    $ErrorActionPreference = 'Stop'");
    for name in order {
        let _ = writeln!(
            out,
            "    just anvil-image-{name} '{{{{profile}}}}' '{{{{tag}}}}' '{{{{registry}}}}'"
        );
        let _ = writeln!(out, "    if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}");
    }
    let _ = writeln!(out, "    Write-Output \"anvil: built {} image(s)\"", order.len());
    let _ = writeln!(out, "    exit 0");
    out
}

// ---------------------------------------------------------------------------
// Pillar 3 — generic Kind cluster harness
// ---------------------------------------------------------------------------

/// Fill in `cluster.just` from the resolved `[cluster]` section.
fn render_cluster(template: &str, cluster: &ClusterConfig, _container: &ResolvedContainer) -> String {
    let diagnostics = cluster.diagnostics.clone().unwrap_or_default();
    template
        .replace("__CLUSTER_NAME__", &just_dq(&cluster.name))
        .replace("__NODE_IMAGE__", &ps_escape(cluster.node_image.as_deref().unwrap_or("")))
        .replace("__WORKERS__", &cluster.workers.to_string())
        .replace("__RETRY_ATTEMPTS__", &cluster.retry.attempts.to_string())
        .replace("__RETRY_DELAY__", &cluster.retry.delay_seconds.to_string())
        .replace(
            "__HOOK_PRE_INSTALL__",
            &ps_escape(cluster.hooks.pre_install.as_deref().unwrap_or("")),
        )
        .replace(
            "__HOOK_POST_INSTALL__",
            &ps_escape(cluster.hooks.post_install.as_deref().unwrap_or("")),
        )
        .replace("__HOOK_PRE_TEST__", &ps_escape(cluster.hooks.pre_test.as_deref().unwrap_or("")))
        .replace("__HOOK_ON_FAILURE__", &ps_escape(cluster.hooks.on_failure.as_deref().unwrap_or("")))
        .replace("__LOAD_IMAGES_ARRAY__", &ps_array(&cluster.load_images))
        .replace("__DEPENDENCIES_PWSH__", &deps_pwsh(&cluster.dependencies))
        .replace("__CHARTS_PWSH__", &charts_pwsh(&cluster.charts))
        .replace("__DIAG_RESOURCES_ARRAY__", &ps_array(&diagnostics.resources))
        .replace("__DIAG_LOGS_ARRAY__", &ps_array(&diagnostics.logs))
        .replace("__DIAG_NAMESPACE__", &ps_escape(diagnostics.namespace.as_deref().unwrap_or("")))
}

/// The cluster dependencies as a `PowerShell` array of hashtables.
fn deps_pwsh(deps: &[ClusterDependency]) -> String {
    if deps.is_empty() {
        return "@()".to_owned();
    }
    let items: Vec<String> = deps
        .iter()
        .map(|d| {
            format!(
                "@{{ name = {}; manifest = {}; version = {}; namespace = {}; preload = {}; wait = {} }}",
                ps_lit(&d.name),
                ps_lit(&d.manifest),
                ps_lit(d.version.as_deref().unwrap_or("")),
                ps_lit(d.namespace.as_deref().unwrap_or("")),
                ps_array(&d.preload_images),
                ps_array(&d.wait),
            )
        })
        .collect();
    format!("@({})", items.join(", "))
}

/// The cluster charts as a `PowerShell` array of hashtables.
fn charts_pwsh(charts: &[ClusterChart]) -> String {
    if charts.is_empty() {
        return "@()".to_owned();
    }
    let items: Vec<String> = charts
        .iter()
        .map(|c| {
            let set = if c.set.is_empty() {
                "@()".to_owned()
            } else {
                let pairs: Vec<String> = c
                    .set
                    .iter()
                    .map(|(k, v)| format!("@{{ k = {}; v = {} }}", ps_lit(k), ps_lit(v)))
                    .collect();
                format!("@({})", pairs.join(", "))
            };
            format!(
                "@{{ name = {}; path = {}; namespace = {}; crds = {}; set = {}; wait = {} }}",
                ps_lit(&c.name),
                ps_lit(&c.path),
                ps_lit(c.namespace.as_deref().unwrap_or("")),
                ps_lit(c.crds.as_deref().unwrap_or("")),
                set,
                ps_array(&c.wait),
            )
        })
        .collect();
    format!("@({})", items.join(", "))
}

/// A single-quoted `PowerShell` literal (escaping embedded quotes).
fn ps_lit(value: &str) -> String {
    format!("'{}'", ps_escape(value))
}

/// Escaping for a value spliced inside a double-quoted `PowerShell` string.
fn ps_dq(value: &str) -> String {
    format!("'{}'", value.replace('`', "``").replace('"', "`\"").replace('$', "`$"))
}

/// Escaping for a value spliced inside a `just` double-quoted string.
fn just_dq(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Splice the container `import?` lines into the entry point next to the other
/// imports. Optional (`import?`) so a manually-removed shim does not break
/// `just`. Placed right after `import 'versions.just'`.
///
/// Only imports that correspond to emitted files are added: the shim is always
/// imported (container mode is on), the image recipes only when at least one
/// `[[image]]` exists, and the cluster files only when a
/// `[cluster]` section exists. This keeps the entry point
/// byte-identical for a container build that uses neither pillar 2 nor 3.
fn add_container_imports(body: &str, container: &ResolvedContainer) -> String {
    const ANCHOR: &str = "import 'versions.just'\n";

    let mut imports = String::from("import? 'container.just'\n");
    if !container.images.is_empty() {
        imports.push_str("import? 'container-images.just'\n");
    }
    if container.cluster.is_some() {
        imports.push_str("import? 'cluster.just'\n");
        imports.push_str("import? 'cluster-bootstrap.just'\n");
    }

    if let Some(pos) = body.find(ANCHOR) {
        let insert_at = pos + ANCHOR.len();
        let mut out = String::with_capacity(body.len() + imports.len());
        out.push_str(&body[..insert_at]);
        out.push_str(&imports);
        out.push_str(&body[insert_at..]);
        out
    } else {
        let mut out = body.to_owned();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&imports);
        out
    }
}

/// Rewrite a dependency-only tier/group recipe into a `[script("pwsh")]`
/// recipe whose body first runs the re-entry guard, then invokes the original
/// dependencies via a single `just` call.
///
/// The tier/group recipes are pure dependency aggregators with no body, and a
/// recipe's dependencies run *before* its body — so a guard prepended as a
/// body would run only after the (uncontainerized) dependencies had already
/// executed. Converting the dependency list into an explicit `just d1 d2 …`
/// invocation inside the body lets the guard run first while preserving the
/// original ordering and `just`'s cross-target de-duplication.
fn containerize_recipe(body: &str, name: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let header_prefix = format!("{name}:");
    let Some(header) = lines.iter().position(|line| line.starts_with(&header_prefix)) else {
        return body.to_owned();
    };

    // Span the recipe signature across `\`-continued lines.
    let mut end = header;
    while lines[end].trim_end().ends_with('\\') && end + 1 < lines.len() {
        end += 1;
    }

    // Dependencies: everything after the first `:`, with continuations and
    // whitespace collapsed. These recipes never take parameters or carry a
    // body, so plain identifiers are all that appear here.
    let joined = lines[header..=end].join(" ");
    let after_colon = joined.split_once(':').map_or("", |(_, rest)| rest);
    let deps: Vec<&str> = after_colon
        .split(|c: char| c.is_whitespace() || c == '\\')
        .filter(|token| !token.is_empty())
        .collect();

    // Preserve any attribute lines (`[group("…")]`) already above the header.
    let mut attr_start = header;
    while attr_start > 0 && lines[attr_start - 1].starts_with('[') {
        attr_start -= 1;
    }

    let mut out = String::with_capacity(body.len() + 256);
    for line in &lines[..attr_start] {
        out.push_str(line);
        out.push('\n');
    }
    for attr in &lines[attr_start..header] {
        out.push_str(attr);
        out.push('\n');
    }
    out.push_str("[script(\"pwsh\")]\n");
    out.push_str(name);
    out.push_str(":\n");
    out.push_str("    $ErrorActionPreference = 'Stop'\n");
    out.push_str("    if ($env:ANVIL_CONTAINER -eq '1' -and $env:ANVIL_IN_CONTAINER -ne '1') {\n");
    let _ = writeln!(out, "        just _anvil-container-run {name}");
    out.push_str("        exit $LASTEXITCODE\n");
    out.push_str("    }\n");
    if !deps.is_empty() {
        let _ = writeln!(out, "    just {}", deps.join(" "));
        out.push_str("    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }\n");
    }
    for line in &lines[end + 1..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Conventional in-container mount point for a named cache volume. Generic and
/// documented: `cargo`/`rustup` map to the official rust image's
/// `CARGO_HOME`/`RUSTUP_HOME`, `target` lives under the workspace, and any
/// other name gets a stable path under `/anvil-cache`.
fn cache_target(name: &str, workdir: &str) -> String {
    match name {
        "cargo" => "/usr/local/cargo".to_owned(),
        "rustup" => "/usr/local/rustup".to_owned(),
        "target" => format!("{workdir}/target"),
        other => format!("/anvil-cache/{other}"),
    }
}

/// Named-volume identity: the repo name prefixes every cache volume so
/// multiple repos on one host do not collide.
fn volume_name(repo: &str, name: &str) -> String {
    format!("{repo}-{name}")
}

/// A `PowerShell` array literal, e.g. `@('a', 'b')` or `@()` when empty.
fn ps_array(items: &[String]) -> String {
    if items.is_empty() {
        return "@()".to_owned();
    }
    let quoted: Vec<String> = items.iter().map(|item| format!("'{}'", ps_escape(item))).collect();
    format!("@({})", quoted.join(", "))
}

/// A `PowerShell` hashtable literal matching os-release keys, or `@{}` when no
/// native-when match is configured.
fn native_when_hashtable(native_when: Option<&NativeWhen>) -> String {
    let Some(native) = native_when else {
        return "@{}".to_owned();
    };
    let mut pairs = Vec::new();
    if let Some(id) = &native.os_release_id {
        pairs.push(format!("'ID' = '{}'", ps_escape(id)));
    }
    if let Some(version) = &native.version_id {
        pairs.push(format!("'VERSION_ID' = '{}'", ps_escape(version)));
    }
    if pairs.is_empty() {
        "@{}".to_owned()
    } else {
        format!("@{{ {} }}", pairs.join("; "))
    }
}

/// The `mounts` JSON array for the devcontainer descriptor.
fn devcontainer_mounts_json(container: &ResolvedContainer) -> String {
    if container.cache_volumes.is_empty() {
        return "[]".to_owned();
    }
    let entries: Vec<String> = container
        .cache_volumes
        .iter()
        .map(|name| {
            format!(
                "    \"source={},target={},type=volume\"",
                volume_name(&container.repo_name, name),
                cache_target(name, &container.workdir)
            )
        })
        .collect();
    format!("[\n{}\n  ]", entries.join(",\n"))
}

/// Single-quote escaping for `PowerShell` literals (double an embedded quote).
fn ps_escape(value: &str) -> String {
    value.replace('\'', "''")
}

// ---------------------------------------------------------------------------
// CI job-level container injection (Pillar 6)
//
// The transforms below add a job-level container to the generated CI so that,
// when container mode is enabled, each check-group job runs inside the pinned
// image. They are intentionally string splices keyed on stable anchor lines in
// the templates: keeping them here (rather than parameterising the templates)
// means the disabled output is byte-identical to today, which the snapshot
// tests assert.
// ---------------------------------------------------------------------------

/// `workflow_call` input added to the GitHub reusable workflows. The root
/// workflow forwards the configured image into this input; an empty value
/// (the default) leaves jobs running directly on the runner.
const GH_CONTAINER_INPUT: &str = "      container_image:
        description: |
          Image to run each check-group job in. Empty (the default) runs
          jobs directly on the runner; the anvil root workflow sets this
          from anvil.toml when container mode is enabled.
        type: string
        default: \"\"\n";

/// Anchor: the last runner input in both reusable workflows.
const GH_RUNNER_INPUT_ANCHOR: &str = "        default: windows-11-arm\n";

/// Four-leg (`Shape A`) OS-matrix `runs-on` block shared by every cross-OS
/// group job.
const GH_RUNS_ON_4LEG: &str = "    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner
      || matrix.os == 'windows' && inputs.windows_runner
      || matrix.os == 'linux-arm' && inputs.linux_arm_runner
      || inputs.windows_arm_runner }}\n";

/// Two-leg (`Shape B`) OS-matrix `runs-on` line used by the single-arch group
/// job (scheduled-exhaustive).
const GH_RUNS_ON_2LEG: &str = "    runs-on: ${{ matrix.os == 'linux' && inputs.linux_runner || inputs.windows_runner }}\n";

/// Job-level `container:`/`env:` block appended after a group job's `runs-on`.
/// `container_image != '' && ... || null` yields the image when set and `null`
/// (no container) otherwise, so the block is inert when the input is empty.
const GH_JOB_CONTAINER_SUFFIX: &str = "    container: ${{ inputs.container_image != '' && inputs.container_image || null }}
    env:
      ANVIL_IN_CONTAINER: '1'\n";

/// Add the `container_image` input and, on every group job, the job-level
/// container block. Impact jobs use a single-line `runs-on` that neither
/// `Shape A` nor `Shape B` matches, so they keep running on the runner.
fn inject_github_impl(body: &str) -> String {
    let with_input = body.replacen(GH_RUNNER_INPUT_ANCHOR, &format!("{GH_RUNNER_INPUT_ANCHOR}{GH_CONTAINER_INPUT}"), 1);
    with_input
        .replace(GH_RUNS_ON_4LEG, &format!("{GH_RUNS_ON_4LEG}{GH_JOB_CONTAINER_SUFFIX}"))
        .replace(GH_RUNS_ON_2LEG, &format!("{GH_RUNS_ON_2LEG}{GH_JOB_CONTAINER_SUFFIX}"))
}

/// Forward the configured image into the reusable workflow's
/// `container_image` input from the root workflow's `uses:` call.
fn inject_github_root(body: &str, container: &ResolvedContainer) -> String {
    let with_block = format!("    with:\n      container_image: {}\n", yaml_double_quote(&container.image));
    body.replacen("    secrets: inherit\n", &format!("{with_block}    secrets: inherit\n"), 1)
}

/// ADO `container` parameter added to the job wrapper. It defaults to the
/// `anvil_container` resource alias declared by the root pipelines, so every
/// job routed through the wrapper opts in without the stages templates having
/// to forward anything; callers may pass `''` to opt a job out.
const ADO_CONTAINER_PARAM: &str = "  - name: container
    type: string
    default: anvil_container\n";

/// Anchor: the last parameter of the ADO job wrapper.
const ADO_ARTIFACTS_PARAM_ANCHOR: &str = "  - name: artifacts\n    type: object\n    default: []\n";

/// Anchor: the ADO job header (name + pool), before `steps:`.
const ADO_JOB_POOL_ANCHOR: &str = "  - job: ${{ parameters.name }}\n    pool: ${{ parameters.pool }}\n";

/// Conditional `container:` plus the always-on `ANVIL_IN_CONTAINER` variable
/// spliced between the job's `pool:` and `steps:`.
const ADO_JOB_CONTAINER_SUFFIX: &str = "    ${{ if ne(parameters.container, '') }}:
      container: ${{ parameters.container }}
    variables:
      ANVIL_IN_CONTAINER: '1'\n";

/// Add the `container` parameter and job-level container block to the ADO job
/// wrapper. Every job (group and impact alike) routes through this single
/// wrapper, so all of them gain the container and `ANVIL_IN_CONTAINER=1`.
fn inject_ado_job(body: &str) -> String {
    let with_param = body.replacen(
        ADO_ARTIFACTS_PARAM_ANCHOR,
        &format!("{ADO_ARTIFACTS_PARAM_ANCHOR}{ADO_CONTAINER_PARAM}"),
        1,
    );
    with_param.replacen(ADO_JOB_POOL_ANCHOR, &format!("{ADO_JOB_POOL_ANCHOR}{ADO_JOB_CONTAINER_SUFFIX}"), 1)
}

/// Declare the `anvil_container` container resource (pinned to the configured
/// image) on an ADO root pipeline, just before its `stages:` block.
fn inject_ado_root(body: &str, container: &ResolvedContainer) -> String {
    let resources = format!(
        "\nresources:\n  containers:\n    - container: anvil_container\n      image: {}\n",
        yaml_double_quote(&container.image)
    );
    body.replacen("\nstages:\n", &format!("{resources}\nstages:\n"), 1)
}

/// Minimal YAML double-quoted scalar (escape backslashes and quotes). Image
/// references are plain registry paths, but quoting keeps tags with `:` safe.
fn yaml_double_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::ContainerConfig;

    fn enabled(repo: &str) -> ResolvedContainer {
        ContainerConfig {
            enabled: Some(true),
            image: Some("ghcr.io/acme/rust-dev:1".to_owned()),
            ..ContainerConfig::default()
        }
        .resolve(repo)
        .unwrap()
    }

    fn disabled(repo: &str) -> ResolvedContainer {
        ContainerConfig::default().resolve(repo).unwrap()
    }

    /// `just --list` renders the *contiguous* comment block immediately above a
    /// recipe as its documentation, and shows only the final line. A two-line
    /// comment therefore surfaces as a meaningless fragment in the primary
    /// discovery UX (e.g. `anvil-container-shell  # `_anvil-container-run` uses.`).
    ///
    /// Keep the last comment line before a recipe's attributes a complete,
    /// standalone summary. Longer prose is still fine — put it above, separated
    /// by a blank line, so it is not part of the doc block.
    #[test]
    fn recipe_doc_comments_are_single_line() {
        let templates = [
            ("container.just", CONTAINER_JUST),
            ("container-images.just", CONTAINER_IMAGES_JUST),
            ("cluster.just", CLUSTER_JUST),
            ("cluster-bootstrap.just", CLUSTER_BOOTSTRAP_JUST),
        ];

        for (name, body) in templates {
            let lines: Vec<&str> = body.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                // Attribute blocks introduce a recipe; walk back over any further
                // attributes, then count the contiguous comment lines above them.
                if !line.starts_with('[') {
                    continue;
                }
                let mut cursor = index;
                while cursor > 0 && lines[cursor - 1].starts_with('[') {
                    cursor -= 1;
                }
                let mut comments = 0;
                while cursor > 0 && lines[cursor - 1].starts_with('#') {
                    comments += 1;
                    cursor -= 1;
                }
                assert!(
                    comments <= 1,
                    "{name}: recipe near line {} has a {comments}-line doc comment; \
                     `just --list` would show only its last line. Collapse it to one \
                     line, or move the prose above a blank line.",
                    index + 1,
                );
            }
        }
    }

    #[test]
    fn disabled_leaves_every_body_untouched() {
        let container = disabled("repo");
        for (path, body) in [
            (MOD_JUST_PATH, "import 'versions.just'\n"),
            (TIERS_JUST_PATH, "[group(\"anvil\")]\nanvil-pr: a b\n"),
            ("justfiles/anvil/groups/pr-fast.just", "anvil-pr-fast: x y\n"),
        ] {
            assert!(matches!(render_owned_body(path, body, &container), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn mod_just_gains_optional_import_when_enabled() {
        let container = enabled("repo");
        let body = "import 'tiers.just'\nimport 'versions.just'\nalias anvil := anvil-pr\n";
        let out = render_owned_body(MOD_JUST_PATH, body, &container);
        assert!(out.contains("import? 'container.just'\n"));
        assert!(out.contains("import 'versions.just'\nimport? 'container.just'\n"));
    }

    #[test]
    fn tier_recipe_is_rewritten_with_the_guard() {
        let container = enabled("repo");
        let body = "# doc\n[group(\"anvil\")]\nanvil-pr: anvil-pr-validate-prereqs \\\n    anvil-pr-fast \\\n    anvil-pr-slow\n";
        let out = render_owned_body(TIERS_JUST_PATH, body, &container).into_owned();
        assert!(out.contains("[group(\"anvil\")]\n[script(\"pwsh\")]\nanvil-pr:\n"));
        assert!(out.contains("just _anvil-container-run anvil-pr"));
        assert!(out.contains("just anvil-pr-validate-prereqs anvil-pr-fast anvil-pr-slow"));
        assert!(out.contains("# doc\n"));
    }

    #[test]
    fn group_recipe_name_is_derived_from_path() {
        let container = enabled("repo");
        let body = "anvil-pr-fast: anvil-fmt anvil-clippy\n";
        let out = render_owned_body("justfiles/anvil/groups/pr-fast.just", body, &container).into_owned();
        assert!(out.contains("just _anvil-container-run anvil-pr-fast"));
        assert!(out.contains("just anvil-fmt anvil-clippy"));
    }

    #[test]
    fn container_just_substitutes_all_tokens() {
        let container = enabled("myrepo");
        let out = render_container_just(CONTAINER_JUST, &container);
        assert!(!out.contains("__IMAGE__"));
        assert!(!out.contains("__ENGINE__"));
        assert!(!out.contains("__WORKDIR__"));
        assert!(!out.contains("__REPO__"));
        assert!(!out.contains("__NATIVE_WHEN_HASHTABLE__"));
        assert!(!out.contains("__CACHE_MOUNTS_ARRAY__"));
        assert!(!out.contains("__CACHE_VOLUME_NAMES_ARRAY__"));
        assert!(!out.contains("__FORWARD_ENV_ARRAY__"));
        assert!(out.contains("ghcr.io/acme/rust-dev:1"));
        assert!(out.contains("@('myrepo-cargo:/usr/local/cargo', 'myrepo-rustup:/usr/local/rustup')"));
        assert!(out.contains("@{}"));
    }

    #[test]
    fn devcontainer_references_same_image_and_volumes() {
        let container = enabled("myrepo");
        let out = render_devcontainer(DEVCONTAINER_JSON, &container);
        assert!(!out.contains("__IMAGE__"));
        assert!(!out.contains("__MOUNTS_JSON__"));
        assert!(out.contains("ghcr.io/acme/rust-dev:1"));
        assert!(out.contains("source=myrepo-cargo,target=/usr/local/cargo,type=volume"));
        assert!(out.contains("source=myrepo-rustup,target=/usr/local/rustup,type=volume"));
    }

    #[test]
    fn cache_target_maps_known_names() {
        assert_eq!(cache_target("cargo", "/w"), "/usr/local/cargo");
        assert_eq!(cache_target("rustup", "/w"), "/usr/local/rustup");
        assert_eq!(cache_target("target", "/w"), "/w/target");
        assert_eq!(cache_target("sccache", "/w"), "/anvil-cache/sccache");
    }

    #[test]
    fn ps_array_handles_empty_and_escaping() {
        assert_eq!(ps_array(&[]), "@()");
        assert_eq!(ps_array(&["a".to_owned(), "b".to_owned()]), "@('a', 'b')");
        assert_eq!(ps_array(&["a'b".to_owned()]), "@('a''b')");
    }

    #[test]
    fn native_when_hashtable_renders_configured_keys() {
        let native = NativeWhen {
            os_release_id: Some("ubuntu".to_owned()),
            version_id: Some("22.04".to_owned()),
        };
        assert_eq!(native_when_hashtable(Some(&native)), "@{ 'ID' = 'ubuntu'; 'VERSION_ID' = '22.04' }");
        assert_eq!(native_when_hashtable(None), "@{}");
    }

    #[test]
    fn devcontainer_emission_needs_flag() {
        let mut container = enabled("repo");
        assert!(!emits_devcontainer(&container), "devcontainer off by default");
        container.devcontainer = true;
        assert!(emits_devcontainer(&container));
        let mut off = disabled("repo");
        off.devcontainer = true;
        assert!(!emits_devcontainer(&off), "disabled container never emits devcontainer");
    }

    const GH_PR_IMPL: &str = include_str!("../../../templates/github/pr-impl-workflow.yml");
    const GH_SCHEDULED_IMPL: &str = include_str!("../../../templates/github/scheduled-impl-workflow.yml");
    const GH_PR_ROOT: &str = include_str!("../../../templates/github/pr-root-workflow.yml");
    const ADO_JOB: &str = include_str!("../../../templates/ado/steps/job.yml");
    const ADO_PR_ROOT: &str = include_str!("../../../templates/ado/pr-root-pipeline.yml");

    #[test]
    fn github_impl_gains_input_and_job_container() {
        let container = enabled("repo");
        for body in [GH_PR_IMPL, GH_SCHEDULED_IMPL] {
            let out = render_owned_body(GH_PR_IMPL_PATH, body, &container).into_owned();
            assert!(out.contains("container_image:"), "adds workflow_call input");
            let containers = out.matches("container: ${{ inputs.container_image").count();
            let group_jobs = body.matches("runs-on: ${{ matrix.os").count();
            assert_eq!(containers, group_jobs, "every matrix (group) job gains a container");
            assert!(out.contains("ANVIL_IN_CONTAINER: '1'"));
            // Impact jobs use a single-line `runs-on` and must stay on the
            // runner (no container injected after them).
            if body.contains("    runs-on: ${{ inputs.linux_runner }}\n") {
                assert!(out.contains("    runs-on: ${{ inputs.linux_runner }}\n"));
                assert!(!out.contains("    runs-on: ${{ inputs.linux_runner }}\n    container:"));
            }
        }
    }

    #[test]
    fn github_impl_disabled_is_untouched() {
        let container = disabled("repo");
        assert!(matches!(
            render_owned_body(GH_PR_IMPL_PATH, GH_PR_IMPL, &container),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn github_root_forwards_image() {
        let container = enabled("repo");
        let out = render_owned_body(GH_PR_ROOT_PATH, GH_PR_ROOT, &container).into_owned();
        assert!(out.contains("    with:\n      container_image: \"ghcr.io/acme/rust-dev:1\"\n"));
        assert!(out.contains("with:\n      container_image: \"ghcr.io/acme/rust-dev:1\"\n    secrets: inherit\n"));
    }

    #[test]
    fn ado_job_gains_param_and_container() {
        let container = enabled("repo");
        let out = render_owned_body(ADO_JOB_PATH, ADO_JOB, &container).into_owned();
        assert!(out.contains("  - name: container\n    type: string\n    default: anvil_container\n"));
        assert!(out.contains("${{ if ne(parameters.container, '') }}:\n      container: ${{ parameters.container }}\n"));
        assert!(out.contains("variables:\n      ANVIL_IN_CONTAINER: '1'\n"));
    }

    #[test]
    fn ado_root_declares_container_resource() {
        let container = enabled("repo");
        let out = render_owned_body(ADO_PR_ROOT_PATH, ADO_PR_ROOT, &container).into_owned();
        assert!(out.contains("resources:\n  containers:\n    - container: anvil_container\n      image: \"ghcr.io/acme/rust-dev:1\"\n"));
        assert!(out.contains("\nstages:\n"), "stages block preserved");
    }

    #[test]
    fn yaml_double_quote_escapes() {
        assert_eq!(yaml_double_quote("a:b"), "\"a:b\"");
        assert_eq!(yaml_double_quote("a\"b"), "\"a\\\"b\"");
    }
}
