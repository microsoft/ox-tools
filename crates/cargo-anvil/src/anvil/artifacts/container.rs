// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Containerized execution: the `anvil-container` recipe and the image it runs.
//!
//! The recipe drives the engine and computes the image identity; the Dockerfile
//! defines what the image contains; its build-context ignore file decides what
//! reaches the build at all, and the two must be replaced together. There is no
//! configuration file: whether the group is emitted at all is a catalog
//! decision, and the only host-specific value — which engine to call — is an
//! environment variable read by the recipe at run time.
//!
//! # Why the Dockerfile is composed, not owned
//!
//! The Dockerfile is a **user-composed file with managed regions**, not a
//! wholly-owned file. An owned file that invites in-place edits fails silently
//! here: anvil preserves the edit and writes its own version to
//! `.anvil-proposed`, with no three-way merge and no recorded ancestor, so a
//! repository that edits it once keeps building on the base digest and the four
//! tool pins frozen at that moment — while `anvil-container-tag` keeps
//! resolving, because the tag hashes *their* file. The identity scheme works
//! perfectly and still names a stale image.
//!
//! Splitting anvil's content into regions does not make it read-only: the same
//! ownership rules apply to a region body as to a file, and anvil never
//! overwrites repository content. What it removes is the *reason* to edit: each
//! gap between the regions is the correct home for one class of addition,
//! defined by what must already be true at that point in the build.
//!
//! | Gap | Runs | Exists for |
//! | --- | --- | --- |
//! | after [`dockerfile_base_image`] | before `FROM` | re-declaring `ARG BASE_IMAGE` to build on your own base |
//! | after [`dockerfile_base`] | before the first download | root CA, proxy, internal apt mirror |
//! | after [`dockerfile_tools`] | after the toolchain, before `anvil-setup` | libraries a catalog tool needs to *compile* |
//! | after [`dockerfile_setup`] | after the catalog is installed | what the repository's own checks need at run time |
//!
//! The base image is the case that most needed a gap of its own: it is the
//! setting a repository is likeliest to want, and leaving it inside a region
//! would have made overriding it mean editing anvil's content — freezing every
//! pin in the file to buy one substitution.
//!
//! A downstream catalog customizes by replacing individual regions, and
//! inherits the rest:
//!
//! - [`dockerfile_base_image`] / [`dockerfile_tools`] plus
//!   [`Artifact::with_body`] to build on a different base OS or install the
//!   toolchain from a different source. [`dockerfile_setup`] and
//!   [`dockerfile_entry`] are the contract with the recipe and are rarely
//!   replaced.
//! - [`dockerignore`] alongside them when the replacement copies more of the
//!   tree, or the added paths never reach the build context.
//! - [`hooks`] to supply credentials, or to resolve a published image through
//!   `Anvil-ResolveImage`. The recipe loads the file when it is present,
//!   regardless of who put it there.

use crate::catalog::{Artifact, ComposedHost, HostSelector, RegionId, RegionSpec};
use crate::region::CommentSyntax;

const RECIPE: &str = include_str!("../../../templates/justfiles/anvil/container.just");
const DOCKERIGNORE: &str = include_str!("../../../templates/anvil/container/Dockerfile.dockerignore");

/// Seeded into the Dockerfile when the file does not exist, and never
/// reconciled afterwards — it is the part of the file anvil cannot own.
///
/// `# syntax=docker/dockerfile:1` pins the `BuildKit` frontend, and `BuildKit`
/// honors the directive only as the very first line, before any comment. A
/// region's opening sentinel *is* a comment, so the directive cannot live inside
/// one without being silently demoted, dropping the build to the default
/// frontend with nothing failing to say so.
///
/// Nothing else belongs here. The scaffold is also what the engine matches a
/// host's opening bytes against when deciding where a missing first region
/// goes, so every line added is a line an existing file must already carry to
/// be recognized -- and content anvil could never correct afterwards.
const DOCKERFILE_HEADER: &str = include_str!("../../../templates/anvil/container/Dockerfile.header");

const DOCKERFILE_BASE_IMAGE: &str = include_str!("../../../templates/anvil/container/Dockerfile.baseimage.region");
const DOCKERFILE_BASE: &str = include_str!("../../../templates/anvil/container/Dockerfile.base.region");
const DOCKERFILE_TOOLS: &str = include_str!("../../../templates/anvil/container/Dockerfile.tools.region");
const DOCKERFILE_SETUP: &str = include_str!("../../../templates/anvil/container/Dockerfile.setup.region");
const DOCKERFILE_ENTRY: &str = include_str!("../../../templates/anvil/container/Dockerfile.entry.region");

/// The composed Dockerfile the managed regions are spliced into.
const DOCKERFILE_PATH: &str = ".anvil/container/Dockerfile";

const DOCKERIGNORE_PATH: &str = ".anvil/container/Dockerfile.dockerignore";

/// Repo-root-relative path of the generated container recipes.
pub(crate) const CONTAINER_JUST_PATH: &str = ".anvil/container.just";

/// The region ids anvil owns inside [`DOCKERFILE_PATH`], in the order a valid
/// Dockerfile must carry them.
///
/// Order is load-bearing in a way no other managed-region host is: the base
/// image argument must precede the `FROM` that consumes it, `FROM` must precede
/// every instruction, the toolchain must exist before `anvil-setup` runs, and
/// `WORKDIR`/`CMD` close the file. The engine checks the on-disk sequence
/// against this list and refuses rather than emitting a Dockerfile that is
/// silently wrong.
const DOCKERFILE_REGION_ORDER: &[&str] = &[
    "anvil-container-base-image",
    "anvil-container-base",
    "anvil-container-tools",
    "anvil-container-setup",
    "anvil-container-entry",
];

/// The path the recipe loads credentials from, when a file is present there.
pub const HOOKS_PATH: &str = ".anvil/container/hooks.ps1";

/// The Dockerfile as a composed host: scaffold plus the order its regions must
/// appear in.
#[must_use]
pub fn composed_host() -> ComposedHost {
    ComposedHost {
        path: DOCKERFILE_PATH,
        scaffold: DOCKERFILE_HEADER,
        order: DOCKERFILE_REGION_ORDER,
    }
}

/// The full container artifact group.
#[must_use]
pub fn all() -> Vec<Artifact> {
    let mut artifacts = recipes();
    artifacts.extend([
        dockerignore(),
        dockerfile_base_image(),
        dockerfile_base(),
        dockerfile_tools(),
        dockerfile_setup(),
        dockerfile_entry(),
    ]);
    artifacts
}

/// The container recipe template as one catalog section.
#[must_use]
pub fn recipes() -> Vec<Artifact> {
    vec![
        super::justfile::generated_header(CONTAINER_JUST_PATH, "header:container"),
        super::justfile::template_section(CONTAINER_JUST_PATH, "justfiles/anvil/container.just", RECIPE),
    ]
}

fn dockerfile_region(id: &'static str, body: &'static str) -> Artifact {
    Artifact::region(RegionSpec {
        host: HostSelector::Path(DOCKERFILE_PATH.to_owned()),
        id: RegionId::new(id),
        body: body.to_owned(),
        syntax: CommentSyntax::Hash,
    })
}

/// The default base image, digest-pinned, alone in its own region.
///
/// Separate from [`dockerfile_base`] so the gap between them is a place to
/// override it: a second `ARG BASE_IMAGE=…` there wins over this default, and
/// `FROM` in the next region consumes the repository's value. That is what lets
/// a repository choose its own base **without** editing anvil's content, which
/// would otherwise freeze every pin in the file at the moment of the edit.
#[must_use]
pub fn dockerfile_base_image() -> Artifact {
    dockerfile_region("anvil-container-base-image", DOCKERFILE_BASE_IMAGE)
}

/// `FROM`, the four download pins, and the environment every later region
/// depends on.
#[must_use]
pub fn dockerfile_base() -> Artifact {
    dockerfile_region("anvil-container-base", DOCKERFILE_BASE)
}

/// The toolchain layer: the system packages, then `pwsh`, `just`, `rustup` and
/// `cargo-binstall` installed against published checksums.
///
/// This is the region a catalog on a different package ecosystem replaces —
/// `tdnf` rather than `apt-get`, an internal toolchain source rather than
/// `rustup`.
#[must_use]
pub fn dockerfile_tools() -> Artifact {
    dockerfile_region("anvil-container-tools", DOCKERFILE_TOOLS)
}

/// The catalog install: copies the generated recipe tree and runs
/// `just anvil-setup`, so the image installs exactly what the checks pin.
///
/// Rarely replaced. Installing by running the same recipe the checks use is
/// what makes "the image has the right tools" true by construction; a
/// replacement reintroduces the second tool list this exists to avoid.
#[must_use]
pub fn dockerfile_setup() -> Artifact {
    dockerfile_region("anvil-container-setup", DOCKERFILE_SETUP)
}

/// The entry contract: `ANVIL_IN_CONTAINER`, the mount point the repository is
/// bind-mounted at, and the default command.
///
/// Every line here is a contract with the recipe rather than an opinion about
/// the image, so replacing it breaks the driver.
#[must_use]
pub fn dockerfile_entry() -> Artifact {
    dockerfile_region("anvil-container-entry", DOCKERFILE_ENTRY)
}

/// The build-context ignore file for the composed Dockerfile.
///
/// `BuildKit` reads `<dockerfile>.dockerignore` in preference to a root
/// `.dockerignore`, so the build context is scoped without the repository
/// having to own a root ignore file. A catalog that replaces a region with one
/// that copies more of the tree must replace this too.
#[must_use]
pub fn dockerignore() -> Artifact {
    Artifact::owned_file(DOCKERIGNORE_PATH, DOCKERIGNORE)
}

/// Add a credential hook at [`HOOKS_PATH`].
///
/// The public catalog emits no hook: crates.io needs no credentials, and an
/// empty script would be one more generated file to review. A downstream
/// catalog adds one with [`crate::CatalogBuilder::with_artifact`]; a single
/// repository can write the same path by hand. The recipe loads it either way.
///
/// The script may define any of three functions, and is dot-sourced before the
/// phase that needs it. Each is named for what it supplies rather than for when
/// it runs, because all three exist to provide credentials or an image, not as
/// general-purpose extension points:
///
/// - `Anvil-BuildSecrets` returns `@{ Secrets = @{ <id> = <value> } }`. Each entry
///   becomes a `BuildKit` `--secret id=<id>`, passed by environment variable
///   name so the value never reaches a process argument, and never a layer.
/// - `Anvil-RunEnv` returns `@{ Env = @{ <NAME> = <value> } }`. Each entry is
///   forwarded into the container by name, for the same reason.
/// - `Anvil-ResolveImage` takes the computed reference and returns one to use
///   instead, or nothing. A hook declaring both `Engine` and `EnginePrefix`
///   also receives the exact selected engine invocation. Declaring only one
///   supplies neither, and hooks declaring neither remain compatible.
///
/// The two credential phases are fail-closed: an empty value, a return with no
/// entries, a throw, or a script that cannot even be loaded stops the run. A
/// build that silently proceeded without its credential would install a reduced
/// tool set and then be tagged with the same content hash a credentialed build
/// produces, so every later run would reuse the broken image.
///
/// `Anvil-ResolveImage` is the opposite, and deliberately so: every failure --
/// including a hook that fails to load -- falls through to a local build, which
/// is slower but always correct. A publisher that has not caught up with a
/// change must not block the developer who made it.
///
/// The file's *content* is part of the image identity, since it decides what the
/// build installs. Its *output* deliberately is not: a credential must never
/// influence a tag.
#[must_use]
pub fn hooks(body: impl Into<String>) -> Artifact {
    Artifact::owned_file(HOOKS_PATH, body)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn paths(artifacts: &[Artifact]) -> Vec<&str> {
        artifacts
            .iter()
            .map(|artifact| match artifact {
                Artifact::OwnedFile(spec) => spec.path,
                Artifact::OwnedFileSection(_) | Artifact::Region(_) => panic!("expected an owned file"),
            })
            .collect()
    }

    fn region_ids(artifacts: &[Artifact]) -> Vec<&str> {
        artifacts
            .iter()
            .filter_map(|artifact| match artifact {
                Artifact::Region(spec) => Some(spec.id.as_str()),
                Artifact::OwnedFile(_) | Artifact::OwnedFileSection(_) => None,
            })
            .collect()
    }

    #[test]
    fn group_is_one_owned_file_one_just_section_and_five_dockerfile_regions() {
        let all = all();
        let owned: Vec<_> = all
            .iter()
            .filter(|artifact| matches!(artifact, Artifact::OwnedFile(_)))
            .cloned()
            .collect();
        assert_eq!(paths(&owned), [DOCKERIGNORE_PATH]);
        let recipe_source = recipes()
            .into_iter()
            .find(|artifact| {
                matches!(
                    artifact,
                    Artifact::OwnedFileSection(section)
                        if section.id == "source:justfiles/anvil/container.just"
                )
            })
            .expect("the container source section must be registered");
        assert!(
            matches!(
                recipe_source,
                Artifact::OwnedFileSection(section)
                    if section.path == CONTAINER_JUST_PATH
                        && section.id == "source:justfiles/anvil/container.just"
                        && section.body.contains("anvil-container *command:")
            ),
            "the container recipe must be a section of the composed Justfile"
        );
        assert_eq!(region_ids(&all), DOCKERFILE_REGION_ORDER);
    }

    #[test]
    fn every_dockerfile_region_targets_the_one_composed_host() {
        for artifact in all() {
            if let Artifact::Region(spec) = artifact {
                assert_eq!(spec.host, HostSelector::Path(DOCKERFILE_PATH.to_owned()));
                assert_eq!(spec.syntax, CommentSyntax::Hash);
            }
        }
    }

    #[cfg_attr(
        miri,
        ignore = "native/cargo-careful retain generated-recipe text contracts; artifact construction and rendering remain interpreted"
    )]
    #[test]
    fn hooks_constructor_uses_the_documented_path() {
        assert_eq!(paths(&[hooks("# body\n")]), [HOOKS_PATH]);
        assert_eq!(hooks("# body\n").body(), "# body\n");
    }

    #[test]
    fn a_fork_replaces_one_region_without_touching_the_others() {
        // The point of splitting the file: a catalog on another base OS
        // rewrites the base and tool layers and inherits the catalog install
        // and the entry contract, instead of forking the whole Dockerfile and
        // freezing every pin in it.
        let replaced = dockerfile_base_image().with_body("ARG BASE_IMAGE=example.invalid/base\n");
        match &replaced {
            Artifact::Region(spec) => {
                assert_eq!(spec.id.as_str(), "anvil-container-base-image");
                assert_eq!(spec.host, HostSelector::Path(DOCKERFILE_PATH.to_owned()));
            }
            Artifact::OwnedFile(_) | Artifact::OwnedFileSection(_) => panic!("the Dockerfile regions must stay regions"),
        }
        assert_eq!(replaced.body(), "ARG BASE_IMAGE=example.invalid/base\n");
        assert_eq!(dockerfile_setup().body(), DOCKERFILE_SETUP);
    }
}
