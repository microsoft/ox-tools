// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The user-facing `anvil.toml` configuration file.
//!
//! `anvil.toml` (repo root) is anvil's first user-facing config file. It is
//! **optional**: an absent file means byte-identical behavior to a build with
//! no container support at all. Four **sibling** top-level sections are
//! defined, each independently optional:
//!
//! * `[container]` — opt a repository into running the generated `just`
//!   recipes inside a container (pillar 1).
//! * `[[image]]` — locally-built OCI images (pillar 2).
//! * `[cluster]` — a generic Kind cluster harness (pillar 3).
//! * `image-output-dir` — a top-level key naming the staged-context guard root
//!   for `[[image]]` builds (default `out`). As a bare top-level key it must
//!   appear **before** any `[section]` header, per TOML.
//!
//! Building images and running a cluster are siblings of containerized
//! execution, not sub-concerns of it, so they live at the top level rather
//! than nested under `[container]`. The parser rejects unknown keys loudly
//! (typo protection) while keeping the top-level dispatch trivial to extend.
//!
//! ## Layering
//!
//! Effective settings come from three layers, later overriding earlier:
//!
//! 1. Built-in defaults (this module).
//! 2. Catalog defaults supplied by a fork via
//!    [`crate::CatalogBuilder::with_container_defaults`] (e.g. an internal
//!    flavor pre-filling `image`).
//! 3. The user's `anvil.toml`.
//!
//! Overriding is **field-by-field**: a value set in `anvil.toml` wins for that
//! field only; every unset field falls back to the catalog default, then the
//! built-in default. See [`ContainerConfig::overlay`] and
//! [`ContainerConfig::resolve`].
//!
//! ## The `[anvil]` section and the artifact-group allow-list
//!
//! `[anvil] artifacts` is an opt-in **allow-list** naming which groups of
//! catalog artifacts anvil manages in this repository:
//!
//! ```toml
//! [anvil]
//! artifacts = ["recipes", "container"]
//! ```
//!
//! The base catalog's artifacts partition into exactly four
//! [`ArtifactGroup`]s (`recipes`, `config`, `backends`, `container`; see
//! [`crate::anvil::group::group_of`]). An artifact is emitted only when its
//! group is in the allow-list **and** its existing gates (backend gate,
//! container gate) pass — group selection *composes with*, and never
//! overrides, the existing gating.
//!
//! * **Omitting the `artifacts` key selects every group**, which is
//!   byte-identical to the pre-allow-list behavior (the hard compatibility
//!   requirement).
//! * An unknown group name is a hard error listing the valid groups.
//! * An empty list is an error: it selects nothing and is meaningless — the
//!   user is told to remove the key (or the whole `[anvil]` section) instead.
//! * Removing a group from the allow-list on a later run cleanly **retracts**
//!   the artifacts that group previously emitted (the owned files anvil
//!   created are deleted and the managed regions it spliced are removed),
//!   because those artifacts leave the live plan and the normal
//!   removal-reconciliation path retracts anything anvil owned that is no
//!   longer selected. Nothing anvil never owned is touched.

use std::collections::BTreeSet;
use std::path::Path;

use ohno::{AppError, IntoAppError as _, app_err, bail};
use toml_edit::{DocumentMut, Item};

/// One selectable group of catalog artifacts, named in `anvil.toml`'s
/// `[anvil] artifacts` allow-list.
///
/// The base catalog's artifacts partition into exactly these four groups; the
/// mapping from an [`Artifact`](crate::catalog::Artifact) to its group is
/// derived structurally in [`crate::anvil::group::group_of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ArtifactGroup {
    /// The `justfiles/anvil/` owned recipe tree (entry/tools/versions/helpers/
    /// tiers + `checks/*` + `groups/*`) and the `Justfile` imports region that
    /// makes it reachable.
    Recipes,
    /// The managed regions spliced into the user's own config files
    /// (`Cargo.toml` lints, `deny.toml`, `rustfmt.toml`, `spellcheck.toml`,
    /// `.delta.toml`, `clippy.toml`, `.gitattributes`).
    Config,
    /// The backend-gated cloud-workflow CI files (GitHub / ADO).
    Backends,
    /// The container-gated artifacts (`container.just`, devcontainer, image
    /// recipes, cluster harness and its bootstrap).
    Container,
}

impl ArtifactGroup {
    /// Every group, in canonical order. This is the default selection when the
    /// `[anvil] artifacts` key is omitted — every group enabled.
    pub(crate) const ALL: [Self; 4] = [Self::Recipes, Self::Config, Self::Backends, Self::Container];

    /// Canonical lowercase name as written in `anvil.toml`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Recipes => "recipes",
            Self::Config => "config",
            Self::Backends => "backends",
            Self::Container => "container",
        }
    }

    /// Parse a group name as written in `[anvil] artifacts`.
    ///
    /// # Errors
    ///
    /// Returns an error listing the valid groups for any unknown name (typo
    /// protection, consistent with the strict `anvil.toml` parsing).
    fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "recipes" => Ok(Self::Recipes),
            "config" => Ok(Self::Config),
            "backends" => Ok(Self::Backends),
            "container" => Ok(Self::Container),
            other => Err(app_err!(
                "unknown artifact group '{other}' (valid groups: recipes, config, backends, container)"
            )),
        }
    }

    /// The default selection (every group), as an owned set.
    pub(crate) fn all_set() -> BTreeSet<Self> {
        Self::ALL.into_iter().collect()
    }
}

/// The container engine to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// Auto-detect: prefer `docker`, then `podman`.
    #[default]
    Auto,
    /// Always use `docker`.
    Docker,
    /// Always use `podman`.
    Podman,
}

impl Engine {
    /// Canonical lowercase name as written in `anvil.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }

    fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "auto" => Ok(Self::Auto),
            "docker" => Ok(Self::Docker),
            "podman" => Ok(Self::Podman),
            other => Err(app_err!(
                "invalid [container] engine '{other}' (valid values: auto, docker, podman)"
            )),
        }
    }
}

/// Host match that, when satisfied, runs recipes natively instead of
/// containerizing (the `[container.native-when]` table).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NativeWhen {
    /// Matched against `ID` from `/etc/os-release`.
    pub os_release_id: Option<String>,
    /// Matched against `VERSION_ID` from `/etc/os-release`.
    pub version_id: Option<String>,
}

/// One `stage-artifacts` entry: copy `from` (repo-relative) into `to`
/// (staged-context-relative) before the image build. Binaries are always
/// copied in prebuilt — never compiled in-image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageArtifact {
    /// Repo-root-relative source path. May contain `{profile}`/`{tag}` tokens.
    pub from: String,
    /// Staged-context-relative destination path. May contain `{profile}`/`{tag}`
    /// tokens.
    pub to: String,
}

/// One `[[image]]` entry: a locally-built OCI image.
///
/// The image is built from a staged context (never the repo root) with its
/// binaries copied in prebuilt. There is deliberately no registry push, no
/// ACR, no auth, and no promotion — images are built locally and loaded into
/// the Kind cluster ([`ClusterConfig`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSpec {
    /// Image name; drives the `anvil-image-<name>` recipe. Unique within the
    /// image set and restricted to a valid `just` recipe token
    /// (`[A-Za-z][A-Za-z0-9_-]*`).
    pub name: String,
    /// Image repository path, appended to the registry to form the reference
    /// `<registry>/<repository>:<tag>`. Defaults to [`Self::name`].
    ///
    /// Needed whenever the published path differs from the recipe name — a
    /// repository that groups its images under a namespace publishes
    /// `local/cosmic-sandbox/cs-agent` while the recipe must stay
    /// `anvil-image-cs-agent`, because `/` is not a valid `just` recipe token.
    /// Getting this wrong is invisible until a pod tries to pull.
    pub repository: Option<String>,
    /// Repo-root-relative path to the Dockerfile.
    pub dockerfile: String,
    /// Optional multi-stage build target.
    pub target: Option<String>,
    /// Staged build-context directory. Must live under the image output dir
    /// (see [`ContainerConfig::image_output_dir`]) — the recipe refuses a
    /// context outside it, guarding against sending the whole repo.
    pub context: String,
    /// Files copied from the repo into the staged context before the build.
    pub stage_artifacts: Vec<StageArtifact>,
    /// `--build-arg` pairs, in declaration order. Values may contain
    /// `{profile}`/`{tag}` tokens.
    pub build_args: Vec<(String, String)>,
    /// Names of images that must be built before this one. Every entry must
    /// name another `[[image]]`; cycles are rejected at resolve.
    pub depends_on: Vec<String>,
}

/// One `[[cluster.dependency]]`: an external, pinned chart/manifest
/// applied to the cluster before the repo's own charts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterDependency {
    /// Human-readable dependency name (used in log output).
    pub name: String,
    /// URL or repo-relative path of the pinned manifest to apply.
    pub manifest: String,
    /// Optional pinned version (informational; recorded in log output).
    pub version: Option<String>,
    /// Namespace the dependency installs into. Rollout targets in `wait` are
    /// resolved against it, so a dependency that creates its own namespace
    /// (cert-manager, ingress-nginx, …) must set this or the wait looks in
    /// `default` and fails.
    pub namespace: Option<String>,
    /// Images to pre-pull on the host and `kind load` before applying the
    /// manifest (Kind node containerd can inherit an unreachable DNS proxy).
    pub preload_images: Vec<String>,
    /// Rollout targets to wait for after applying the manifest.
    pub wait: Vec<String>,
}

/// One `[[cluster.chart]]`: a repo-local Helm chart to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterChart {
    /// Chart name; drives the release name.
    pub name: String,
    /// Repo-root-relative path to the chart directory.
    pub path: String,
    /// Optional target namespace (created if missing).
    pub namespace: Option<String>,
    /// Optional directory of CRDs applied server-side **before**
    /// `helm upgrade --install` (Helm skips CRDs on upgrade).
    pub crds: Option<String>,
    /// `--set key=value` overrides, in declaration order. Values may contain
    /// `{tag}`/`{profile}` tokens.
    pub set: Vec<(String, String)>,
    /// Rollout targets to wait for after install.
    pub wait: Vec<String>,
}

/// The `[cluster.diagnostics]` table: what to dump when a deploy or
/// test step fails.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterDiagnostics {
    /// `kubectl get <entry>` invocations to dump.
    pub resources: Vec<String>,
    /// Deployments whose logs to tail.
    pub logs: Vec<String>,
    /// Namespace the `logs` targets live in. Without it the targets resolve
    /// in `default`, so a chart installed into its own namespace yields
    /// "not found" instead of logs — exactly when the diagnostics matter most.
    pub namespace: Option<String>,
}

/// The `[cluster.retry]` table: bounded retries around the deploy +
/// test flow, with a readiness re-check between attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterRetry {
    /// Total attempts (>= 1). Built-in default: `1` (no retry).
    pub attempts: u32,
    /// Delay in seconds between attempts. Built-in default: `0`.
    pub delay_seconds: u32,
}

impl Default for ClusterRetry {
    fn default() -> Self {
        Self {
            attempts: 1,
            delay_seconds: 0,
        }
    }
}

/// The `[cluster.hooks]` table: names of user-defined `just` recipes
/// invoked at fixed extension points.
///
/// Anvil never models what a hook does — it only invokes the named recipe when
/// set, so a repository can inject bespoke wiring without anvil learning any
/// domain concept.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterHooks {
    /// Invoked after the cluster is up but before charts are installed.
    pub pre_install: Option<String>,
    /// Invoked after all charts are installed.
    pub post_install: Option<String>,
    /// Invoked before the test phase.
    pub pre_test: Option<String>,
    /// Invoked when a phase fails (before diagnostics).
    pub on_failure: Option<String>,
}

/// The `[cluster]` section: a generic Kind cluster harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterConfig {
    /// Kind cluster name. Built-in default: `anvil-kind`.
    pub name: String,
    /// Pinned Kind node image (digest encouraged). `None` uses the Kind
    /// binary's built-in default.
    pub node_image: Option<String>,
    /// Number of worker nodes. Built-in default: `0` (control-plane only).
    pub workers: u32,
    /// Names of `[[image]]` entries to `kind load` into the cluster.
    pub load_images: Vec<String>,
    /// External pinned chart/manifest dependencies, applied in order before
    /// the repo's own charts.
    pub dependencies: Vec<ClusterDependency>,
    /// Repo-local Helm charts to install, in order.
    pub charts: Vec<ClusterChart>,
    /// Failure diagnostics.
    pub diagnostics: Option<ClusterDiagnostics>,
    /// Bounded-retry policy.
    pub retry: ClusterRetry,
    /// Extension hooks.
    pub hooks: ClusterHooks,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            name: "anvil-kind".to_owned(),
            node_image: None,
            workers: 0,
            load_images: Vec::new(),
            dependencies: Vec::new(),
            charts: Vec::new(),
            diagnostics: None,
            retry: ClusterRetry::default(),
            hooks: ClusterHooks::default(),
        }
    }
}

/// Where the exec image comes from.
///
/// The four variants are selected by which of `[container] image`,
/// `dockerfile` and `extends` are set — see [`ContainerConfig::resolve`].
/// Setting more than one is rejected, because "pull this", "build that" and
/// "build on top of mine" cannot all be the answer, and silently preferring
/// one would hide a config mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecImageSource {
    /// `image` set: pull the reference verbatim. The reference carries its own
    /// tag, so there is nothing to hash.
    Pull,
    /// Nothing set: build from the Dockerfile anvil generates. This is what
    /// makes a repository self-sustaining with no image configuration at all.
    BuildDefault,
    /// `dockerfile` set: build from that repo-owned Dockerfile, instead of
    /// anvil's. Use when the base OS itself has to differ.
    BuildRepo,
    /// `extends` set: build anvil's own image, then build the repository's
    /// Dockerfile `FROM` it. Two images, two tags.
    ///
    /// The reason this is not just [`Self::BuildRepo`] with a hand-written
    /// `FROM`: the base is content-tagged, so its reference is not knowable
    /// until it is resolved. Anvil resolves it and injects it as a build arg.
    /// Layering also keeps the expensive half — toolchain and tool catalog —
    /// cached across changes to the cheap half.
    BuildExtended,
}

impl ExecImageSource {
    /// Whether this source builds an image locally rather than pulling one.
    /// Only built images have a content-derived tag.
    pub(crate) const fn builds(self) -> bool {
        matches!(self, Self::BuildDefault | Self::BuildRepo | Self::BuildExtended)
    }

    /// Whether anvil's own Dockerfile is built as part of this source. True
    /// for the default image and for the base underneath an extension.
    pub(crate) const fn builds_anvil_base(self) -> bool {
        matches!(self, Self::BuildDefault | Self::BuildExtended)
    }
}

/// The resolved container-feature configuration, as a set of **optional**
/// overrides.
///
/// The `[container]`-exec fields (`enabled`, `image`, `engine`, …) come from
/// the `[container]` section. The `images`, `image_output_dir` and `cluster`
/// fields carry the **top-level sibling** sections (`[[image]]`, the
/// `image-output-dir` key, and `[cluster]`); parsing folds them in here so a
/// single value drives resolution and rendering.
///
/// Every field is `Option` so the catalog default layer and the user's
/// `anvil.toml` can be merged field-by-field ([`Self::overlay`]) before the
/// built-in defaults are applied by resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerConfig {
    /// Opt into container execution. Built-in default: `false`.
    pub enabled: Option<bool>,
    /// Pre-built exec image reference to pull. Mutually exclusive with
    /// [`Self::dockerfile`] and [`Self::extends`]; see [`ExecImageSource`] for
    /// the selection rule.
    pub image: Option<String>,
    /// Repo-root-relative Dockerfile that replaces anvil's and builds the exec
    /// image on its own. Mutually exclusive with [`Self::image`] and
    /// [`Self::extends`].
    pub dockerfile: Option<String>,
    /// Repo-root-relative Dockerfile layered *on top of* anvil's image. It
    /// receives the resolved base reference as the `ANVIL_BASE_IMAGE` build
    /// arg, which it is expected to `FROM`.
    ///
    /// Mutually exclusive with [`Self::image`] and [`Self::dockerfile`].
    pub extends: Option<String>,
    /// `--build-arg` pairs for the exec-image build, in declaration order.
    /// Part of the image identity hash. Built-in default: empty.
    pub build_args: Option<Vec<(String, String)>>,
    /// BuildKit `--secret` specifications for the exec-image build, e.g.
    /// `id=msrustup_token,env=MSRUSTUP_ACCESS_TOKEN`.
    ///
    /// Deliberately *excluded* from the identity hash: a secret's value must
    /// never influence a tag, and its presence is a property of the caller's
    /// environment rather than of the image contents. Built-in default: empty.
    pub build_secrets: Option<Vec<String>>,
    /// Extra repo-root-relative files folded into the exec-image identity
    /// hash. Needed whenever the Dockerfile `COPY`s something the built-in
    /// input list does not already cover. Built-in default: empty.
    pub hash_inputs: Option<Vec<String>>,
    /// Container engine. Built-in default: [`Engine::Auto`].
    pub engine: Option<Engine>,
    /// Stable identifier for this repository, used to expand `{repo}` in
    /// `workdir` and to prefix cache volume names. Built-in default: the
    /// repo-root directory name.
    ///
    /// Set this explicitly in any repository that enforces a regeneration
    /// drift check. The directory-name default is *not* stable across
    /// checkouts — a CI agent that clones into `s`, a git worktree, and a
    /// developer's clone all produce different names, so the emitted
    /// `container.just` would differ and the drift check would fail.
    pub name: Option<String>,
    /// In-container mount point of the repo root. Built-in default:
    /// `/workspaces/{repo}` (`{repo}` = repo directory name).
    pub workdir: Option<String>,
    /// Named cache volumes to mount. Built-in default: `["cargo", "rustup"]`.
    pub cache_volumes: Option<Vec<String>>,
    /// Host env-var glob patterns to forward. Built-in default: `[]`.
    pub forward_env: Option<Vec<String>>,
    /// Also emit `.devcontainer/devcontainer.json`. Built-in default: `false`.
    pub devcontainer: Option<bool>,
    /// Optional native-execution host match.
    pub native_when: Option<NativeWhen>,
    /// OCI images to build locally (the top-level `[[image]]` sections).
    /// Built-in default: empty.
    pub images: Option<Vec<ImageSpec>>,
    /// Root directory under which every image build context must live (the
    /// top-level `image-output-dir` key). The image recipes refuse a context
    /// outside it. Built-in default: `out`.
    pub image_output_dir: Option<String>,
    /// The Kind cluster harness (the top-level `[cluster]` section). Built-in
    /// default: absent (no cluster recipes emitted).
    pub cluster: Option<ClusterConfig>,
}

impl ContainerConfig {
    /// Field-by-field overlay: values set on `self` win; every unset field
    /// falls back to `base`. Used to layer the user's `anvil.toml` (`self`)
    /// over the catalog defaults (`base`).
    #[must_use]
    pub fn overlay(self, base: &Self) -> Self {
        // `image`, `dockerfile` and `extends` are mutually exclusive, so they
        // overlay as a single slot rather than field-by-field: if `self` names
        // any one of them, it owns the choice of source and inherits none.
        // Merging them independently would let a catalog default `image`
        // collide with a repository's `extends` and fail resolution with an
        // ambiguity the author never wrote.
        let overrides_source = self.image.is_some() || self.dockerfile.is_some() || self.extends.is_some();
        let (image, dockerfile, extends) = if overrides_source {
            (self.image, self.dockerfile, self.extends)
        } else {
            (base.image.clone(), base.dockerfile.clone(), base.extends.clone())
        };
        Self {
            enabled: self.enabled.or(base.enabled),
            image,
            dockerfile,
            extends,
            build_args: self.build_args.or_else(|| base.build_args.clone()),
            build_secrets: self.build_secrets.or_else(|| base.build_secrets.clone()),
            hash_inputs: self.hash_inputs.or_else(|| base.hash_inputs.clone()),
            engine: self.engine.or(base.engine),
            name: self.name.or_else(|| base.name.clone()),
            workdir: self.workdir.or_else(|| base.workdir.clone()),
            cache_volumes: self.cache_volumes.or_else(|| base.cache_volumes.clone()),
            forward_env: self.forward_env.or_else(|| base.forward_env.clone()),
            devcontainer: self.devcontainer.or(base.devcontainer),
            native_when: self.native_when.or_else(|| base.native_when.clone()),
            images: self.images.or_else(|| base.images.clone()),
            image_output_dir: self.image_output_dir.or_else(|| base.image_output_dir.clone()),
            cluster: self.cluster.or_else(|| base.cluster.clone()),
        }
    }

    /// Apply built-in defaults and validate, producing the concrete settings
    /// the emitter consumes. `dir_name` is the repo-root directory name, used
    /// as the fallback identity when `[container] name` is unset: it expands
    /// the `{repo}` placeholder in `workdir` and prefixes cache volume names.
    ///
    /// Prefer setting `name` explicitly. The directory name is not stable
    /// across checkouts, so relying on it makes the emitted `container.just`
    /// depend on where the repository happens to be cloned.
    ///
    /// # Errors
    ///
    /// Returns an error if `enabled` resolves to `true` but the exec image
    /// source is ambiguous (both `image` and `dockerfile` set) or ill-formed
    /// (an empty `image` or `dockerfile`), or if the image/cluster sections
    /// are invalid (duplicate/ill-named images, a `depends-on` cycle, a
    /// context outside the image output dir, or a `load-images` entry naming
    /// no image).
    pub(crate) fn resolve(&self, dir_name: &str) -> Result<ResolvedContainer, AppError> {
        let enabled = self.enabled.unwrap_or(false);
        let image = self.image.clone().unwrap_or_default();
        let dockerfile = self.dockerfile.clone().unwrap_or_default();
        let extends = self.extends.clone().unwrap_or_default();
        let repo_name = self.name.clone().unwrap_or_else(|| dir_name.to_owned());
        let workdir = self.workdir.clone().unwrap_or_else(|| format!("/workspaces/{repo_name}"));
        let cache_volumes = self
            .cache_volumes
            .clone()
            .unwrap_or_else(|| vec!["cargo".to_owned(), "rustup".to_owned()]);
        let forward_env = self.forward_env.clone().unwrap_or_default();

        let images = self.images.clone().unwrap_or_default();
        let image_output_dir = self.image_output_dir.clone().unwrap_or_else(|| "out".to_owned());
        validate_images(&images, &image_output_dir)?;
        let image_build_order = topo_order(&images)?;
        let cluster = self.cluster.clone();
        if let Some(cluster) = &cluster {
            validate_cluster(cluster, &images)?;
        }

        // Select the exec-image source. `image` pulls a pre-built reference;
        // `dockerfile` builds one in place of anvil's; `extends` builds anvil's
        // and then layers on top of it; nothing at all builds the Dockerfile
        // anvil generates, which is what lets a repository adopt container
        // execution without owning any image plumbing.
        //
        // An explicitly empty value clears the slot rather than erroring, so a
        // repository whose catalog pre-fills one of these can select the
        // default build with `image = ""`. Rejecting it would leave no way to
        // opt out: deleting the key just re-inherits the catalog's value.
        let set: Vec<&str> = [
            (!image.trim().is_empty()).then_some("image"),
            (!dockerfile.trim().is_empty()).then_some("dockerfile"),
            (!extends.trim().is_empty()).then_some("extends"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let image_source = match set.as_slice() {
            [] => ExecImageSource::BuildDefault,
            ["image"] => ExecImageSource::Pull,
            ["dockerfile"] => ExecImageSource::BuildRepo,
            ["extends"] => ExecImageSource::BuildExtended,
            several => bail!(
                "[container] sets {}; these are alternatives, so keep exactly one: \
                 `image` to pull a pre-built image, `dockerfile` to build one in place of \
                 anvil's, or `extends` to build on top of anvil's",
                several.join(" and ")
            ),
        };

        // A devcontainer descriptor names one image or one build. It cannot
        // express "build the base, then build this FROM it", and the extension
        // alone is unbuildable without the base reference anvil injects. Say so
        // rather than emit a descriptor the editor will fail to open.
        let devcontainer = self.devcontainer.unwrap_or(false);
        if devcontainer && image_source == ExecImageSource::BuildExtended {
            bail!(
                "[container] `devcontainer` cannot be combined with `extends`: a devcontainer \
                 descriptor cannot express a two-stage build. Publish the extended image and \
                 point `image` at it, or drop `devcontainer`"
            );
        }

        Ok(ResolvedContainer {
            enabled,
            image,
            dockerfile,
            extends,
            image_source,
            build_args: self.build_args.clone().unwrap_or_default(),
            build_secrets: self.build_secrets.clone().unwrap_or_default(),
            hash_inputs: self.hash_inputs.clone().unwrap_or_default(),
            engine: self.engine.unwrap_or_default(),
            workdir,
            cache_volumes,
            forward_env,
            devcontainer,
            native_when: self.native_when.clone(),
            repo_name,
            images,
            image_output_dir,
            image_build_order,
            cluster,
        })
    }
}

/// Validate the image set: unique, well-formed names; `depends-on` targets
/// that exist; non-empty dockerfile; and a context under `output_dir`.
fn validate_images(images: &[ImageSpec], output_dir: &str) -> Result<(), AppError> {
    let mut seen = std::collections::BTreeSet::new();
    for image in images {
        if !is_recipe_token(&image.name) {
            bail!("invalid [[image]] name '{}' (must match [A-Za-z][A-Za-z0-9_-]*)", image.name);
        }
        if !seen.insert(image.name.as_str()) {
            bail!("duplicate [[image]] name '{}'", image.name);
        }
    }
    for image in images {
        if image.dockerfile.trim().is_empty() {
            bail!("[[image]] '{}' requires a non-empty `dockerfile`", image.name);
        }
        if !context_under_output_dir(&image.context, output_dir) {
            bail!(
                "[[image]] '{}' context '{}' must live under the image output dir '{}' \
                 (a staged context, never the repo root)",
                image.name,
                image.context,
                output_dir
            );
        }
        for dep in &image.depends_on {
            if !images.iter().any(|other| other.name == *dep) {
                bail!("[[image]] '{}' depends-on '{}' names no image", image.name, dep);
            }
        }
    }
    Ok(())
}

/// Whether `context` is a repo-relative path nested under `output_dir` and not
/// the output dir itself, an absolute path, or an escaping `..` path.
fn context_under_output_dir(context: &str, output_dir: &str) -> bool {
    let normalized = context.replace('\\', "/");
    let output = output_dir.trim_end_matches('/');
    let prefix = format!("{output}/");
    if !normalized.starts_with(&prefix) {
        return false;
    }
    // Reject any `..` segment so the context cannot climb back out.
    !normalized.split('/').any(|segment| segment == "..")
}

/// Whether `name` is a valid `just` recipe token: `[A-Za-z][A-Za-z0-9_-]*`.
fn is_recipe_token(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Deterministic topological order of the image build graph (declaration
/// order breaks ties). Returns an error naming the members of a `depends-on`
/// cycle.
fn topo_order(images: &[ImageSpec]) -> Result<Vec<String>, AppError> {
    let mut ordered: Vec<String> = Vec::with_capacity(images.len());
    let mut emitted = std::collections::BTreeSet::new();
    // Repeatedly emit, in declaration order, any image whose dependencies are
    // all already emitted. A full pass that emits nothing means a cycle.
    loop {
        let mut progressed = false;
        for image in images {
            if emitted.contains(image.name.as_str()) {
                continue;
            }
            if image.depends_on.iter().all(|dep| emitted.contains(dep.as_str())) {
                ordered.push(image.name.clone());
                emitted.insert(image.name.as_str());
                progressed = true;
            }
        }
        if ordered.len() == images.len() {
            break;
        }
        if !progressed {
            let cycle: Vec<&str> = images
                .iter()
                .map(|image| image.name.as_str())
                .filter(|name| !emitted.contains(name))
                .collect();
            bail!("[[image]] depends-on cycle among: {}", cycle.join(", "));
        }
    }
    Ok(ordered)
}

/// Validate the cluster section against the image set: every `load-images`
/// entry must name a declared image.
fn validate_cluster(cluster: &ClusterConfig, images: &[ImageSpec]) -> Result<(), AppError> {
    if cluster.name.trim().is_empty() {
        bail!("[cluster] requires a non-empty `name`");
    }
    for wanted in &cluster.load_images {
        if !images.iter().any(|image| image.name == *wanted) {
            bail!("[cluster] load-images entry '{}' names no [[image]]", wanted);
        }
    }
    for chart in &cluster.charts {
        if chart.name.trim().is_empty() {
            bail!("[[cluster.chart]] requires a non-empty `name`");
        }
        if chart.path.trim().is_empty() {
            bail!("[[cluster.chart]] '{}' requires a non-empty `path`", chart.name);
        }
    }
    for dependency in &cluster.dependencies {
        if dependency.name.trim().is_empty() {
            bail!("[[cluster.dependency]] requires a non-empty `name`");
        }
        if dependency.manifest.trim().is_empty() {
            bail!("[[cluster.dependency]] '{}' requires a non-empty `manifest`", dependency.name);
        }
    }
    if cluster.retry.attempts == 0 {
        bail!("[cluster.retry] attempts must be >= 1");
    }
    Ok(())
}

/// The parsed `anvil.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnvilConfig {
    /// The container-feature configuration: the `[container]` section plus the
    /// top-level `[[image]]`, `image-output-dir` and `[cluster]` siblings,
    /// folded together by [`parse`].
    pub container: ContainerConfig,
    /// The `[anvil] artifacts` allow-list of catalog-artifact groups to
    /// manage, or `None` when the `artifacts` key is omitted (every group
    /// enabled — byte-identical to the pre-allow-list behavior).
    pub artifacts: Option<BTreeSet<ArtifactGroup>>,
}

/// Fully-resolved container settings with all defaults applied.
///
/// Distinct from the optional [`ContainerConfig`] so the emitter never has to
/// re-apply defaults or worry about `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedContainer {
    pub enabled: bool,
    /// Pre-built reference to pull; empty unless `image_source` is
    /// [`ExecImageSource::Pull`].
    pub image: String,
    /// Repo-owned Dockerfile path; empty unless `image_source` is
    /// [`ExecImageSource::BuildRepo`].
    pub dockerfile: String,
    /// Repo-owned Dockerfile layered on anvil's image; empty unless
    /// `image_source` is [`ExecImageSource::BuildExtended`].
    pub extends: String,
    /// How the exec image is obtained.
    pub image_source: ExecImageSource,
    /// `--build-arg` pairs for the exec-image build, in declaration order.
    pub build_args: Vec<(String, String)>,
    /// BuildKit `--secret` specifications for the exec-image build.
    pub build_secrets: Vec<String>,
    /// Extra files folded into the exec-image identity hash.
    pub hash_inputs: Vec<String>,
    pub engine: Engine,
    pub workdir: String,
    pub cache_volumes: Vec<String>,
    pub forward_env: Vec<String>,
    pub devcontainer: bool,
    pub native_when: Option<NativeWhen>,
    pub repo_name: String,
    /// Images to build, in declaration order.
    pub images: Vec<ImageSpec>,
    /// Root dir every image context must live under.
    pub image_output_dir: String,
    /// Image names in deterministic dependency (build) order.
    pub image_build_order: Vec<String>,
    /// The Kind cluster harness, if configured.
    pub cluster: Option<ClusterConfig>,
}

/// Load `anvil.toml` from the repo root, returning defaults when it is absent.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed, or if it
/// contains unknown keys or ill-typed values.
pub(crate) fn load(repo_root: &Path) -> Result<AnvilConfig, AppError> {
    let path = repo_root.join("anvil.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(AnvilConfig::default()),
        Err(err) => return Err(err).into_app_err_with(|| format!("failed to read {}", path.display())),
    };
    parse(&text).map_err(|err| app_err!("invalid {}: {err:#}", path.display()))
}

/// Parse the text of an `anvil.toml`.
pub(crate) fn parse(text: &str) -> Result<AnvilConfig, AppError> {
    let doc: DocumentMut = text.parse::<DocumentMut>().into_app_err("not valid TOML")?;

    // Top-level dispatch: `[container]`, the container siblings, and the
    // `[anvil]` meta-section are defined today. New sections are added by
    // extending this match — an unknown table is a typo.
    let mut container = ContainerConfig::default();
    let mut images: Option<Vec<ImageSpec>> = None;
    let mut image_output_dir: Option<String> = None;
    let mut cluster: Option<ClusterConfig> = None;
    let mut artifacts: Option<BTreeSet<ArtifactGroup>> = None;
    for (key, item) in doc.as_table() {
        match key {
            "container" => container = parse_container(item)?,
            "image" => images = Some(parse_images(item)?),
            "image-output-dir" => image_output_dir = Some(as_string(key, item)?),
            "cluster" => cluster = Some(parse_cluster(item)?),
            "anvil" => artifacts = parse_anvil_section(item)?,
            other => bail!(
                "unknown top-level key '{other}' (valid top-level items: [anvil], [container], \
                 [[image]], [cluster], image-output-dir)"
            ),
        }
    }
    // `[[image]]`, `[cluster]` and `image-output-dir` are top-level siblings of
    // `[container]`, but resolve alongside the container settings, so fold them
    // into the container config here.
    container.images = images;
    container.image_output_dir = image_output_dir;
    container.cluster = cluster;
    Ok(AnvilConfig { container, artifacts })
}

/// Parse the `[anvil]` meta-section, returning the `artifacts` allow-list.
///
/// Returns `None` when the section is present but carries no `artifacts` key
/// (equivalent to omitting the section — every group enabled). Rejects any
/// other key in `[anvil]` as a typo.
fn parse_anvil_section(item: &Item) -> Result<Option<BTreeSet<ArtifactGroup>>, AppError> {
    let table = item.as_table_like().ok_or_else(|| app_err!("[anvil] must be a table"))?;
    let mut groups = None;
    for (key, value) in table.iter() {
        match key {
            "artifacts" => groups = Some(parse_artifact_groups(value)?),
            other => bail!("unknown key '[anvil] {other}' (valid keys: artifacts)"),
        }
    }
    Ok(groups)
}

/// Parse `[anvil] artifacts` (an array of group names) into a set of
/// [`ArtifactGroup`]s.
///
/// # Errors
///
/// Rejects an empty list (meaningless — selects nothing) and any unknown group
/// name.
fn parse_artifact_groups(item: &Item) -> Result<BTreeSet<ArtifactGroup>, AppError> {
    let names = as_string_array("artifacts", item)?;
    if names.is_empty() {
        bail!(
            "[anvil] artifacts is an empty list, which selects no artifacts and is meaningless; \
             remove the `artifacts` key (or the whole [anvil] section) to manage every group"
        );
    }
    let mut groups = BTreeSet::new();
    for name in &names {
        groups.insert(ArtifactGroup::parse(name)?);
    }
    Ok(groups)
}

fn parse_container(item: &Item) -> Result<ContainerConfig, AppError> {
    let table = item.as_table_like().ok_or_else(|| app_err!("[container] must be a table"))?;

    let mut config = ContainerConfig::default();
    for (key, value) in table.iter() {
        match key {
            "enabled" => config.enabled = Some(as_bool(key, value)?),
            // Image builds moved to the top-level `[[image]]` array; give a
            // pointed error rather than a generic type mismatch when a stale
            // nested `[[container.image]]` is encountered.
            "image" if value.is_array_of_tables() => bail!(
                "nested [[container.image]] is no longer supported; declare image builds at the \
                 top level as [[image]]"
            ),
            "image" => config.image = Some(as_string(key, value)?),
            "dockerfile" => config.dockerfile = Some(as_string(key, value)?),
            "extends" => config.extends = Some(as_string(key, value)?),
            "build-args" => config.build_args = Some(parse_string_map(key, value)?),
            "build-secrets" => config.build_secrets = Some(as_string_array(key, value)?),
            "hash-inputs" => config.hash_inputs = Some(as_string_array(key, value)?),
            "engine" => config.engine = Some(Engine::parse(&as_string(key, value)?)?),
            "name" => config.name = Some(as_string(key, value)?),
            "workdir" => config.workdir = Some(as_string(key, value)?),
            "cache-volumes" => config.cache_volumes = Some(as_string_array(key, value)?),
            "forward-env" => config.forward_env = Some(as_string_array(key, value)?),
            "devcontainer" => config.devcontainer = Some(as_bool(key, value)?),
            "native-when" => config.native_when = Some(parse_native_when(value)?),
            "cluster" => bail!(
                "nested [container.cluster] is no longer supported; declare the cluster at the \
                 top level as [cluster]"
            ),
            "image-output-dir" => bail!(
                "[container] image-output-dir is no longer supported; set image-output-dir as a \
                 top-level key"
            ),
            other => bail!(
                "unknown key '[container] {other}' (valid keys: enabled, image, dockerfile, \
                 extends, build-args, build-secrets, hash-inputs, engine, name, workdir, \
                 cache-volumes, forward-env, devcontainer, native-when)"
            ),
        }
    }
    Ok(config)
}

/// Parse `[[image]]` (an array of tables).
fn parse_images(item: &Item) -> Result<Vec<ImageSpec>, AppError> {
    let array = item
        .as_array_of_tables()
        .ok_or_else(|| app_err!("[[image]] must be an array of tables"))?;
    let mut images = Vec::with_capacity(array.len());
    for table in array {
        images.push(parse_image(table)?);
    }
    Ok(images)
}

fn parse_image(table: &toml_edit::Table) -> Result<ImageSpec, AppError> {
    let mut name = None;
    let mut repository = None;
    let mut dockerfile = None;
    let mut target = None;
    let mut context = None;
    let mut stage_artifacts = Vec::new();
    let mut build_args = Vec::new();
    let mut depends_on = Vec::new();
    for (key, value) in table {
        match key {
            "name" => name = Some(as_string(key, value)?),
            "repository" => repository = Some(as_string(key, value)?),
            "dockerfile" => dockerfile = Some(as_string(key, value)?),
            "target" => target = Some(as_string(key, value)?),
            "context" => context = Some(as_string(key, value)?),
            "stage-artifacts" => stage_artifacts = parse_stage_artifacts(value)?,
            "build-args" => build_args = parse_string_map(key, value)?,
            "depends-on" => depends_on = as_string_array(key, value)?,
            other => bail!(
                "unknown key '[[image]] {other}' (valid keys: name, repository, dockerfile, \
                 target, context, stage-artifacts, build-args, depends-on)"
            ),
        }
    }
    Ok(ImageSpec {
        name: name.ok_or_else(|| app_err!("[[image]] requires a `name`"))?,
        repository,
        dockerfile: dockerfile.ok_or_else(|| app_err!("[[image]] requires a `dockerfile`"))?,
        target,
        context: context.ok_or_else(|| app_err!("[[image]] requires a `context`"))?,
        stage_artifacts,
        build_args,
        depends_on,
    })
}

fn parse_stage_artifacts(item: &Item) -> Result<Vec<StageArtifact>, AppError> {
    let array = item
        .as_array()
        .ok_or_else(|| app_err!("'stage-artifacts' must be an array of {{ from, to }} tables"))?;
    let mut out = Vec::with_capacity(array.len());
    for value in array {
        let table = value
            .as_inline_table()
            .ok_or_else(|| app_err!("each 'stage-artifacts' entry must be a {{ from, to }} table"))?;
        let mut from = None;
        let mut to = None;
        for (key, entry) in table {
            match key {
                "from" => from = Some(inline_string(key, entry)?),
                "to" => to = Some(inline_string(key, entry)?),
                other => bail!("unknown key 'stage-artifacts {other}' (valid keys: from, to)"),
            }
        }
        out.push(StageArtifact {
            from: from.ok_or_else(|| app_err!("'stage-artifacts' entry requires `from`"))?,
            to: to.ok_or_else(|| app_err!("'stage-artifacts' entry requires `to`"))?,
        });
    }
    Ok(out)
}

/// Parse the `[cluster]` table.
fn parse_cluster(item: &Item) -> Result<ClusterConfig, AppError> {
    let table = item.as_table_like().ok_or_else(|| app_err!("[cluster] must be a table"))?;
    let mut cluster = ClusterConfig::default();
    for (key, value) in table.iter() {
        match key {
            "name" => cluster.name = as_string(key, value)?,
            "node-image" => cluster.node_image = Some(as_string(key, value)?),
            "workers" => cluster.workers = as_u32(key, value)?,
            "load-images" => cluster.load_images = as_string_array(key, value)?,
            "dependency" => cluster.dependencies = parse_dependencies(value)?,
            "chart" => cluster.charts = parse_charts(value)?,
            "diagnostics" => cluster.diagnostics = Some(parse_diagnostics(value)?),
            "retry" => cluster.retry = parse_retry(value)?,
            "hooks" => cluster.hooks = parse_hooks(value)?,
            other => bail!(
                "unknown key '[cluster] {other}' (valid keys: name, node-image, \
                 workers, load-images, dependency, chart, diagnostics, retry, hooks)"
            ),
        }
    }
    Ok(cluster)
}

fn parse_dependencies(item: &Item) -> Result<Vec<ClusterDependency>, AppError> {
    let array = item
        .as_array_of_tables()
        .ok_or_else(|| app_err!("[[cluster.dependency]] must be an array of tables"))?;
    let mut out = Vec::with_capacity(array.len());
    for table in array {
        let mut name = None;
        let mut manifest = None;
        let mut version = None;
        let mut namespace = None;
        let mut preload_images = Vec::new();
        let mut wait = Vec::new();
        for (key, value) in table {
            match key {
                "name" => name = Some(as_string(key, value)?),
                "manifest" => manifest = Some(as_string(key, value)?),
                "version" => version = Some(as_string(key, value)?),
                "namespace" => namespace = Some(as_string(key, value)?),
                "preload-images" => preload_images = as_string_array(key, value)?,
                "wait" => wait = as_string_array(key, value)?,
                other => bail!(
                    "unknown key '[[cluster.dependency]] {other}' (valid keys: name, \
                     manifest, version, namespace, preload-images, wait)"
                ),
            }
        }
        out.push(ClusterDependency {
            name: name.ok_or_else(|| app_err!("[[cluster.dependency]] requires a `name`"))?,
            manifest: manifest.ok_or_else(|| app_err!("[[cluster.dependency]] requires a `manifest`"))?,
            version,
            namespace,
            preload_images,
            wait,
        });
    }
    Ok(out)
}

fn parse_charts(item: &Item) -> Result<Vec<ClusterChart>, AppError> {
    let array = item
        .as_array_of_tables()
        .ok_or_else(|| app_err!("[[cluster.chart]] must be an array of tables"))?;
    let mut out = Vec::with_capacity(array.len());
    for table in array {
        let mut name = None;
        let mut path = None;
        let mut namespace = None;
        let mut crds = None;
        let mut set = Vec::new();
        let mut wait = Vec::new();
        for (key, value) in table {
            match key {
                "name" => name = Some(as_string(key, value)?),
                "path" => path = Some(as_string(key, value)?),
                "namespace" => namespace = Some(as_string(key, value)?),
                "crds" => crds = Some(as_string(key, value)?),
                "set" => set = parse_string_map(key, value)?,
                "wait" => wait = as_string_array(key, value)?,
                other => bail!(
                    "unknown key '[[cluster.chart]] {other}' (valid keys: name, path, \
                     namespace, crds, set, wait)"
                ),
            }
        }
        out.push(ClusterChart {
            name: name.ok_or_else(|| app_err!("[[cluster.chart]] requires a `name`"))?,
            path: path.ok_or_else(|| app_err!("[[cluster.chart]] requires a `path`"))?,
            namespace,
            crds,
            set,
            wait,
        });
    }
    Ok(out)
}

fn parse_diagnostics(item: &Item) -> Result<ClusterDiagnostics, AppError> {
    let table = item
        .as_table_like()
        .ok_or_else(|| app_err!("[cluster.diagnostics] must be a table"))?;
    let mut diagnostics = ClusterDiagnostics::default();
    for (key, value) in table.iter() {
        match key {
            "resources" => diagnostics.resources = as_string_array(key, value)?,
            "logs" => diagnostics.logs = as_string_array(key, value)?,
            "namespace" => diagnostics.namespace = Some(as_string(key, value)?),
            other => bail!("unknown key '[cluster.diagnostics] {other}' (valid keys: resources, logs, namespace)"),
        }
    }
    Ok(diagnostics)
}

fn parse_retry(item: &Item) -> Result<ClusterRetry, AppError> {
    let table = item.as_table_like().ok_or_else(|| app_err!("[cluster.retry] must be a table"))?;
    let mut retry = ClusterRetry::default();
    for (key, value) in table.iter() {
        match key {
            "attempts" => retry.attempts = as_u32(key, value)?,
            "delay-seconds" => retry.delay_seconds = as_u32(key, value)?,
            other => bail!("unknown key '[cluster.retry] {other}' (valid keys: attempts, delay-seconds)"),
        }
    }
    Ok(retry)
}

fn parse_hooks(item: &Item) -> Result<ClusterHooks, AppError> {
    let table = item.as_table_like().ok_or_else(|| app_err!("[cluster.hooks] must be a table"))?;
    let mut hooks = ClusterHooks::default();
    for (key, value) in table.iter() {
        match key {
            "pre-install" => hooks.pre_install = Some(as_string(key, value)?),
            "post-install" => hooks.post_install = Some(as_string(key, value)?),
            "pre-test" => hooks.pre_test = Some(as_string(key, value)?),
            "on-failure" => hooks.on_failure = Some(as_string(key, value)?),
            other => bail!(
                "unknown key '[cluster.hooks] {other}' (valid keys: pre-install, \
                 post-install, pre-test, on-failure)"
            ),
        }
    }
    Ok(hooks)
}

fn parse_native_when(item: &Item) -> Result<NativeWhen, AppError> {
    let table = item
        .as_table_like()
        .ok_or_else(|| app_err!("[container.native-when] must be a table"))?;

    let mut native = NativeWhen::default();
    for (key, value) in table.iter() {
        match key {
            "os-release-id" => native.os_release_id = Some(as_string(key, value)?),
            "version-id" => native.version_id = Some(as_string(key, value)?),
            other => bail!("unknown key '[container.native-when] {other}' (valid keys: os-release-id, version-id)"),
        }
    }
    Ok(native)
}

fn as_bool(key: &str, item: &Item) -> Result<bool, AppError> {
    item.as_bool().ok_or_else(|| app_err!("'{key}' must be a boolean"))
}

fn as_u32(key: &str, item: &Item) -> Result<u32, AppError> {
    let value = item.as_integer().ok_or_else(|| app_err!("'{key}' must be an integer"))?;
    u32::try_from(value).map_err(|_err| app_err!("'{key}' must be a non-negative integer that fits in u32"))
}

/// A string value from an inline-table entry (a `&Value`, not an `&Item`).
fn inline_string(key: &str, value: &toml_edit::Value) -> Result<String, AppError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| app_err!("'{key}' must be a string"))
}

/// Parse an inline table of string→string pairs, preserving declaration order
/// (used for `build-args` and chart `set`).
fn parse_string_map(key: &str, item: &Item) -> Result<Vec<(String, String)>, AppError> {
    let table = item
        .as_table_like()
        .ok_or_else(|| app_err!("'{key}' must be a table of string values"))?;
    let mut out = Vec::new();
    for (name, value) in table.iter() {
        let text = value
            .as_str()
            .ok_or_else(|| app_err!("'{key}' value for '{name}' must be a string"))?;
        out.push((name.to_owned(), text.to_owned()));
    }
    Ok(out)
}

fn as_string(key: &str, item: &Item) -> Result<String, AppError> {
    item.as_str().map(str::to_owned).ok_or_else(|| app_err!("'{key}' must be a string"))
}

fn as_string_array(key: &str, item: &Item) -> Result<Vec<String>, AppError> {
    let array = item.as_array().ok_or_else(|| app_err!("'{key}' must be an array of strings"))?;
    let mut out = Vec::with_capacity(array.len());
    for value in array {
        let entry = value.as_str().ok_or_else(|| app_err!("'{key}' entries must be strings"))?;
        out.push(entry.to_owned());
    }
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_all_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config, AnvilConfig::default());
        let resolved = config.container.resolve("myrepo").unwrap();
        assert!(!resolved.enabled);
        assert!(!resolved.devcontainer);
        assert_eq!(resolved.engine, Engine::Auto);
        assert_eq!(resolved.workdir, "/workspaces/myrepo");
        assert_eq!(resolved.cache_volumes, vec!["cargo", "rustup"]);
        assert!(resolved.forward_env.is_empty());
    }

    #[test]
    fn full_container_section_parses() {
        let text = r#"
[container]
enabled = true
image = "ghcr.io/acme/rust-dev:1.2.3"
engine = "podman"
workdir = "/work/repo"
cache-volumes = ["cargo", "rustup", "target"]
forward-env = ["CARGO_*", "RUST*"]
devcontainer = true

[container.native-when]
os-release-id = "ubuntu"
version-id = "22.04"
"#;
        let config = parse(text).unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert!(resolved.enabled);
        assert_eq!(resolved.image, "ghcr.io/acme/rust-dev:1.2.3");
        assert_eq!(resolved.engine, Engine::Podman);
        assert_eq!(resolved.workdir, "/work/repo");
        assert_eq!(resolved.cache_volumes, vec!["cargo", "rustup", "target"]);
        assert_eq!(resolved.forward_env, vec!["CARGO_*", "RUST*"]);
        assert!(resolved.devcontainer);
        let native = resolved.native_when.unwrap();
        assert_eq!(native.os_release_id.as_deref(), Some("ubuntu"));
        assert_eq!(native.version_id.as_deref(), Some("22.04"));
    }

    #[test]
    fn enabled_without_image_builds_the_default_image() {
        let config = parse("[container]\nenabled = true\n").unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        // No image plumbing configured is the self-sustaining case, not an
        // error: anvil supplies the Dockerfile and builds it locally.
        assert_eq!(resolved.image_source, ExecImageSource::BuildDefault);
        assert!(resolved.image_source.builds());
    }

    #[test]
    fn image_alone_selects_pull() {
        let config = parse("[container]\nenabled = true\nimage = \"example.io/dev:1\"\n").unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert_eq!(resolved.image_source, ExecImageSource::Pull);
        assert!(!resolved.image_source.builds());
        assert_eq!(resolved.image, "example.io/dev:1");
    }

    #[test]
    fn dockerfile_alone_selects_a_repo_build() {
        let config = parse("[container]\nenabled = true\ndockerfile = \"ci/Dockerfile\"\n").unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert_eq!(resolved.image_source, ExecImageSource::BuildRepo);
        assert_eq!(resolved.dockerfile, "ci/Dockerfile");
    }

    #[test]
    fn image_and_dockerfile_together_are_an_error() {
        let config = parse("[container]\nenabled = true\nimage = \"a:1\"\ndockerfile = \"D\"\n").unwrap();
        let err = config.container.resolve("repo").unwrap_err().to_string();
        assert!(err.contains("sets image and dockerfile"), "got: {err}");
    }

    #[test]
    fn extends_alone_selects_an_extended_build() {
        let config = parse("[container]\nenabled = true\nextends = \"ci/ext.Dockerfile\"\n").unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert_eq!(resolved.image_source, ExecImageSource::BuildExtended);
        assert_eq!(resolved.extends, "ci/ext.Dockerfile");
        // The base underneath is still anvil's own, so its Dockerfile is
        // emitted and both halves are built.
        assert!(resolved.image_source.builds());
        assert!(resolved.image_source.builds_anvil_base());
    }

    /// Every pair of source keys is an error, not just the first one written.
    #[test]
    fn any_two_sources_together_are_an_error() {
        for (a, b) in [
            ("image = \"a:1\"", "dockerfile = \"D\""),
            ("image = \"a:1\"", "extends = \"E\""),
            ("dockerfile = \"D\"", "extends = \"E\""),
        ] {
            let config = parse(&format!("[container]\nenabled = true\n{a}\n{b}\n")).unwrap();
            let err = config.container.resolve("repo").unwrap_err().to_string();
            assert!(err.contains("alternatives"), "expected a conflict for {a} + {b}, got: {err}");
        }
    }

    /// A devcontainer descriptor names one image or one build, so it cannot
    /// describe a base plus a layer on top. Rejecting the combination beats
    /// emitting a descriptor the editor cannot open.
    #[test]
    fn extends_with_devcontainer_is_an_error() {
        let config = parse("[container]\nenabled = true\nextends = \"E\"\ndevcontainer = true\n").unwrap();
        let err = config.container.resolve("repo").unwrap_err().to_string();
        assert!(err.contains("cannot be combined with `extends`"), "got: {err}");
    }

    /// A catalog default naming a pre-built image must not collide with a
    /// repository that chooses to build its own: picking either field replaces
    /// the whole source choice rather than merging with it.
    #[test]
    fn choosing_a_source_does_not_inherit_the_other() {
        let base = parse("[container]\nenabled = true\nimage = \"catalog.io/dev:1\"\n")
            .unwrap()
            .container;
        let repo = parse("[container]\ndockerfile = \"ci/Dockerfile\"\n").unwrap().container;
        let resolved = repo.overlay(&base).resolve("repo").unwrap();
        assert_eq!(resolved.image_source, ExecImageSource::BuildRepo);
        assert!(resolved.image.is_empty());
    }

    #[test]
    fn build_inputs_round_trip() {
        let config = parse(
            "[container]\nenabled = true\ndockerfile = \"ci/Dockerfile\"\n\
             build-args = { RUST_CHANNEL = \"1.95\" }\n\
             build-secrets = [\"id=tok,env=TOK\"]\n\
             hash-inputs = [\"ci/setup.sh\"]\n",
        )
        .unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert_eq!(resolved.build_args, vec![("RUST_CHANNEL".to_owned(), "1.95".to_owned())]);
        assert_eq!(resolved.build_secrets, vec!["id=tok,env=TOK"]);
        assert_eq!(resolved.hash_inputs, vec!["ci/setup.sh"]);
    }

    /// An explicitly empty value clears the source slot rather than failing.
    /// This is the only way for a repository to opt out of a catalog-provided
    /// `image`: deleting the key would simply re-inherit it.
    #[test]
    fn blank_image_clears_a_catalog_provided_image() {
        let base = parse("[container]\nenabled = true\nimage = \"catalog.io/dev:1\"\n")
            .unwrap()
            .container;
        let repo = parse("[container]\nimage = \"\"\n").unwrap().container;
        let resolved = repo.overlay(&base).resolve("repo").unwrap();
        assert_eq!(resolved.image_source, ExecImageSource::BuildDefault);
    }

    #[test]
    fn disabled_without_image_is_fine() {
        let config = parse("[container]\nenabled = false\n").unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert!(!resolved.enabled);
    }

    #[test]
    fn unknown_top_level_key_errors() {
        let err = parse("[widget]\nname = \"x\"\n").unwrap_err().to_string();
        assert!(err.contains("unknown top-level key 'widget'"), "got: {err}");
    }

    #[test]
    fn omitted_artifacts_key_is_none() {
        // No `[anvil]` section at all: the allow-list is absent (every group).
        assert_eq!(parse("").unwrap().artifacts, None);
        // An `[anvil]` section present but without `artifacts`: still absent.
        assert_eq!(parse("[anvil]\n").unwrap().artifacts, None);
    }

    #[test]
    fn valid_artifacts_list_parses_into_a_set() {
        let config = parse("[anvil]\nartifacts = [\"recipes\", \"container\"]\n").unwrap();
        let groups = config.artifacts.expect("artifacts key present");
        assert_eq!(groups, [ArtifactGroup::Recipes, ArtifactGroup::Container].into_iter().collect());
    }

    #[test]
    fn all_four_groups_parse() {
        let config = parse("[anvil]\nartifacts = [\"recipes\", \"config\", \"backends\", \"container\"]\n").unwrap();
        assert_eq!(config.artifacts.unwrap(), ArtifactGroup::all_set());
    }

    #[test]
    fn duplicate_group_names_collapse() {
        // A set, so a repeated name is not an error and collapses.
        let config = parse("[anvil]\nartifacts = [\"config\", \"config\"]\n").unwrap();
        assert_eq!(config.artifacts.unwrap(), std::iter::once(ArtifactGroup::Config).collect());
    }

    #[test]
    fn unknown_artifact_group_errors_with_valid_list() {
        let err = parse("[anvil]\nartifacts = [\"recipes\", \"widgets\"]\n").unwrap_err().to_string();
        assert!(err.contains("unknown artifact group 'widgets'"), "got: {err}");
        assert!(
            err.contains("recipes, config, backends, container"),
            "error must list the valid groups: {err}"
        );
    }

    #[test]
    fn empty_artifacts_list_errors() {
        let err = parse("[anvil]\nartifacts = []\n").unwrap_err().to_string();
        assert!(err.contains("empty list"), "got: {err}");
        assert!(err.contains("remove the `artifacts` key"), "error must suggest removal: {err}");
    }

    #[test]
    fn unknown_anvil_key_errors() {
        let err = parse("[anvil]\ngroups = [\"recipes\"]\n").unwrap_err().to_string();
        assert!(err.contains("unknown key '[anvil] groups'"), "got: {err}");
    }

    #[test]
    fn artifacts_wrong_type_errors() {
        let err = parse("[anvil]\nartifacts = \"recipes\"\n").unwrap_err().to_string();
        assert!(err.contains("must be an array of strings"), "got: {err}");
    }

    #[test]
    fn artifacts_coexists_with_container_section() {
        let text = "[anvil]\nartifacts = [\"recipes\", \"container\"]\n\n\
             [container]\nenabled = true\nimage = \"img:1\"\n";
        let config = parse(text).unwrap();
        assert_eq!(
            config.artifacts.unwrap(),
            [ArtifactGroup::Recipes, ArtifactGroup::Container].into_iter().collect()
        );
        assert!(config.container.resolve("repo").unwrap().enabled);
    }

    #[test]
    fn artifact_group_names_round_trip() {
        for group in ArtifactGroup::ALL {
            assert_eq!(ArtifactGroup::parse(group.as_str()).unwrap(), group);
        }
    }

    #[test]
    fn top_level_image_cluster_and_output_dir_are_accepted() {
        // The three new sibling sections parse at the top level, next to
        // `[container]`, without the old collision.
        let text = "image-output-dir = \"out\"\n\n\
             [container]\nenabled = true\nimage = \"img:1\"\n\n\
             [[image]]\nname = \"svc\"\ndockerfile = \"D\"\ncontext = \"out/svc\"\n\n\
             [cluster]\nname = \"k\"\n";
        let resolved = parse(text).unwrap().container.resolve("repo").unwrap();
        assert!(resolved.enabled);
        assert_eq!(resolved.image, "img:1");
        assert_eq!(resolved.image_output_dir, "out");
        assert_eq!(resolved.images.len(), 1);
        assert!(resolved.cluster.is_some());
    }

    #[test]
    fn nested_image_array_is_rejected_with_migration_hint() {
        let err = parse("[container]\nenabled = true\n\n[[container.image]]\nname = \"a\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("top level as [[image]]"), "got: {err}");
    }

    #[test]
    fn unknown_container_key_errors() {
        let err = parse("[container]\nenabledd = true\n").unwrap_err().to_string();
        assert!(err.contains("unknown key '[container] enabledd'"), "got: {err}");
    }

    #[test]
    fn unknown_native_when_key_errors() {
        let err = parse("[container.native-when]\nkernel = \"6\"\n").unwrap_err().to_string();
        assert!(err.contains("unknown key '[container.native-when] kernel'"), "got: {err}");
    }

    #[test]
    fn invalid_engine_errors() {
        let err = parse("[container]\nengine = \"containerd\"\n").unwrap_err().to_string();
        assert!(err.contains("invalid [container] engine 'containerd'"), "got: {err}");
    }

    #[test]
    fn ill_typed_value_errors() {
        parse("[container]\nenabled = \"yes\"\n").unwrap_err();
        parse("[container]\nimage = 3\n").unwrap_err();
        parse("[container]\ncache-volumes = \"cargo\"\n").unwrap_err();
        parse("[container]\ncache-volumes = [1, 2]\n").unwrap_err();
    }

    #[test]
    fn catalog_default_supplies_image_user_enables() {
        // Catalog pre-fills the image; user only flips `enabled`.
        let catalog = ContainerConfig {
            image: Some("ghcr.io/acme/base:1".to_owned()),
            ..ContainerConfig::default()
        };
        let user = parse("[container]\nenabled = true\n").unwrap().container;
        let merged = user.overlay(&catalog);
        let resolved = merged.resolve("repo").unwrap();
        assert!(resolved.enabled);
        assert_eq!(resolved.image, "ghcr.io/acme/base:1");
    }

    #[test]
    fn user_value_overrides_catalog_field_by_field() {
        let catalog = ContainerConfig {
            enabled: Some(true),
            image: Some("ghcr.io/acme/base:1".to_owned()),
            engine: Some(Engine::Docker),
            ..ContainerConfig::default()
        };
        // User overrides only the image and engine; enabled inherited.
        let user = parse("[container]\nimage = \"ghcr.io/acme/other:2\"\nengine = \"podman\"\n")
            .unwrap()
            .container;
        let merged = user.overlay(&catalog);
        let resolved = merged.resolve("repo").unwrap();
        assert!(resolved.enabled, "enabled inherited from catalog default");
        assert_eq!(resolved.image, "ghcr.io/acme/other:2");
        assert_eq!(resolved.engine, Engine::Podman);
    }

    #[test]
    fn empty_cache_volumes_overrides_default() {
        let config = parse("[container]\ncache-volumes = []\n").unwrap();
        let resolved = config.container.resolve("repo").unwrap();
        assert!(resolved.cache_volumes.is_empty(), "explicit [] overrides the default");
    }

    // -----------------------------------------------------------------------
    // Pillar 2 — [[image]]
    // -----------------------------------------------------------------------

    const IMAGES_TOML: &str = r#"
image-output-dir = "out"

[[image]]
name = "base-image"
dockerfile = "containers/base/Dockerfile"
context = "out/base"

[[image]]
name = "my-service"
dockerfile = "containers/svc/Dockerfile"
target = "runtime"
context = "out/svc"
build-args = { BASE_IMAGE = "mcr.example/base:1" }
depends-on = ["base-image"]
stage-artifacts = [
  { from = "target/{profile}/my-svc", to = "bin/my-svc" },
]
"#;

    #[test]
    fn image_section_parses_all_fields() {
        let resolved = parse(IMAGES_TOML).unwrap().container.resolve("repo").unwrap();
        assert_eq!(resolved.images.len(), 2);
        let svc = &resolved.images[1];
        assert_eq!(svc.name, "my-service");
        assert_eq!(svc.dockerfile, "containers/svc/Dockerfile");
        assert_eq!(svc.target.as_deref(), Some("runtime"));
        assert_eq!(svc.context, "out/svc");
        assert_eq!(svc.build_args, vec![("BASE_IMAGE".to_owned(), "mcr.example/base:1".to_owned())]);
        assert_eq!(svc.depends_on, vec!["base-image".to_owned()]);
        assert_eq!(svc.stage_artifacts.len(), 1);
        assert_eq!(svc.stage_artifacts[0].from, "target/{profile}/my-svc");
        assert_eq!(svc.stage_artifacts[0].to, "bin/my-svc");
    }

    /// Resolve from a top-level `[[image]]`/`[cluster]`/`image-output-dir` body
    /// with no `[container]` section. Container execution stays disabled, so
    /// the exec-image requirement never fires — image/cluster validation runs
    /// on its own, independent of pillar 1.
    fn resolve_body(body: &str) -> Result<ResolvedContainer, AppError> {
        parse(body).unwrap().container.resolve("repo")
    }

    /// Without `[container] name`, identity falls back to the repo-root
    /// directory name. This is convenient but *not* stable across checkouts.
    #[test]
    fn identity_falls_back_to_the_directory_name() {
        let body = "[container]\nenabled = true\nimage = \"img:1\"\n";
        let resolved = parse(body).unwrap().container.resolve("some-dir").unwrap();
        assert_eq!(resolved.repo_name, "some-dir");
        assert_eq!(resolved.workdir, "/workspaces/some-dir");
    }

    /// An explicit `[container] name` pins the identity, so the emitted
    /// `container.just` is byte-identical no matter where the repository is
    /// cloned. Without this, a CI agent cloning into `s`, a git worktree, and a
    /// developer's clone each render different content and any regeneration
    /// drift check fails.
    #[test]
    fn explicit_name_makes_identity_independent_of_the_checkout_directory() {
        let body = "[container]\nenabled = true\nimage = \"img:1\"\nname = \"cosmicrust\"\n";
        let config = parse(body).unwrap().container;

        let from_clone = config.resolve("COSMICRust").unwrap();
        let from_worktree = config.resolve("COSMICRust-container").unwrap();
        let from_ci = config.resolve("s").unwrap();

        for resolved in [&from_clone, &from_worktree, &from_ci] {
            assert_eq!(resolved.repo_name, "cosmicrust");
            assert_eq!(resolved.workdir, "/workspaces/cosmicrust");
        }
        assert_eq!(from_clone, from_ci);
        assert_eq!(from_clone, from_worktree);
    }

    /// An explicit `workdir` still wins over the derived default.
    #[test]
    fn explicit_workdir_overrides_the_name_derived_default() {
        let body = "[container]\nenabled = true\nimage = \"img:1\"\nname = \"x\"\nworkdir = \"/src\"\n";
        let resolved = parse(body).unwrap().container.resolve("dir").unwrap();
        assert_eq!(resolved.repo_name, "x");
        assert_eq!(resolved.workdir, "/src");
    }

    /// A dependency that installs into its own namespace must be able to say
    /// so: its `wait` rollout targets are resolved against that namespace.
    /// Without it, waiting on `deployment/cert-manager-webhook` looks in
    /// `default` and fails, so the charts install before the dependency's
    /// webhook is serving and Helm dies calling it.
    #[test]
    fn dependency_namespace_is_parsed_for_wait_resolution() {
        let body = "\n[cluster]\nname = \"c\"\n\n[[cluster.dependency]]\nname = \"cert-manager\"\n\
                    manifest = \"https://example/cert-manager.yaml\"\nnamespace = \"cert-manager\"\n\
                    wait = [\"deployment/cert-manager-webhook\"]\n";
        let resolved = resolve_body(body).unwrap();
        let dep = &resolved.cluster.as_ref().unwrap().dependencies[0];
        assert_eq!(dep.namespace.as_deref(), Some("cert-manager"));
        assert_eq!(dep.wait, vec!["deployment/cert-manager-webhook".to_owned()]);
    }

    /// The namespace is optional; omitting it keeps the previous shape.
    #[test]
    fn dependency_namespace_is_optional() {
        let body = "\n[cluster]\nname = \"c\"\n\n[[cluster.dependency]]\nname = \"d\"\nmanifest = \"m\"\n";
        let resolved = resolve_body(body).unwrap();
        assert_eq!(resolved.cluster.as_ref().unwrap().dependencies[0].namespace, None);
    }

    /// Diagnostics log targets are namespaced too: without a namespace they
    /// resolve in `default` and report "not found" instead of logs, precisely
    /// when a failure has occurred and the logs are most needed.
    #[test]
    fn diagnostics_namespace_is_parsed() {
        let body = "\n[cluster]\nname = \"c\"\n\n[cluster.diagnostics]\n\
                    logs = [\"deployment/demo\"]\nnamespace = \"demo-system\"\n";
        let resolved = resolve_body(body).unwrap();
        let diag = resolved.cluster.as_ref().unwrap().diagnostics.as_ref().unwrap();
        assert_eq!(diag.namespace.as_deref(), Some("demo-system"));
        assert_eq!(diag.logs, vec!["deployment/demo".to_owned()]);
    }

    /// A published image path can differ from the recipe name: `/` is not a
    /// valid `just` recipe token, so a repository grouping its images under a
    /// namespace needs `repository` to carry the real path. The cluster loader
    /// must resolve `load-images` names through it, or the images land in the
    /// nodes under a reference no pod ever requests.
    #[test]
    fn image_repository_overrides_the_reference_path() {
        let body = "\n[[image]]\nname = \"cs-agent\"\nrepository = \"cosmic-sandbox/cs-agent\"\n\
                    dockerfile = \"D\"\ncontext = \"out/a\"\n\n[cluster]\nname = \"c\"\n\
                    load-images = [\"cs-agent\"]\n";
        let resolved = resolve_body(body).unwrap();
        assert_eq!(resolved.images[0].repository.as_deref(), Some("cosmic-sandbox/cs-agent"));
        // The recipe name stays a valid just token even though the path is nested.
        assert_eq!(resolved.images[0].name, "cs-agent");
    }

    /// Omitting `repository` keeps the previous behaviour: path == name.
    #[test]
    fn image_repository_defaults_to_the_name() {
        let body = "\n[[image]]\nname = \"svc\"\ndockerfile = \"D\"\ncontext = \"out/a\"\n";
        let resolved = resolve_body(body).unwrap();
        assert_eq!(resolved.images[0].repository, None);
    }

    #[test]
    fn image_defaults_context_output_dir_to_out() {
        let resolved = resolve_body("\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a\"\n").unwrap();
        assert_eq!(resolved.image_output_dir, "out");
    }

    #[test]
    fn image_context_outside_output_dir_is_rejected() {
        let err = resolve_body("image-output-dir = \"out\"\n\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"src/a\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must live under the image output dir"), "got: {err}");
    }

    #[test]
    fn image_context_equal_to_output_dir_is_rejected() {
        resolve_body("image-output-dir = \"out\"\n\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out\"\n").unwrap_err();
    }

    #[test]
    fn image_context_escaping_dotdot_is_rejected() {
        resolve_body("image-output-dir = \"out\"\n\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/../src\"\n").unwrap_err();
    }

    #[test]
    fn image_invalid_name_is_rejected() {
        let err = resolve_body("\n[[image]]\nname = \"1bad\"\ndockerfile = \"D\"\ncontext = \"out/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must match [A-Za-z]"), "got: {err}");
    }

    #[test]
    fn image_duplicate_name_is_rejected() {
        let err = resolve_body("\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a\"\n\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a2\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn image_empty_dockerfile_is_rejected() {
        resolve_body("\n[[image]]\nname = \"a\"\ndockerfile = \"\"\ncontext = \"out/a\"\n").unwrap_err();
    }

    #[test]
    fn image_depends_on_unknown_is_rejected() {
        let err = resolve_body("\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a\"\ndepends-on = [\"ghost\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no image"), "got: {err}");
    }

    #[test]
    fn image_unknown_key_is_rejected() {
        let text = "[container]\nenabled = true\n\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\nbogus = 1\n";
        parse(text).unwrap_err();
    }

    #[test]
    fn topo_order_is_deterministic_and_respects_deps() {
        // c depends on b, b depends on a; declared out of order.
        let resolved = resolve_body(
            "\n[[image]]\nname = \"c\"\ndockerfile = \"D\"\ncontext = \"out/c\"\ndepends-on = [\"b\"]\n\n\
             [[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a\"\n\n\
             [[image]]\nname = \"b\"\ndockerfile = \"D\"\ncontext = \"out/b\"\ndepends-on = [\"a\"]\n",
        )
        .unwrap();
        assert_eq!(resolved.image_build_order, vec!["a", "b", "c"]);
    }

    #[test]
    fn topo_order_breaks_ties_by_declaration_order() {
        // Two roots then a dependent: order is declaration order for the roots.
        let resolved = resolve_body(
            "\n[[image]]\nname = \"z\"\ndockerfile = \"D\"\ncontext = \"out/z\"\n\n\
             [[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a\"\n",
        )
        .unwrap();
        assert_eq!(resolved.image_build_order, vec!["z", "a"]);
    }

    #[test]
    fn depends_on_cycle_is_rejected() {
        let err = resolve_body(
            "\n[[image]]\nname = \"a\"\ndockerfile = \"D\"\ncontext = \"out/a\"\ndepends-on = [\"b\"]\n\n\
             [[image]]\nname = \"b\"\ndockerfile = \"D\"\ncontext = \"out/b\"\ndepends-on = [\"a\"]\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // Pillar 3 — [cluster]
    // -----------------------------------------------------------------------

    const CLUSTER_TOML: &str = r#"
[[image]]
name = "my-service"
dockerfile = "containers/svc/Dockerfile"
context = "out/svc"

[cluster]
name = "anvil-kind"
node-image = "kindest/node:v1.31.0"
workers = 2
load-images = ["my-service"]

[[cluster.dependency]]
name = "cert-manager"
manifest = "https://example.com/cert-manager.yaml"
version = "v1.16.1"
preload-images = ["quay.io/jetstack/cert-manager-controller:v1.16.1"]
wait = ["deployment/cert-manager-webhook"]

[[cluster.chart]]
name = "svc"
path = "charts/svc"
namespace = "svc-system"
crds = "charts/svc/crds"
set = { "image.tag" = "{tag}" }
wait = ["deployment/svc-controller"]

[cluster.diagnostics]
resources = ["pods -A -o wide", "events"]
logs = ["deployment/svc-controller"]

[cluster.retry]
attempts = 2
delay-seconds = 10

[cluster.hooks]
pre-install = "cosmic-native-auth"
"#;

    #[test]
    fn cluster_section_parses_all_fields() {
        let resolved = parse(CLUSTER_TOML).unwrap().container.resolve("repo").unwrap();
        let cluster = resolved.cluster.expect("cluster configured");
        assert_eq!(cluster.name, "anvil-kind");
        assert_eq!(cluster.node_image.as_deref(), Some("kindest/node:v1.31.0"));
        assert_eq!(cluster.workers, 2);
        assert_eq!(cluster.load_images, vec!["my-service".to_owned()]);
        assert_eq!(cluster.dependencies.len(), 1);
        let dep = &cluster.dependencies[0];
        assert_eq!(dep.name, "cert-manager");
        assert_eq!(dep.version.as_deref(), Some("v1.16.1"));
        assert_eq!(
            dep.preload_images,
            vec!["quay.io/jetstack/cert-manager-controller:v1.16.1".to_owned()]
        );
        assert_eq!(dep.wait, vec!["deployment/cert-manager-webhook".to_owned()]);
        assert_eq!(cluster.charts.len(), 1);
        let chart = &cluster.charts[0];
        assert_eq!(chart.name, "svc");
        assert_eq!(chart.namespace.as_deref(), Some("svc-system"));
        assert_eq!(chart.crds.as_deref(), Some("charts/svc/crds"));
        assert_eq!(chart.set, vec![("image.tag".to_owned(), "{tag}".to_owned())]);
        let diag = cluster.diagnostics.expect("diagnostics configured");
        assert_eq!(diag.resources, vec!["pods -A -o wide".to_owned(), "events".to_owned()]);
        assert_eq!(cluster.retry.attempts, 2);
        assert_eq!(cluster.retry.delay_seconds, 10);
        assert_eq!(cluster.hooks.pre_install.as_deref(), Some("cosmic-native-auth"));
        assert!(cluster.hooks.post_install.is_none());
    }

    #[test]
    fn cluster_defaults_apply() {
        let resolved = resolve_body("\n[cluster]\n").unwrap();
        let cluster = resolved.cluster.unwrap();
        assert_eq!(cluster.name, "anvil-kind");
        assert_eq!(cluster.workers, 0);
        assert!(cluster.node_image.is_none());
        assert_eq!(cluster.retry.attempts, 1, "retry defaults to a single attempt");
        assert_eq!(cluster.retry.delay_seconds, 0);
    }

    #[test]
    fn cluster_load_images_must_name_an_image() {
        let err = resolve_body("\n[cluster]\nload-images = [\"ghost\"]\n").unwrap_err().to_string();
        assert!(err.contains("names no [[image]]"), "got: {err}");
    }

    #[test]
    fn cluster_retry_zero_attempts_is_rejected() {
        let err = resolve_body("\n[cluster]\n\n[cluster.retry]\nattempts = 0\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("attempts must be >= 1"), "got: {err}");
    }

    #[test]
    fn cluster_chart_requires_name_and_path() {
        resolve_body("\n[cluster]\n\n[[cluster.chart]]\nname = \"svc\"\npath = \"\"\n").unwrap_err();
    }

    #[test]
    fn cluster_dependency_requires_manifest() {
        resolve_body("\n[cluster]\n\n[[cluster.dependency]]\nname = \"dep\"\nmanifest = \"\"\n").unwrap_err();
    }

    #[test]
    fn cluster_unknown_key_is_rejected() {
        let text = "[container]\nenabled = true\n\n[cluster]\nbogus = 1\n";
        parse(text).unwrap_err();
    }

    #[test]
    fn cluster_hooks_unknown_key_is_rejected() {
        let text = "[container]\nenabled = true\nimage = \"img:1\"\n\n[cluster]\n\n[cluster.hooks]\nmid-install = \"x\"\n";
        parse(text).unwrap_err();
    }

    // -----------------------------------------------------------------------
    // All three pillars together — the customer's shape
    // -----------------------------------------------------------------------

    /// The primary use case: containerized recipe execution (`[container]`),
    /// two image builds with a `depends-on` edge (`[[image]]`), and a full Kind
    /// cluster harness (`[cluster]`) — all in one config. This shape was
    /// previously inexpressible (the exec `image` string collided with the
    /// nested `[[container.image]]` array); with the sections promoted to the
    /// top level it parses and resolves cleanly.
    #[test]
    fn all_three_sections_resolve_together() {
        let text = r#"
image-output-dir = "out"

[container]
enabled = true
image = "ghcr.io/acme/rust-dev:1.2.3"
engine = "docker"

[[image]]
name = "base-image"
dockerfile = "containers/base/Dockerfile"
context = "out/base"

[[image]]
name = "my-service"
dockerfile = "containers/svc/Dockerfile"
target = "runtime"
context = "out/svc"
depends-on = ["base-image"]
stage-artifacts = [
  { from = "target/{profile}/my-svc", to = "bin/my-svc" },
]

[cluster]
name = "anvil-kind"
workers = 2
load-images = ["my-service"]

[[cluster.dependency]]
name = "cert-manager"
manifest = "https://example.com/cert-manager.yaml"
preload-images = ["quay.io/jetstack/cert-manager-controller:v1.16.1"]
wait = ["deployment/cert-manager-webhook"]

[[cluster.chart]]
name = "svc"
path = "charts/svc"
crds = "charts/svc/crds"
set = { "image.tag" = "{tag}" }
wait = ["deployment/svc-controller"]

[cluster.diagnostics]
resources = ["pods -A -o wide"]
logs = ["deployment/svc-controller"]

[cluster.retry]
attempts = 2
delay-seconds = 10

[cluster.hooks]
pre-install = "cosmic-native-auth"
on-failure = "collect-support-bundle"
"#;
        let resolved = parse(text).unwrap().container.resolve("repo").unwrap();

        // Pillar 1: exec container is enabled with its own image.
        assert!(resolved.enabled);
        assert_eq!(resolved.image, "ghcr.io/acme/rust-dev:1.2.3");
        assert_eq!(resolved.engine, Engine::Docker);

        // Pillar 2: two images, resolved in dependency order.
        assert_eq!(resolved.images.len(), 2);
        assert_eq!(resolved.image_output_dir, "out");
        assert_eq!(resolved.image_build_order, vec!["base-image", "my-service"]);

        // Pillar 3: the cluster cross-references a declared image and carries
        // its dependency, chart, diagnostics, retry and hooks.
        let cluster = resolved.cluster.expect("cluster configured");
        assert_eq!(cluster.load_images, vec!["my-service".to_owned()]);
        assert_eq!(cluster.dependencies.len(), 1);
        assert_eq!(cluster.charts.len(), 1);
        assert_eq!(cluster.charts[0].crds.as_deref(), Some("charts/svc/crds"));
        assert!(cluster.diagnostics.is_some());
        assert_eq!(cluster.retry.attempts, 2);
        assert_eq!(cluster.hooks.pre_install.as_deref(), Some("cosmic-native-auth"));
        assert_eq!(cluster.hooks.on_failure.as_deref(), Some("collect-support-bundle"));
    }
}
