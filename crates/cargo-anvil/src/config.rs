// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The user-facing `anvil.toml` configuration file.
//!
//! `anvil.toml` (repo root) is anvil's first user-facing config file. It is
//! **optional**: an absent file means byte-identical behavior to a build with
//! no container support at all. The optional `[container]` section opts a
//! repository into running the generated `just` recipes inside a container.
//!
//! The parser rejects unknown keys loudly (typo protection) while keeping the
//! top-level dispatch trivial to extend.
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
    /// The container-gated artifacts (`container.just`, its default image
    /// assets, and the devcontainer descriptor).
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
    /// source is ambiguous (more than one of `image`, `dockerfile`, and
    /// `extends` is set).
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

        // A build arg's value is not a secret-safe channel. It is folded into
        // the image identity hash and written verbatim into two files the
        // consuming repository commits: the generated `container.just` and, when
        // enabled, `.devcontainer/devcontainer.json`. A credential put here
        // therefore lands in git.
        //
        // This is a footgun guard, not a security control: it catches the
        // obvious names and cannot know that `FEED_AUTH` holds a bearer. The
        // author being protected is the author writing the config, so there is
        // no adversary to evade it.
        let build_args = self.build_args.clone().unwrap_or_default();
        for (name, _) in &build_args {
            if is_credential_shaped(name) {
                bail!(
                    "[container] build-args `{name}` looks like a credential, and build-arg \
                     values are written into generated files that are committed. Pass it as a \
                     `build-secrets` entry instead (e.g. \"id={}, env={name}\")",
                    name.to_ascii_lowercase()
                );
            }
        }

        Ok(ResolvedContainer {
            enabled,
            image,
            dockerfile,
            extends,
            image_source,
            build_args,
            build_secrets: self.build_secrets.clone().unwrap_or_default(),
            hash_inputs: self.hash_inputs.clone().unwrap_or_default(),
            engine: self.engine.unwrap_or_default(),
            workdir,
            cache_volumes,
            forward_env,
            devcontainer,
            native_when: self.native_when.clone(),
            repo_name,
        })
    }
}

/// Whether a build-arg name looks like it holds a credential.
///
/// `KEY` is deliberately absent: it matches `SSH_KEY_PATH` and `API_KEY_NAME`,
/// and a guard that cries wolf is a guard people learn to route around.
fn is_credential_shaped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "CRED"].iter().any(|needle| upper.contains(needle)) || upper.ends_with("PAT")
}

/// The parsed `anvil.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnvilConfig {
    /// The container-feature configuration from the optional `[container]`
    /// section.
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

    // Top-level dispatch: `[container]` and the `[anvil]` meta-section are
    // defined today. New sections are added by extending this match — an
    // unknown table is a typo.
    let mut container = ContainerConfig::default();
    let mut artifacts: Option<BTreeSet<ArtifactGroup>> = None;
    for (key, item) in doc.as_table() {
        match key {
            "container" => container = parse_container(item)?,
            "anvil" => artifacts = parse_anvil_section(item)?,
            other => bail!("unknown top-level key '{other}' (valid top-level items: [anvil], [container])"),
        }
    }
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
            other => bail!(
                "unknown key '[container] {other}' (valid keys: enabled, image, dockerfile, \
                 extends, build-args, build-secrets, hash-inputs, engine, name, workdir, \
                 cache-volumes, forward-env, devcontainer, native-when)"
            ),
        }
    }
    Ok(config)
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

/// Parse an inline table of string→string pairs, preserving declaration order
/// (used for `build-args`).
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

    /// A build-arg value is committed twice over — into the generated
    /// `container.just` and the devcontainer descriptor — so a credential put
    /// there lands in git. Catch the obvious names at generation time.
    #[test]
    fn credential_shaped_build_args_are_rejected() {
        for name in ["FEED_TOKEN", "my_secret", "NUGET_PASSWORD", "ADO_PAT", "GIT_CREDENTIAL"] {
            let toml = format!("[container]\nenabled = true\nbuild-args = {{ {name} = \"x\" }}\n");
            let err = parse(&toml).unwrap().container.resolve("repo").unwrap_err().to_string();
            assert!(err.contains("looks like a credential"), "{name} should be rejected: {err}");
            assert!(err.contains("build-secrets"), "{name} should name the alternative: {err}");
        }
    }

    /// The guard must not cry wolf: a name people legitimately use as a build
    /// arg has to keep working, or the guard is the thing that gets removed.
    #[test]
    fn ordinary_build_args_are_accepted() {
        for name in ["RUST_CHANNEL", "SSH_KEY_PATH", "API_KEY_NAME", "TARGETS", "PATCH_LEVEL"] {
            let toml = format!("[container]\nenabled = true\nbuild-args = {{ {name} = \"x\" }}\n");
            parse(&toml)
                .unwrap()
                .container
                .resolve("repo")
                .unwrap_or_else(|e| panic!("{name} should be accepted: {e}"));
        }
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
}
