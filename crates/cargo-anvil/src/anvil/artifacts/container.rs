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
//! of the three gaps between the regions is the correct home for one class of
//! addition, defined by what must already be true at that point in the build.
//!
//! | Gap | Runs | Exists for |
//! | --- | --- | --- |
//! | after [`dockerfile_base`] | before the first download | root CA, proxy, internal apt mirror |
//! | after [`dockerfile_tools`] | after the toolchain, before `anvil-setup` | libraries a catalog tool needs to *compile* |
//! | after [`dockerfile_setup`] | after the catalog is installed | what the repository's own checks need at run time |
//!
//! A downstream catalog customizes by replacing individual regions, and
//! inherits the rest:
//!
//! - [`dockerfile_base`] / [`dockerfile_tools`] plus [`Artifact::with_body`] to
//!   build on a different base OS or install the toolchain from a different
//!   source. [`dockerfile_setup`] and [`dockerfile_entry`] are the contract with
//!   the recipe and are rarely replaced.
//! - [`dockerignore`] alongside them when the replacement copies more of the
//!   tree, or the added paths never reach the build context.
//! - [`hooks`] to supply credentials, or to resolve a published image through
//!   `Anvil-ResolveImage`. The recipe loads the file when it is present,
//!   regardless of who put it there.

use crate::catalog::{Artifact, HostSelector, RegionId, RegionSpec};
use crate::region::CommentSyntax;

const RECIPE: &str = include_str!("../../../templates/justfiles/anvil/container.just");
const DOCKERIGNORE: &str = include_str!("../../../templates/anvil/container/Dockerfile.dockerignore");

/// Seeded into the Dockerfile when the file does not exist, and never
/// reconciled afterwards — it is the user's half of a composed file.
///
/// It carries `# syntax=docker/dockerfile:1`, which `BuildKit` honors only as
/// the very first line of the file. A region's opening sentinel is a comment,
/// so the directive cannot live inside a region without being demoted to an
/// ordinary comment — silently, with the build falling back to the default
/// frontend and nothing failing to say so.
pub(crate) const DOCKERFILE_HEADER: &str = include_str!("../../../templates/anvil/container/Dockerfile.header");

const DOCKERFILE_BASE: &str = include_str!("../../../templates/anvil/container/Dockerfile.base.region");
const DOCKERFILE_TOOLS: &str = include_str!("../../../templates/anvil/container/Dockerfile.tools.region");
const DOCKERFILE_SETUP: &str = include_str!("../../../templates/anvil/container/Dockerfile.setup.region");
const DOCKERFILE_ENTRY: &str = include_str!("../../../templates/anvil/container/Dockerfile.entry.region");

const RECIPE_PATH: &str = "justfiles/anvil/container.just";

/// The composed Dockerfile the managed regions are spliced into.
pub(crate) const DOCKERFILE_PATH: &str = ".anvil/container/Dockerfile";

const DOCKERIGNORE_PATH: &str = ".anvil/container/Dockerfile.dockerignore";

/// The region ids anvil owns inside [`DOCKERFILE_PATH`], in the order a valid
/// Dockerfile must carry them.
///
/// Order is load-bearing in a way no other managed-region host is: `FROM` must
/// precede every instruction, the toolchain must exist before `anvil-setup`
/// runs, and `WORKDIR`/`CMD` close the file. The engine checks the on-disk
/// sequence against this list and refuses rather than emitting a Dockerfile
/// that is silently wrong.
pub(crate) const DOCKERFILE_REGION_ORDER: &[&str] = &[
    "anvil-container-base",
    "anvil-container-tools",
    "anvil-container-setup",
    "anvil-container-entry",
];

/// The path the recipe loads credentials from, when a file is present there.
pub const HOOKS_PATH: &str = ".anvil/container/hooks.ps1";

/// The full container artifact group.
#[must_use]
pub fn all() -> Vec<Artifact> {
    vec![
        recipe(),
        dockerignore(),
        dockerfile_base(),
        dockerfile_tools(),
        dockerfile_setup(),
        dockerfile_entry(),
    ]
}

/// The `anvil-container` recipe and its private helpers.
#[must_use]
pub fn recipe() -> Artifact {
    Artifact::owned_file(RECIPE_PATH, RECIPE)
}

fn dockerfile_region(id: &'static str, body: &'static str) -> Artifact {
    Artifact::region(RegionSpec {
        host: HostSelector::Path(DOCKERFILE_PATH.to_owned()),
        id: RegionId::new(id),
        body: body.to_owned(),
        syntax: CommentSyntax::Hash,
    })
}

/// The base image and the pinned tool versions: a digest-pinned base tracking
/// the Linux CI runner, the four download pins, and the environment every later
/// region depends on.
///
/// A downstream catalog that needs a different base OS replaces this and
/// usually [`dockerfile_tools`] with it:
///
/// ```ignore
/// catalog.replace_artifact(
///     artifacts::container::dockerfile_base()
///         .with_body(include_str!("../templates/base.dockerfile")),
/// )
/// ```
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
///   instead, or nothing. It is how a repository fetches a published image
///   rather than building locally.
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
                Artifact::Region(_) => panic!("expected an owned file"),
            })
            .collect()
    }

    fn region_ids(artifacts: &[Artifact]) -> Vec<&str> {
        artifacts
            .iter()
            .filter_map(|artifact| match artifact {
                Artifact::Region(spec) => Some(spec.id.as_str()),
                Artifact::OwnedFile(_) => None,
            })
            .collect()
    }

    /// The Dockerfile as a fresh repository first receives it: the seeded
    /// header followed by every region body in the order the engine enforces.
    fn composed_dockerfile() -> String {
        let mut out = DOCKERFILE_HEADER.to_owned();
        for body in [DOCKERFILE_BASE, DOCKERFILE_TOOLS, DOCKERFILE_SETUP, DOCKERFILE_ENTRY] {
            out.push_str(body);
        }
        out
    }

    #[test]
    fn group_is_two_owned_files_and_four_dockerfile_regions() {
        let all = all();
        let owned: Vec<_> = all
            .iter()
            .filter(|artifact| matches!(artifact, Artifact::OwnedFile(_)))
            .cloned()
            .collect();
        assert_eq!(paths(&owned), [RECIPE_PATH, DOCKERIGNORE_PATH]);
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

    #[test]
    fn the_syntax_directive_leads_the_seeded_header() {
        // BuildKit honors the parser directive only when nothing precedes it.
        // If this ever moves, the frontend pin stops applying and nothing
        // fails to say so.
        assert_eq!(
            DOCKERFILE_HEADER.lines().next(),
            Some("# syntax=docker/dockerfile:1"),
            "the parser directive must be the first line of the seeded header"
        );
    }

    #[test]
    fn no_region_body_carries_the_parser_directive() {
        // Inside a region the directive would sit below an opening sentinel --
        // a comment -- and be silently demoted.
        for body in [DOCKERFILE_BASE, DOCKERFILE_TOOLS, DOCKERFILE_SETUP, DOCKERFILE_ENTRY] {
            assert!(!body.contains("# syntax="), "a region body must not carry the parser directive");
        }
    }

    #[test]
    fn image_installs_the_generated_toolset() {
        // The image must not carry a second tool list: it installs by running
        // the same recipe the checks use, from the same generated pins.
        let composed = composed_dockerfile();
        assert!(composed.contains("just anvil-setup binstall"));
        assert!(composed.contains("COPY justfiles"));
        assert!(composed.contains("COPY rust-toolchain.toml"));
        // The re-entry guard the recipe relies on to avoid nesting.
        assert!(composed.contains("ENV ANVIL_IN_CONTAINER=1"));
    }

    #[test]
    fn the_composed_order_is_a_buildable_dockerfile() {
        // The region order is not cosmetic: everything depends on FROM, and
        // the catalog install needs the toolchain that precedes it.
        let composed = composed_dockerfile();
        let at = |needle: &str| {
            composed
                .find(needle)
                .unwrap_or_else(|| panic!("missing from composed Dockerfile: {needle}"))
        };
        assert!(at("FROM ${BASE_IMAGE}") < at("RUN apt-get update"));
        assert!(at("RUN apt-get update") < at("just anvil-setup binstall"));
        assert!(at("just anvil-setup binstall") < at("WORKDIR /workspace"));
    }

    #[test]
    fn base_image_is_digest_pinned() {
        // A floating base tag can change underneath an identity hash that
        // claims to name fixed content, which would make every cached image a
        // potential lie.
        let base = DOCKERFILE_BASE
            .lines()
            .find(|line| line.starts_with("ARG BASE_IMAGE="))
            .expect("the base region must declare a default BASE_IMAGE");
        assert!(base.contains("@sha256:"), "BASE_IMAGE must be digest-pinned: {base}");
    }

    #[test]
    fn build_context_admits_only_what_the_image_copies() {
        assert!(DOCKERIGNORE.contains("!justfiles"));
        assert!(DOCKERIGNORE.contains("!rust-toolchain.toml"));
    }

    #[test]
    fn recipe_has_no_generation_time_placeholders() {
        // Every value is a literal or resolved at run time; nothing is
        // substituted at emit time, so the recipe cannot drift from a
        // configuration file that no longer exists.
        assert!(!RECIPE.contains("__"), "the recipe must not carry rendering placeholders");
        assert!(!RECIPE.contains("anvil.toml"));
    }

    #[test]
    fn recipe_exposes_the_documented_surface() {
        for expected in [
            "anvil-container *target:",
            "anvil-container-tag:",
            "anvil-container-status:",
            "anvil-container-down:",
        ] {
            assert!(RECIPE.contains(expected), "missing recipe: {expected}");
        }
        // A cache-defeating rebuild is `ANVIL_CONTAINER_NO_CACHE=1`, which is
        // already public and composes with the other guards. A recipe wrapping
        // one variable assignment would be a second way to say the same thing.
        assert!(!RECIPE.contains("anvil-container-rebuild:"));
    }

    #[test]
    fn every_public_recipe_lists_with_a_whole_sentence() {
        // `just --list` takes the last comment line before the attributes as
        // the description, so a recipe whose rationale paragraph ends mid
        // sentence lists as a fragment -- "# toolchain that would otherwise
        // mask the image's own." The generated tree is the discovery surface,
        // so each public recipe repeats a one-line summary immediately above
        // its attributes.
        for recipe in [
            "anvil-container *target:",
            "anvil-container-tag:",
            "anvil-container-status:",
            "anvil-container-down:",
        ] {
            let at = RECIPE.find(recipe).expect("recipe must exist");
            let description = RECIPE[..at]
                .lines()
                .rev()
                .find(|line| line.trim_start().starts_with('#'))
                .expect("a public recipe must carry a description")
                .trim_start()
                .trim_start_matches('#')
                .trim();
            assert!(
                description.ends_with('.') && description.starts_with(|c: char| c.is_uppercase()),
                "{recipe} lists as a fragment: {description:?}"
            );
        }
    }

    #[test]
    fn the_tag_is_computed_in_exactly_one_place() {
        // `anvil-container-tag` is public so a publisher can name the image it
        // is about to build. That only holds while it is the same computation
        // the consumer performs: a second copy of the hash would let the two
        // drift and turn a published tag into a claim nobody checks.
        assert_eq!(
            RECIPE.matches("TransformFinalBlock").count(),
            1,
            "the content hash must be computed once, by anvil-container-tag"
        );
        // Bytes, not decoded text: ReadAllText replaces every invalid sequence
        // with U+FFFD, so two files differing only in invalid bytes would hash
        // alike while COPY put their real bytes in the image.
        assert!(RECIPE.contains("[System.IO.File]::ReadAllBytes($path)"));
        assert!(
            RECIPE.contains("$image = & '{{ replace(just_executable(), \"'\", \"''\") }}' anvil-container-tag"),
            "the resolver must ask anvil-container-tag rather than recompute"
        );
    }

    #[test]
    fn resolution_is_attempted_before_building_and_never_fatal() {
        // Order: local image, then the hook, then a build. Resolving sits
        // inside the cache guard (NO_CACHE must defeat a remote cache too) and
        // before the NO_REBUILD guard, because fetching is not building.
        let inspect = RECIPE.find("image inspect $image").expect("the local check must exist");
        let resolve = RECIPE.find("Anvil-ResolveImage $image").expect("the resolve call must exist");
        let build = RECIPE.find("anvil: building $image").expect("the build must exist");
        assert!(
            inspect < resolve && resolve < build,
            "resolve belongs between the local check and the build"
        );

        let no_rebuild = RECIPE
            .find("if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {")
            .expect("the no-rebuild guard must exist");
        assert!(resolve < no_rebuild, "resolving is not building, so NO_REBUILD must not block it");

        // A publisher that has not caught up must not stop the developer who
        // made the change, so every failure falls through to a build --
        // including a hook that cannot even be loaded, which is why the
        // dot-source is inside the try rather than ahead of it.
        let try_start = RECIPE[..resolve].rfind("try {").expect("the resolve call must sit inside a try");
        let load = RECIPE[..resolve]
            .rfind(". $hookPath")
            .expect("the hook must be loaded before it is called");
        assert!(try_start < load, "loading a broken hook must not escape the catch");
        assert!(RECIPE.contains("anvil: $hookRel failed:"));
        assert!(RECIPE.contains("anvil: nothing resolved; building locally"));
    }

    #[test]
    fn a_resolved_reference_is_checked_for_presence_before_it_is_used() {
        // Presence, not verification: `image inspect` proves something carries
        // that reference, not that its contents match the digest the tag
        // claims. Trusting the hook is the contract; this only keeps a
        // reference the hook never fetched from failing later, under
        // `--pull=never`, a long way from the cause.
        let resolve = RECIPE.find("Anvil-ResolveImage $image").expect("the resolve call must exist");
        let verify = RECIPE[resolve..]
            .find("image inspect $resolved")
            .expect("a resolved reference must be inspected before use");
        let accept = RECIPE[resolve..]
            .find("Write-Output $resolved")
            .expect("a resolved reference must be returned");
        assert!(verify < accept, "check the resolved reference before returning it");
    }

    #[test]
    fn a_query_never_pulls() {
        // Resolving can mean pulling gigabytes; `anvil-container-status` asks
        // about this machine and must not reach a registry to answer.
        assert!(RECIPE.contains("$env:ANVIL_CONTAINER_NO_RESOLVE = '1'"));
        assert!(RECIPE.contains("$env:ANVIL_CONTAINER_NO_RESOLVE -ne '1'"));
    }

    #[test]
    fn every_interpolation_into_powershell_is_escaped() {
        // A `just` value pasted raw into a '…' literal ends the string on an
        // apostrophe: a repository path containing one breaks every recipe
        // here, and `target` would let the remainder run as host PowerShell.
        // The deleted runner.just escaped every interpolation; this guards
        // against losing that again.
        //
        // Every `{{` is visited, not just those preceded by an apostrophe,
        // because the defect found in review was the other case: an
        // interpolation escaped with `replace(…, "'", "''")` -- correct for a
        // '…' literal -- that landed inside a "…" literal, where the doubling
        // renders as a literal '' and `$` stays live. Scanning only for `'{{`
        // cannot see that site at all, since it does not begin with a quote.
        //
        // Two variables are exempt and checked explicitly below: the image name
        // is regex-sanitized at definition, and the workdir is a literal.
        const EXEMPT: [&str; 2] = ["{{anvil_container_name}}", "{{anvil_container_workdir}}"];
        for (index, _) in RECIPE.match_indices("{{") {
            let tail = &RECIPE[index..];
            if EXEMPT.iter().any(|exempt| tail.starts_with(exempt)) {
                continue;
            }
            // The quote this interpolation is being pasted into, if any.
            let quote = RECIPE[..index].chars().next_back();
            let escaped = tail.starts_with("{{ replace(");
            match quote {
                // A single-quoted literal needs just's doubling form.
                Some('\'') => assert!(
                    escaped,
                    "unescaped interpolation into a PowerShell literal at byte {index}: {}",
                    &tail[..tail.len().min(60)]
                ),
                // A double-quoted literal is the reviewed hazard: `''` doubling
                // does not escape there and `$` keeps expanding, so only an
                // exempt name is safe. Interpolating anything else needs a
                // single-quoted literal instead.
                Some('"') => panic!(
                    "interpolation into a double-quoted PowerShell literal at byte {index}: {}",
                    &tail[..tail.len().min(60)]
                ),
                _ => {}
            }
        }
        // And the escaping that is present uses just's own doubling form.
        assert!(RECIPE.contains(r#"replace(justfile_directory(), "'", "''")"#));
        assert!(RECIPE.contains(r#"replace(invocation_directory_native(), "'", "''")"#));
        assert!(RECIPE.contains(r#"replace(target, "'", "''")"#));
    }

    #[test]
    fn the_image_name_cannot_carry_an_apostrophe() {
        // What makes the exemption above safe: the character class admits
        // only alphanumerics, so no quote can reach a PowerShell literal.
        assert!(RECIPE.contains(r#"replace_regex(lowercase(file_name(justfile_directory())), '[^a-z0-9]+', "-")"#));
    }

    #[test]
    fn a_recipe_argument_survives_as_its_own_word() {
        // `*target` joins with spaces, so passing it through as one string
        // would break `anvil-container anvil-setup binstall` -- and around
        // fifty generated recipes take a parameter.
        assert!(RECIPE.contains(r"-split '\s+'"));
        assert!(RECIPE.contains("}}' @targetParts"));
        assert!(RECIPE.contains("@('just') + $targetParts"));
        // Every nested call goes through the launching binary, including the
        // in-container passthrough: ANVIL_IN_CONTAINER is a documented control
        // a developer can set on a host, where a bare name resolves against
        // PATH rather than the `just` that is running.
        assert!(!RECIPE.contains("\n        just @targetParts"));
    }

    #[test]
    fn podman_is_pointed_at_the_ignore_file() {
        // BuildKit finds `<dockerfile>.dockerignore` itself; buildah reads only
        // a context-root file, so without this the whole worktree is the build
        // context.
        assert!(RECIPE.contains(r"if ($engineCmd[-1] -eq 'podman') { $buildCmd += @('--ignorefile'"));
    }

    #[test]
    fn no_rebuild_is_honoured_even_with_no_cache_set() {
        // The two controls compose: NO_REBUILD must not be skipped just
        // because NO_CACHE is exported, or `anvil-container-status` spends
        // minutes building from a query.
        let no_cache = RECIPE
            .find("if ($env:ANVIL_CONTAINER_NO_CACHE -ne '1') {")
            .expect("the cache guard must exist");
        let no_rebuild = RECIPE
            .find("if ($env:ANVIL_CONTAINER_NO_REBUILD -eq '1') {")
            .expect("the no-rebuild guard must exist");
        let guard_end = RECIPE[no_cache..].find("\n    }\n").expect("the cache guard must be closed") + no_cache;
        assert!(no_rebuild > guard_end, "the NO_REBUILD check must sit outside the NO_CACHE guard");
    }

    #[test]
    fn engine_is_an_environment_variable_with_a_docker_default() {
        assert!(RECIPE.contains(r#"env_var_or_default("ANVIL_CONTAINER_ENGINE", "docker")"#));
        assert!(RECIPE.contains(r#"replace(anvil_container_engine, "'", "''")"#));
    }

    #[test]
    fn hook_file_is_an_image_input_but_hook_output_is_not() {
        // A changed hook must rename the tag; a minted credential must not.
        // The hook is no longer named individually -- it is picked up by the
        // walk of `.anvil/container/`, which is what also catches a file a
        // repository `COPY`s from one of the Dockerfile's user gaps.
        assert!(RECIPE.contains("$containerRoot = Join-Path $repoRoot '.anvil/container'"));
        assert!(RECIPE.contains("id=$id,env=$name"));
    }

    #[test]
    fn every_file_under_the_container_directory_is_an_image_input() {
        // The Dockerfile is composed: a repository adds `COPY` lines in the
        // gaps between anvil's regions, naming files anvil never hears about.
        // Hashing a fixed list would let those change the image under a
        // reference that already resolves.
        let walk = RECIPE
            .find("$containerRoot = Join-Path $repoRoot '.anvil/container'")
            .expect("the tag must walk the container directory");
        let recurse = RECIPE[walk..]
            .find("Get-ChildItem -LiteralPath $containerRoot -Recurse -File -Force")
            .expect("the walk must be recursive and include hidden entries");
        // A missing Dockerfile must still be fatal: the walk alone would let
        // it contribute nothing and yield a confident tag for an unbuildable
        // image.
        assert!(RECIPE[walk + recurse..].contains("container image input is missing: $dockerfile"));
    }

    #[test]
    fn hook_values_are_passed_by_name_never_by_value() {
        // NAME=VALUE on a command line is recorded by endpoint telemetry and
        // retained far longer than a short-lived token is meant to live.
        assert!(RECIPE.contains("$runArgs += @('-e', $name)"));
        assert!(!RECIPE.contains("-e', \"$name="));
    }

    #[test]
    fn empty_hook_values_fail_closed() {
        // Both phases, asserted independently: "for" alone is a substring of
        // the build-side message, so it would pass with the run-side guard
        // deleted.
        assert!(RECIPE.contains("Anvil-BuildSecrets returned an empty value for secret"));
        assert!(RECIPE.contains("Anvil-RunEnv returned an empty value for"));
    }

    #[test]
    fn cache_volumes_never_mask_the_images_tools() {
        // An engine seeds a named volume from the image only on first
        // creation, so mounting a directory that holds installed binaries
        // pins the first image's tools over every later tag.
        assert!(RECIPE.contains("-cargo-registry:/usr/local/cargo/registry"));
        assert!(RECIPE.contains("-cargo-git:/usr/local/cargo/git"));
        assert!(!RECIPE.contains("-cargo:/usr/local/cargo'"));
        assert!(!RECIPE.contains(":/usr/local/rustup"));
    }

    #[test]
    fn wsl_calls_bypass_the_login_shell() {
        // `wsl.exe -- <cmd>` re-parses the command line through the default
        // shell: a path holding `$` is silently truncated (and wslpath still
        // exits 0), and a `;` in any forwarded argument runs on the host.
        // Matched on the invocation form so the comment explaining this may
        // still name the broken spelling.
        assert!(!RECIPE.contains("& wsl.exe -- "));
        assert!(!RECIPE.contains("wsl.exe|--|"));
        assert!(RECIPE.contains("& wsl.exe --exec "));
        assert!(RECIPE.contains("wsl.exe|--exec|"));
    }

    #[test]
    fn container_name_is_always_a_valid_reference() {
        // A repository name may not end in a separator or repeat `.`/`_`, so
        // a directory like `ox-tools (copy)` must not reach the engine as
        // `anvil-ox-tools--copy-`.
        assert!(RECIPE.contains(r#"replace_regex(lowercase(file_name(justfile_directory())), '[^a-z0-9]+', "-")"#));
        assert!(RECIPE.contains(r#"trim_end_matches("anvil-" + replace_regex"#));
    }

    #[test]
    fn teardown_reports_a_removal_that_failed() {
        // $ErrorActionPreference does not cover native commands, and this is
        // the only way to clear a cache volume.
        assert!(RECIPE.contains("if ($LASTEXITCODE -ne 0) { $failed += $vol }"));
        assert!(RECIPE.contains("anvil: could not remove: "));
    }

    #[test]
    fn a_mapped_user_gets_a_writable_home() {
        // A uid with no passwd entry is given HOME=/, which is not writable.
        assert!(RECIPE.contains("$runArgs += @('--user', \"${hostUid}:${hostGid}\")"));
        assert!(RECIPE.contains("$runArgs += @('-e', 'HOME=/tmp')"));
    }

    #[test]
    fn a_host_token_is_resolved_as_the_recipe_does_and_forwarded_by_name() {
        // anvil-aprz is in scheduled-advisories, and unauthenticated it does not merely
        // warn: `cargo aprz deps` sleeps until the hourly quota resets, so a
        // containerized tier blocks for up to an hour. The driver therefore
        // resolves a token the same way the recipe does natively -- the
        // environment first, then the gh CLI -- so both paths authenticate for
        // the same developers.
        assert!(RECIPE.contains("gh auth token --hostname github.com"));
        assert!(RECIPE.contains("$forwardedEnv += 'GITHUB_TOKEN'"));
        assert!(RECIPE.contains("$runArgs += @('-e', 'GITHUB_TOKEN')"));
        // By name, never by value: `-e NAME=VALUE` would put the credential on
        // the host's command line, where endpoint telemetry retains it.
        assert!(!RECIPE.contains("'-e', \"GITHUB_TOKEN="));
        // A derived token is set on this process, so it must be registered for
        // the same cleanup the hook's variables get.
        assert!(RECIPE.contains("$hookEnv += 'GITHUB_TOKEN'"));
        // An exported token is left alone rather than re-derived; scoping of
        // the derived one is asserted in its own test below.
        assert!(RECIPE.contains("if (-not $env:GITHUB_TOKEN -and (Get-Command gh"));
        // Forwarding by name only works if the engine can see the name, so a
        // WSL engine needs it bridged -- otherwise `-e NAME` forwards nothing.
        assert!(RECIPE.contains("$engineExe -eq 'wsl.exe' -and $forwardedEnv.Count -gt 0"));
    }

    #[test]
    fn the_whole_recipe_tree_defines_the_image() {
        // `just anvil-setup` reaches the install recipes through the tier,
        // group and check recipes, so the routing decides *whether* a tool is
        // installed as surely as tools.just decides *how*. Hashing only the
        // install definitions would let a group drop a `-setup` dependency,
        // changing the installed set, without renaming the image.
        // Every file, not only `*.just`: the build context admits the whole
        // directory, so a non-recipe file an adopter adds by hand is copied
        // into the image. Filtering here would let it change the image's
        // contents without changing its tag. -Force because a dot-prefixed
        // file is copied like any other and would otherwise be skipped.
        assert!(RECIPE.contains("-Recurse -File -Force"));
        assert!(!RECIPE.contains("-Recurse -File -Force -Filter '*.just'"));
        // ComputeHash, not the static HashData: the latter needs .NET 5, and
        // the prerequisite check accepts PowerShell 7.0 on .NET Core 3.1.
        assert!(!RECIPE.contains("SHA256]::HashData"));
        assert!(RECIPE.contains("SHA256]::Create()"));
        // Including this driver, which passes the build arguments, the secret
        // mounts and the hook's PreBuild output into the build.
        assert!(!RECIPE.contains("-cne 'justfiles/anvil/container.just'"));
    }

    #[test]
    fn the_recipe_contract_inputs_cross_the_boundary() {
        // A check that reads one of these natively must read the same value in
        // a container, or the same command means two different things.
        // anvil-pr-title is the sharp case: with PR_TITLE unset it exits 0 with
        // a skip notice, so a title a native run rejects would pass in a
        // container and the tier would still report green.
        for name in ["PR_TITLE", "BASE_REF", "GITHUB_BASE_REF", "SYSTEM_PULLREQUEST_TARGETBRANCH"] {
            assert!(RECIPE.contains(name), "{name} must be forwarded");
        }
        for name in ["ANVIL_INCLUDE_MODIFIED", "ANVIL_INCLUDE_AFFECTED", "ANVIL_INCLUDE_REQUIRED"] {
            assert!(RECIPE.contains(name), "{name} must be forwarded");
        }
    }

    #[test]
    fn a_derived_token_is_scoped_to_a_target_that_reads_it() {
        // Forwarding an exported GITHUB_TOKEN is exact parity: natively it is
        // visible to every process the shell spawns too. Minting one from `gh`
        // is not -- PID 1's environment reaches every build script and proc
        // macro, where natively the recipe mints it in its own process -- so it
        // happens only for a target whose plan reads the variable.
        let derive = RECIPE.find("gh auth token --hostname").expect("the gh fallback must exist");
        let guard = RECIPE[..derive].rfind("if ($needsToken)").expect("the derive must be guarded");
        let plan = RECIPE[..guard]
            .rfind("$plan -match 'GITHUB_TOKEN'")
            .expect("the plan must decide whether a token is needed");
        let dry_run = RECIPE[..plan]
            .rfind("--dry-run @targetParts")
            .expect("the plan must come from just");
        assert!(dry_run < plan && plan < guard, "compute the plan, match it, then derive");
        // Through the launching binary, like every other nested call: a bare
        // `just` here fails silently when the caller invoked it by absolute
        // path, and an empty plan reads as "no token needed".
        assert!(!RECIPE.contains("(just --dry-run"));
        assert!(RECIPE.contains("}}' --dry-run @targetParts"));
        // The predicate is the variable, not the name of a check, so a catalog
        // that adds another GitHub-authenticated check is covered for free.
        assert!(!RECIPE.contains("$plan -match 'aprz'"));
        // An interactive session has no target to plan, and can run anything.
        assert!(RECIPE.contains("$needsToken = $targetParts.Count -eq 0"));
    }

    #[test]
    fn the_credential_phases_are_fail_closed() {
        // Unlike resolution, these must stop the run: a container that starts
        // without its credentials fails deep inside, far from the cause.
        assert!(RECIPE.contains("anvil: Anvil-BuildSecrets returned no secrets"));
        assert!(RECIPE.contains("anvil: Anvil-RunEnv returned no variables"));
        assert!(RECIPE.contains("anvil: failed to load ${hookRel}:"));
        // Whitespace is not a credential. IsNullOrEmpty would accept " ".
        assert!(!RECIPE.contains("[string]::IsNullOrEmpty($hook"));
        // Take the last object, not the whole stream: a hook that writes
        // progress with Write-Output would otherwise hand back an array whose
        // .Secrets is silently $null, and the guard above would not fire.
        assert!(RECIPE.contains("@(Anvil-BuildSecrets | Where-Object { $_ }) | Select-Object -Last 1"));
        assert!(RECIPE.contains("@(Anvil-RunEnv | Where-Object { $_ }) | Select-Object -Last 1"));
    }

    #[test]
    fn the_working_directory_is_mapped_from_a_native_path() {
        // `invocation_directory()` reports a Cygwin-style path when cygpath is
        // on PATH, which shares no prefix with the native justfile_directory()
        // it is made relative to -- so the run would be placed outside the
        // mount, on a path that does not exist in the container.
        assert!(RECIPE.contains("invocation_directory_native()"));
        assert!(!RECIPE.contains("replace(invocation_directory(), "));
        assert!(RECIPE.contains("$rel.StartsWith('..')"));
    }

    #[test]
    fn teardown_removes_only_volumes_the_run_creates() {
        // Naming a volume the run never mounts is a claim that it exists.
        let down = RECIPE.find("anvil-container-down:").expect("the teardown recipe must exist");
        for stale in ["-cargo'", "-rustup'"] {
            assert!(
                !RECIPE[down..].contains(stale),
                "{stale} is never created, so it cannot be torn down"
            );
        }
        assert!(RECIPE[down..].contains("-cargo-registry'"));
        assert!(RECIPE[down..].contains("-cargo-git'"));
    }

    #[test]
    fn the_build_context_stays_scoped_to_the_recipe_tree() {
        // The whole tree is copied because `just` must parse it, but nothing
        // outside it is: an unscoped context streams every stale `target/` to
        // the daemon on each build.
        assert!(DOCKERIGNORE.contains("justfiles/*\n!justfiles/anvil\n"));
        assert!(!DOCKERIGNORE.contains("!justfiles\n!rust-toolchain.toml"));
    }

    #[test]
    fn a_linked_worktree_can_reach_its_git_directory() {
        // A worktree's .git is a file naming a host path outside the mount, so
        // without this the container resolves no refs at all and every check
        // that needs history fails.
        assert!(RECIPE.contains("git rev-parse --git-common-dir"));
        assert!(RECIPE.contains("${engineGitCommon}:/anvil/gitdir"));
        // Redirected through the checkout's own .git file, never through
        // GIT_DIR/GIT_WORK_TREE: those are ambient, so every process in the
        // container would inherit them and any git run outside the workspace
        // -- `git init` in a test's scratch directory, most of all -- would
        // operate on this repository instead of its own.
        assert!(RECIPE.contains("gitdir: /anvil/gitdir/$rel"));
        assert!(RECIPE.contains("{{anvil_container_workdir}}/.git:ro"));
        assert!(!RECIPE.contains("GIT_DIR="));
        assert!(!RECIPE.contains("GIT_WORK_TREE="));
        // The generated file is temporary and must not outlive the run.
        assert!(RECIPE.contains("if ($gitFile) { Remove-Item -LiteralPath $gitFile"));
        // An ordinary clone must not take the extra mount.
        assert!(RECIPE.contains("if ($gitDirAbs -ne $gitCommonAbs) {"));
        // A host with a working engine but no git must not start failing here.
        assert!(RECIPE.contains("if (Get-Command git -ErrorAction SilentlyContinue) {"));
    }

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
        let replaced = dockerfile_base().with_body("FROM example.invalid/base\n");
        match &replaced {
            Artifact::Region(spec) => {
                assert_eq!(spec.id.as_str(), "anvil-container-base");
                assert_eq!(spec.host, HostSelector::Path(DOCKERFILE_PATH.to_owned()));
            }
            Artifact::OwnedFile(_) => panic!("the Dockerfile regions must stay regions"),
        }
        assert_eq!(replaced.body(), "FROM example.invalid/base\n");
        assert_eq!(dockerfile_setup().body(), DOCKERFILE_SETUP);
    }
}
