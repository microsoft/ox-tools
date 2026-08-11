// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Plan-time rendering for the opt-in container-execution surface.
//!
//! When a repository enables container mode in `anvil.toml`, the generated
//! recipe tree gains a shim (`justfiles/anvil/container.just`) and the tier
//! and group recipes gain a re-entry guard that runs the same `just` target
//! inside the container. All of this is applied at plan time from the
//! `ResolvedContainer` settings, so the artifact bodies stored in the
//! catalog stay repository-independent (the catalog checksum is stable) and
//! an absent / disabled config is byte-for-byte identical to a no-container
//! build.
//!
//! The token-substitution approach mirrors the `github` module's
//! `render_group_action`: templates carry `__TOKEN__`
//! placeholders that are replaced here with the resolved settings.

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::catalog::Artifact;
use crate::config::{ClusterChart, ClusterConfig, ClusterDependency, ExecImageSource, ImageSpec, NativeWhen, ResolvedContainer};

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

/// Repo-root-relative path of the generated default exec-image Dockerfile.
///
/// Container *assets* live under `.anvil/`, not in the recipe tree:
/// `justfiles/` holds `just` recipe files and nothing else.
pub(crate) const EXEC_DOCKERFILE_PATH: &str = ".anvil/container/Dockerfile";

/// Repo-root-relative path of the exec-image build-context filter.
///
/// Must sit beside [`EXEC_DOCKERFILE_PATH`]: BuildKit resolves the filter as
/// `<dockerfile>.dockerignore`, so the two move together.
pub(crate) const EXEC_DOCKERIGNORE_PATH: &str = ".anvil/container/Dockerfile.dockerignore";

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

/// Embedded body of the default exec-image Dockerfile (with `__TOKEN__`
/// placeholders).
const EXEC_DOCKERFILE: &str = include_str!("../../../templates/container/Dockerfile");

/// Embedded body of the exec-image build-context filter (fully static).
const EXEC_DOCKERIGNORE: &str = include_str!("../../../templates/container/Dockerfile.dockerignore");

/// `justfiles/anvil/container.just` — the container shim.
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact)
/// so it is emitted only when container mode is enabled. The body is a
/// template; `render_owned_body` fills in the resolved settings at plan
/// time.
#[must_use]
pub fn container_just() -> Artifact {
    Artifact::owned_file(CONTAINER_JUST_PATH, CONTAINER_JUST)
}

/// `.devcontainer/devcontainer.json` — the devcontainer descriptor.
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact);
/// it is additionally suppressed unless `devcontainer = true`.
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
/// configured.
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
/// configured.
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

/// `.anvil/container/Dockerfile` — the default exec image.
///
/// Register it with
/// [`CatalogBuilder::with_container_artifact`](crate::CatalogBuilder::with_container_artifact).
/// Suppressed unless the exec image is built from anvil's own Dockerfile; a
/// repository that pulls a pre-built image or points at its own Dockerfile has
/// no use for it, and emitting it anyway would invite edits to a file nothing
/// reads.
#[must_use]
pub fn exec_dockerfile() -> Artifact {
    Artifact::owned_file(EXEC_DOCKERFILE_PATH, EXEC_DOCKERFILE)
}

/// `.anvil/container/Dockerfile.dockerignore` — build-context filter
/// for [`exec_dockerfile`]. Gated identically.
#[must_use]
pub fn exec_dockerignore() -> Artifact {
    Artifact::owned_file(EXEC_DOCKERIGNORE_PATH, EXEC_DOCKERIGNORE)
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
        EXEC_DOCKERFILE_PATH | EXEC_DOCKERIGNORE_PATH => container.image_source.builds_anvil_base(),
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
        // The default Dockerfile carries one placeholder (the in-container
        // workdir); the ignore file is fully static.
        EXEC_DOCKERFILE_PATH => Cow::Owned(body.replace("__WORKDIR__", &container.workdir)),
        EXEC_DOCKERIGNORE_PATH => Cow::Borrowed(body),
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
        // CI job containers are gated on `Pull`. A job-level `container:` needs
        // a reference the runner can resolve *before* the job starts, which a
        // locally-built, content-tagged image is not: it exists only on the
        // machine that built it. Injecting it anyway would splice an empty
        // reference — rejected outright by ADO, and silently non-containerized
        // on GitHub, which is worse. A repository that wants containerized CI
        // publishes an image and sets `image`.
        GH_PR_IMPL_PATH | GH_SCHEDULED_IMPL_PATH if container.image_source == ExecImageSource::Pull => Cow::Owned(inject_github_impl(body)),
        GH_PR_ROOT_PATH | GH_SCHEDULED_ROOT_PATH if container.image_source == ExecImageSource::Pull => {
            Cow::Owned(inject_github_root(body, container))
        }
        ADO_JOB_PATH if container.image_source == ExecImageSource::Pull => Cow::Owned(inject_ado_job(body)),
        ADO_PR_ROOT_PATH | ADO_SCHEDULED_ROOT_PATH if container.image_source == ExecImageSource::Pull => {
            Cow::Owned(inject_ado_root(body, container))
        }
        _ => Cow::Borrowed(body),
    }
}

/// Recipe-visible name for the exec-image source.
const fn image_source_token(container: &ResolvedContainer) -> &'static str {
    match container.image_source {
        ExecImageSource::Pull => "pull",
        ExecImageSource::BuildDefault | ExecImageSource::BuildRepo => "build",
        ExecImageSource::BuildExtended => "extend",
    }
}

/// Repository part of a locally-built exec image. The tag is appended at run
/// time from the content hash, so this is only the stable half of the
/// reference.
fn built_image_name(repo_name: &str) -> String {
    format!("anvil-{}", sanitize_image_name(repo_name))
}

/// Coerce a repository name into the character set an image reference allows
/// (lowercase alphanumerics plus `.`, `_`, `-`). A repo whose directory name
/// contains anything else would otherwise produce a reference the engine
/// rejects, at run time, with a message about the tag rather than the name.
fn sanitize_image_name(repo_name: &str) -> String {
    let mut out: String = repo_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // A reference component must start with an alphanumeric.
    while out.starts_with(|c: char| !c.is_ascii_alphanumeric()) {
        out.remove(0);
    }
    if out.is_empty() { "repo".to_owned() } else { out }
}

/// Dockerfile for the image anvil builds first: the repository's own when it
/// replaced anvil's, otherwise the copy anvil generates. In `extends` mode
/// this is anvil's — the base the extension is layered onto. Empty in pull
/// mode, where nothing is built.
fn exec_dockerfile_path(container: &ResolvedContainer) -> String {
    match container.image_source {
        ExecImageSource::Pull => String::new(),
        ExecImageSource::BuildDefault | ExecImageSource::BuildExtended => EXEC_DOCKERFILE_PATH.to_owned(),
        ExecImageSource::BuildRepo => container.dockerfile.clone(),
    }
}

/// Dockerfile layered on top of the base. Empty outside `extends` mode.
fn extension_dockerfile_path(container: &ResolvedContainer) -> String {
    match container.image_source {
        ExecImageSource::BuildExtended => container.extends.clone(),
        _ => String::new(),
    }
}

/// Whether the repository's own `build-args`, `build-secrets` and
/// `hash-inputs` describe the extension rather than the base.
///
/// In `extends` mode the base is anvil's own image: the repository never
/// declared it, so its build inputs cannot apply to it. Feeding them there
/// would also defeat the point — the base would rebuild for changes that only
/// concern the layer above it.
const fn inputs_describe_extension(container: &ResolvedContainer) -> bool {
    matches!(container.image_source, ExecImageSource::BuildExtended)
}

/// `--build-arg` pairs as `KEY=VALUE`, in declaration order.
fn build_arg_pairs(container: &ResolvedContainer) -> Vec<String> {
    container.build_args.iter().map(|(key, value)| format!("{key}={value}")).collect()
}

/// Files whose contents define the base image's identity.
///
/// This is the *literal* half of the list: the Dockerfile, its context filter,
/// the pinned toolchain, and whatever the repository declared. The generated
/// recipe adds the recipe tree at run time, because the image installs its
/// tools by running `just anvil-setup` and that chain reaches the tier, group,
/// check and tool files — a set anvil cannot enumerate from configuration
/// alone. See [`hash_excludes`] for the files held back.
fn hash_inputs(container: &ResolvedContainer) -> Vec<String> {
    if !container.image_source.builds() {
        return Vec::new();
    }
    let mut inputs = vec![exec_dockerfile_path(container), "rust-toolchain.toml".to_owned()];
    if container.image_source.builds_anvil_base() {
        // The ignore file decides what reaches the daemon, so it changes the
        // build as surely as the Dockerfile does.
        inputs.push(EXEC_DOCKERIGNORE_PATH.to_owned());
    }
    if !inputs_describe_extension(container) {
        inputs.extend(container.hash_inputs.iter().cloned());
    }
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

/// Files whose contents define the extension image's identity, on top of the
/// resolved base reference the recipe folds in at run time.
///
/// Only the extension Dockerfile and the repository's declared inputs: the
/// base is represented by its tag, which already summarises everything
/// underneath. Empty outside `extends` mode.
fn extension_hash_inputs(container: &ResolvedContainer) -> Vec<String> {
    if !inputs_describe_extension(container) {
        return Vec::new();
    }
    let mut inputs = vec![container.extends.clone()];
    inputs.extend(container.hash_inputs.iter().cloned());
    inputs.sort_unstable();
    inputs.dedup();
    inputs
}

/// Recipe files excluded from the image identity.
///
/// Each one drives the container *from the host* or targets the cluster, so
/// none can change what `just anvil-setup` installs. `container.just` in
/// particular must be excluded on pain of circularity: it is the file that
/// computes the hash.
fn hash_excludes(container: &ResolvedContainer) -> Vec<String> {
    let mut excluded = vec![CONTAINER_JUST_PATH.to_owned()];
    if !container.images.is_empty() {
        excluded.push(CONTAINER_IMAGES_JUST_PATH.to_owned());
    }
    if container.cluster.is_some() {
        excluded.push(CLUSTER_JUST_PATH.to_owned());
        excluded.push(CLUSTER_BOOTSTRAP_JUST_PATH.to_owned());
    }
    excluded.sort_unstable();
    excluded
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
        .replace("__IMAGE_SOURCE__", image_source_token(container))
        .replace("__IMAGE_NAME__", &built_image_name(&container.repo_name))
        .replace("__EXT_IMAGE_NAME__", &format!("{}-ext", built_image_name(&container.repo_name)))
        .replace("__DOCKERFILE__", &exec_dockerfile_path(container))
        .replace("__EXT_DOCKERFILE__", &extension_dockerfile_path(container))
        .replace("__BUILD_ARGS_ARRAY__", &ps_array(&build_arg_pairs(container)))
        .replace("__BUILD_SECRETS_ARRAY__", &ps_array(&container.build_secrets))
        .replace("__HASH_INPUTS_ARRAY__", &ps_array(&hash_inputs(container)))
        .replace("__EXT_HASH_INPUTS_ARRAY__", &ps_array(&extension_hash_inputs(container)))
        .replace("__HASH_EXCLUDE_ARRAY__", &ps_array(&hash_excludes(container)))
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
        .replace("__IMAGE_OR_BUILD__", &devcontainer_source_json(container))
        .replace("__WORKDIR__", &container.workdir)
        .replace("__MOUNTS_JSON__", &devcontainer_mounts_json(container))
}

/// The descriptor's image source: a reference to pull, or a build to run.
///
/// The editor resolves this itself and cannot call the generated recipes, so a
/// locally-built image has to be expressed as a `build` block. Emitting an
/// `image` key here in build mode would name an image the editor has no way to
/// obtain — and, before the tag exists, no way to even identify.
fn devcontainer_source_json(container: &ResolvedContainer) -> String {
    if container.image_source.builds() {
        let mut out = format!(
            "\"build\": {{\n    \"dockerfile\": \"{}\",\n    \"context\": \"..\"",
            json_escape(&exec_dockerfile_path(container))
        );
        if !container.build_args.is_empty() {
            let args: Vec<String> = container
                .build_args
                .iter()
                .map(|(key, value)| format!("\"{}\": \"{}\"", json_escape(key), json_escape(value)))
                .collect();
            out.push_str(&format!(",\n    \"args\": {{ {} }}", args.join(", ")));
        }
        out.push_str("\n  }");
        out
    } else {
        format!("\"image\": \"{}\"", json_escape(&container.image))
    }
}

/// Escape a string for embedding in a JSON string literal.
fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Pillar 2 — generic OCI image build recipes
// ---------------------------------------------------------------------------

/// Fill in `container-images.just` with one `anvil-image` recipe driven by a
/// generated spec table, plus an `anvil-images` aggregate that walks it in
/// dependency order.
fn render_container_images(template: &str, container: &ResolvedContainer) -> String {
    template.replace("__IMAGE_RECIPES__", &image_recipes(container))
}

/// The image spec table, the single build recipe, and the aggregate.
fn image_recipes(container: &ResolvedContainer) -> String {
    let mut out = String::new();
    out.push_str(&image_recipe(&container.images, &container.image_output_dir));
    out.push('\n');
    out.push_str(&images_aggregate(&container.image_build_order));
    out
}

/// One PowerShell hashtable entry per `[[image]]`.
///
/// Every image is built by identical logic, so only the *data* is generated
/// per image: emitting a whole recipe each time would repeat ~35 lines of body
/// for every entry and leave N copies to drift apart.
fn image_spec_entries(images: &[ImageSpec]) -> String {
    let mut out = String::new();
    for image in images {
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
        let target = image.target.as_deref().map_or_else(|| "$null".to_owned(), ps_lit);

        let _ = writeln!(out, "        {} = @{{", ps_lit(&image.name));
        let _ = writeln!(
            out,
            "            repository = {}",
            ps_lit(image.repository.as_deref().unwrap_or(&image.name))
        );
        let _ = writeln!(out, "            dockerfile = {}", ps_lit(&image.dockerfile));
        let _ = writeln!(out, "            context    = {}", ps_lit(&image.context));
        let _ = writeln!(out, "            target     = {target}");
        let _ = writeln!(out, "            stages     = {stages}");
        let _ = writeln!(out, "            buildArgs  = {build_args}");
        let _ = writeln!(out, "        }}");
    }
    out
}

/// The single `anvil-image <name>` recipe: look the name up in the spec table,
/// stage the prebuilt artifacts into the context, guard the context path, then
/// build the image.
fn image_recipe(images: &[ImageSpec], output_dir: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Build one image by name. `just anvil-image` with no name lists them.");
    let _ = writeln!(out, "[group(\"anvil-image\")]");
    let _ = writeln!(out, "[script(\"pwsh\")]");
    let _ = writeln!(out, "anvil-image name=\"\" profile=\"debug\" tag=\"dev\" registry=\"local\":");
    let _ = writeln!(out, "    $ErrorActionPreference = 'Stop'");
    let _ = writeln!(out, "    # Generated from [[image]]; every entry is built by the body below.");
    let _ = writeln!(out, "    $specs = [ordered]@{{");
    out.push_str(&image_spec_entries(images));
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    $name = '{{{{name}}}}'");
    let _ = writeln!(out, "    if (-not $name) {{");
    let _ = writeln!(out, "        Write-Output \"anvil: images: $($specs.Keys -join ', ')\"");
    let _ = writeln!(out, "        exit 0");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    if (-not $specs.Contains($name)) {{");
    let _ = writeln!(
        out,
        "        Write-Error \"anvil: unknown image '$name' (valid: $($specs.Keys -join ', '))\""
    );
    let _ = writeln!(out, "        exit 1");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "    $spec = $specs[$name]");
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
    let _ = writeln!(out, "    $context = Expand-Tokens $spec.context");
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
    let _ = writeln!(out, "    foreach ($s in $spec.stages) {{");
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
    let _ = writeln!(out, "    $ref = \"${{registry}}/$($spec.repository):${{tag}}\"");
    let _ = writeln!(
        out,
        "    if ($engine -eq 'podman' -and $registry -notmatch '[.:/]' -and $registry -ne 'localhost') {{ $ref = \"localhost/$ref\" }}"
    );
    let _ = writeln!(
        out,
        "    $cmd = @('build', '-f', (Join-Path $repoRoot $spec.dockerfile), '-t', $ref)"
    );
    let _ = writeln!(out, "    if ($spec.target) {{ $cmd += @('--target', $spec.target) }}");
    let _ = writeln!(
        out,
        "    foreach ($b in $spec.buildArgs) {{ $cmd += @('--build-arg', \"$($b.name)=$(Expand-Tokens $b.value)\") }}"
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
            "    just anvil-image {} '{{{{profile}}}}' '{{{{tag}}}}' '{{{{registry}}}}'",
            ps_lit(name)
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
fn render_cluster(template: &str, cluster: &ClusterConfig, container: &ResolvedContainer) -> String {
    let diagnostics = cluster.diagnostics.clone().unwrap_or_default();
    // `load-images` names images; the cluster loads them by *reference*, so map
    // each name through its repository override before emitting.
    let load_refs: Vec<String> = cluster
        .load_images
        .iter()
        .map(|name| {
            container
                .images
                .iter()
                .find(|image| &image.name == name)
                .and_then(|image| image.repository.clone())
                .unwrap_or_else(|| name.clone())
        })
        .collect();
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
        .replace("__LOAD_IMAGES_ARRAY__", &ps_array(&load_refs))
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

    /// `_anvil-container-image` returns the image reference on stdout, and its
    /// callers capture that stdout. Anything else written there silently
    /// becomes part of the reference — which produced a real bug where the
    /// build notice was prepended to the tag. Progress must go to stderr.
    #[test]
    fn image_recipe_keeps_stdout_clean() {
        let body = CONTAINER_JUST
            .split("_anvil-container-image:")
            .nth(1)
            .expect("the shim defines the image recipe");
        let recipe = body.split("\n# ").next().unwrap_or(body);
        assert!(
            !recipe.contains("Write-Host"),
            "Write-Host in _anvil-container-image pollutes the returned image reference; \
             use [Console]::Error.WriteLine for progress"
        );
        assert!(
            recipe.contains("[Console]::Error.WriteLine"),
            "the build notice must still be reported, on stderr"
        );
    }

    /// Every `[script("pwsh")]` recipe body must actually parse as PowerShell.
    ///
    /// A parse error is not local to the offending line: PowerShell rejects the
    /// whole script, so one bad token disables an entire recipe. This shipped
    /// once already (`*>&2`, which is not valid redirection syntax) and was
    /// invisible to every Rust-level test, because the generator's job is to
    /// emit text — it never runs what it writes.
    #[test]
    fn generated_powershell_recipes_parse() {
        let Some(pwsh) = which_pwsh() else {
            // Not installed here; the recipes themselves already require it, so
            // any machine that can run them will run this check.
            return;
        };

        // Render with every section configured: the cluster and image
        // templates keep their placeholders when their section is absent, and
        // in that case they are never emitted at all.
        let container = ContainerConfig {
            enabled: Some(true),
            images: Some(vec![ImageSpec {
                name: "svc".to_owned(),
                repository: None,
                dockerfile: "svc/Dockerfile".to_owned(),
                target: None,
                context: "out/svc".to_owned(),
                stage_artifacts: Vec::new(),
                build_args: Vec::new(),
                depends_on: Vec::new(),
            }]),
            cluster: Some(crate::config::ClusterConfig::default()),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();

        for (name, template) in [
            ("container.just", CONTAINER_JUST),
            ("container-images.just", CONTAINER_IMAGES_JUST),
            ("cluster.just", CLUSTER_JUST),
            ("cluster-bootstrap.just", CLUSTER_BOOTSTRAP_JUST),
        ] {
            let rendered = super::render_owned_body(&format!("justfiles/anvil/{name}"), template, &container).into_owned();
            for (recipe, body) in pwsh_recipe_bodies(&rendered) {
                assert_powershell_parses(&pwsh, name, &recipe, &body);
            }
        }
    }

    /// Locate a PowerShell executable, or `None` when the host has none.
    fn which_pwsh() -> Option<String> {
        for candidate in ["pwsh", "pwsh.exe"] {
            if std::process::Command::new(candidate)
                .args(["-NoProfile", "-Command", "exit 0"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
            {
                return Some(candidate.to_owned());
            }
        }
        None
    }

    /// Split a rendered `.just` file into `(recipe name, script body)` pairs
    /// for every `[script("pwsh")]` recipe, undoing `just`'s indentation.
    fn pwsh_recipe_bodies(rendered: &str) -> Vec<(String, String)> {
        let lines: Vec<&str> = rendered.lines().collect();
        let mut out = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if !line.starts_with("[script(\"pwsh\"") {
                continue;
            }
            // Skip any further attributes to reach the recipe header.
            let mut cursor = index + 1;
            while cursor < lines.len() && lines[cursor].starts_with('[') {
                cursor += 1;
            }
            let Some(header) = lines.get(cursor) else { continue };
            let name = header.split(|c: char| c == ':' || c == ' ').next().unwrap_or(header);
            let mut body = String::new();
            cursor += 1;
            while cursor < lines.len() {
                let current = lines[cursor];
                if !current.is_empty() && !current.starts_with("    ") {
                    break;
                }
                body.push_str(current.strip_prefix("    ").unwrap_or(current));
                body.push('\n');
                cursor += 1;
            }
            out.push((name.to_owned(), body));
        }
        assert!(!out.is_empty(), "expected at least one pwsh recipe to check");
        out
    }

    /// Parse a script with PowerShell's own parser and fail with its errors.
    fn assert_powershell_parses(pwsh: &str, file: &str, recipe: &str, body: &str) {
        // `just` substitutes `{{ ... }}` before pwsh sees the script; replace
        // them with a literal so interpolation braces do not read as syntax.
        let mut script = String::new();
        let mut rest = body;
        while let Some(start) = rest.find("{{") {
            script.push_str(&rest[..start]);
            script.push_str("anvil_substituted");
            let after = &rest[start + 2..];
            match after.find("}}") {
                Some(end) => rest = &after[end + 2..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        script.push_str(rest);

        let mut child = std::process::Command::new(pwsh)
            .args([
                "-NoProfile",
                "-Command",
                "$src = [Console]::In.ReadToEnd(); \
                 $errors = $null; \
                 [void][System.Management.Automation.Language.Parser]::ParseInput($src, [ref]$null, [ref]$errors); \
                 if ($errors.Count) { $errors | ForEach-Object { [Console]::Error.WriteLine($_.Message) }; exit 1 }",
            ])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn pwsh");
        {
            use std::io::Write as _;
            child.stdin.take().expect("stdin").write_all(script.as_bytes()).unwrap();
        }
        let output = child.wait_with_output().expect("run pwsh");
        assert!(
            output.status.success(),
            "{file}: recipe `{recipe}` is not valid PowerShell:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// An image reference component must be lowercase and drawn from a
    /// restricted alphabet. A repository whose directory name breaks that would
    /// otherwise fail at run time with a message about the tag rather than the
    /// name, long after the mistake was made.
    #[test]
    fn built_image_name_is_a_valid_reference_component() {
        assert_eq!(built_image_name("ox-tools"), "anvil-ox-tools");
        assert_eq!(built_image_name("COSMICRust"), "anvil-cosmicrust");
        assert_eq!(built_image_name("my repo!"), "anvil-my-repo-");
        assert_eq!(built_image_name("_leading"), "anvil-leading");
        assert_eq!(built_image_name("---"), "anvil-repo");
    }

    /// Identity inputs are sorted and deduplicated, so the same declared set
    /// always produces the same stream regardless of declaration order — a tag
    /// that changed when a config line moved would rebuild for no reason.
    #[test]
    fn hash_inputs_are_ordered_and_deduplicated() {
        let container = ContainerConfig {
            enabled: Some(true),
            dockerfile: Some("ci/Dockerfile".to_owned()),
            hash_inputs: Some(vec![
                "z.sh".to_owned(),
                "a.sh".to_owned(),
                "z.sh".to_owned(),
                // Redeclaring a built-in input must not double it.
                "rust-toolchain.toml".to_owned(),
            ]),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();

        assert_eq!(
            hash_inputs(&container),
            vec!["a.sh", "ci/Dockerfile", "rust-toolchain.toml", "z.sh"]
        );
    }

    /// The image must install tools the way CI does. `binstall` is not merely
    /// a speed-up here: a source install of a pinned tool can fail on a
    /// toolchain CI never source-builds on, which turns a toolchain bump into
    /// a broken image while CI stays green.
    #[test]
    fn the_image_installs_tools_the_way_ci_does() {
        assert!(
            EXEC_DOCKERFILE.contains("just anvil-setup binstall"),
            "the exec Dockerfile must pass `binstall` to anvil-setup"
        );
        assert!(
            EXEC_DOCKERFILE.contains("cargo-binstall"),
            "the exec Dockerfile must ship binstall, or every build compiles it"
        );
    }

    /// The image installs its tools through `just anvil-setup`, whose
    /// dependency chain reaches the tier, group, check and tool recipes — so
    /// the recipe tree defines image contents and is hashed at run time. Only
    /// the files that drive the container from the host, or target the
    /// cluster, are held back. Excluding `container.just` is load-bearing: it
    /// is the file that computes the hash.
    #[test]
    fn only_host_side_recipes_are_excluded_from_identity() {
        let default_build = ContainerConfig {
            enabled: Some(true),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();
        assert_eq!(hash_excludes(&default_build), vec![CONTAINER_JUST_PATH.to_owned()]);

        // The generated Dockerfile and its context filter both change the
        // build, so both belong to the identity.
        let inputs = hash_inputs(&default_build);
        assert!(inputs.contains(&EXEC_DOCKERFILE_PATH.to_owned()));
        assert!(inputs.contains(&EXEC_DOCKERIGNORE_PATH.to_owned()));
        assert!(inputs.contains(&"rust-toolchain.toml".to_owned()));

        // Pull mode describes no build, so there is nothing to hash.
        assert!(hash_inputs(&enabled("repo")).is_empty());
    }

    /// Extending splits the identity in two. The base keeps anvil's own inputs
    /// so it stays shareable and rarely rebuilt; the repository's declared
    /// inputs describe the layer it actually owns. Putting them on the base
    /// would rebuild the expensive half for a change to the cheap one, which
    /// is the whole reason `extends` exists.
    #[test]
    fn extending_splits_the_identity_in_two() {
        let extended = ContainerConfig {
            enabled: Some(true),
            extends: Some("ci/ext.Dockerfile".to_owned()),
            hash_inputs: Some(vec!["ci/tools.sh".to_owned()]),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();

        let base = hash_inputs(&extended);
        assert!(base.contains(&EXEC_DOCKERFILE_PATH.to_owned()), "the base is anvil's image");
        assert!(
            !base.contains(&"ci/tools.sh".to_owned()) && !base.contains(&"ci/ext.Dockerfile".to_owned()),
            "the repository's inputs must not reach the base: {base:?}"
        );

        let extension = extension_hash_inputs(&extended);
        assert_eq!(extension, vec!["ci/ext.Dockerfile", "ci/tools.sh"]);

        // The two Dockerfiles are distinct files with distinct roles.
        assert_eq!(exec_dockerfile_path(&extended), EXEC_DOCKERFILE_PATH);
        assert_eq!(extension_dockerfile_path(&extended), "ci/ext.Dockerfile");
    }

    /// Only `extends` has a second layer; every other mode must leave the
    /// extension surface empty so the generated recipe never tries to build one.
    #[test]
    fn only_extending_has_an_extension_layer() {
        for container in [
            enabled("repo"),
            ContainerConfig {
                enabled: Some(true),
                ..ContainerConfig::default()
            }
            .resolve("repo")
            .unwrap(),
            ContainerConfig {
                enabled: Some(true),
                dockerfile: Some("ci/own.Dockerfile".to_owned()),
                ..ContainerConfig::default()
            }
            .resolve("repo")
            .unwrap(),
        ] {
            assert!(extension_dockerfile_path(&container).is_empty());
            assert!(extension_hash_inputs(&container).is_empty());
        }
    }

    /// A repository that replaces anvil's Dockerfile gets no generated one;
    /// a repository that extends it does, because that is the base being built.
    #[test]
    fn generated_dockerfile_follows_the_base() {
        let extended = ContainerConfig {
            enabled: Some(true),
            extends: Some("ci/ext.Dockerfile".to_owned()),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();
        assert!(secondary_gate_open(EXEC_DOCKERFILE_PATH, &extended));
        assert!(secondary_gate_open(EXEC_DOCKERIGNORE_PATH, &extended));
    }

    /// The cluster and image-build recipes never affect what `anvil-setup`
    /// installs, so they must not force an image rebuild when they change.
    #[test]
    fn cluster_and_image_recipes_do_not_define_image_identity() {
        let container = ContainerConfig {
            enabled: Some(true),
            images: Some(vec![ImageSpec {
                name: "svc".to_owned(),
                repository: None,
                dockerfile: "svc/Dockerfile".to_owned(),
                target: None,
                context: "out/svc".to_owned(),
                stage_artifacts: Vec::new(),
                build_args: Vec::new(),
                depends_on: Vec::new(),
            }]),
            cluster: Some(crate::config::ClusterConfig::default()),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();

        let excluded = hash_excludes(&container);
        for path in [
            CONTAINER_JUST_PATH,
            CONTAINER_IMAGES_JUST_PATH,
            CLUSTER_JUST_PATH,
            CLUSTER_BOOTSTRAP_JUST_PATH,
        ] {
            assert!(excluded.contains(&path.to_owned()), "{path} must not define image identity");
        }
    }

    /// The generated Dockerfile is emitted only when it is the one being built.
    /// Shipping it alongside a repository's own would invite edits to a file
    /// nothing reads.
    #[test]
    fn generated_dockerfile_is_emitted_only_when_used() {
        let default_build = ContainerConfig {
            enabled: Some(true),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();
        assert!(secondary_gate_open(EXEC_DOCKERFILE_PATH, &default_build));

        let repo_build = ContainerConfig {
            enabled: Some(true),
            dockerfile: Some("ci/Dockerfile".to_owned()),
            ..ContainerConfig::default()
        }
        .resolve("repo")
        .unwrap();
        assert!(!secondary_gate_open(EXEC_DOCKERFILE_PATH, &repo_build));
        assert!(!secondary_gate_open(EXEC_DOCKERIGNORE_PATH, &enabled("repo")));
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
