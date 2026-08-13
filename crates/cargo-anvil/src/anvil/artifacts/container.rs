// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Containerized execution: the `anvil-container` recipe and the image it runs.
//!
//! Two artifacts define the whole feature. The recipe drives the engine and
//! computes the image identity; the Dockerfile (with its build-context ignore
//! file) defines what the image contains. There is no configuration file:
//! whether the group is emitted at all is a catalog decision, and the only
//! host-specific value — which engine to call — is an environment variable read
//! by the recipe at run time.
//!
//! A downstream catalog customizes exactly two things, and inherits everything
//! else:
//!
//! - [`dockerfile`] plus [`Artifact::with_body`] to build on a different base
//!   or install the toolchain from a different source.
//! - [`hooks`] to supply credentials, which the recipe loads when the file is
//!   present regardless of who put it there.

use crate::catalog::Artifact;

const RECIPE: &str = include_str!("../../../templates/justfiles/anvil/container.just");
const DOCKERFILE: &str = include_str!("../../../templates/container/Dockerfile");
const DOCKERIGNORE: &str = include_str!("../../../templates/container/Dockerfile.dockerignore");

const RECIPE_PATH: &str = "justfiles/anvil/container.just";
const DOCKERFILE_PATH: &str = ".anvil/container/Dockerfile";
const DOCKERIGNORE_PATH: &str = ".anvil/container/Dockerfile.dockerignore";

/// The path the recipe loads credentials from, when a file is present there.
pub const HOOKS_PATH: &str = ".anvil/container/hooks.ps1";

/// The full container artifact group.
#[must_use]
pub fn all() -> Vec<Artifact> {
    vec![recipe(), dockerfile(), dockerignore()]
}

/// The `anvil-container` recipe and its private helpers.
#[must_use]
pub fn recipe() -> Artifact {
    Artifact::owned_file(RECIPE_PATH, RECIPE)
}

/// The default execution image: a digest-pinned Debian base that installs the
/// pinned toolchain and the generated tool catalog by running `just anvil-setup`.
///
/// A downstream catalog that needs a different base OS or toolchain source
/// replaces the body wholesale:
///
/// ```ignore
/// catalog.replace_artifact(
///     artifacts::container::dockerfile().with_body(include_str!("../templates/Dockerfile")),
/// )
/// ```
#[must_use]
pub fn dockerfile() -> Artifact {
    Artifact::owned_file(DOCKERFILE_PATH, DOCKERFILE)
}

/// The build-context ignore file for [`dockerfile`].
///
/// `BuildKit` reads `<dockerfile>.dockerignore` in preference to a root
/// `.dockerignore`, so the build context is scoped without the repository
/// having to own a root ignore file. A catalog that replaces the Dockerfile
/// with one that copies more of the tree must replace this too.
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
/// The script may define either or both of two functions, and is dot-sourced
/// before the phase that needs it:
///
/// - `Anvil-PreBuild` returns `@{ Secrets = @{ <id> = <value> } }`. Each entry
///   becomes a `BuildKit` `--secret id=<id>`, passed by environment variable
///   name so the value never reaches a process argument, and never a layer.
/// - `Anvil-PreRun` returns `@{ Env = @{ <NAME> = <value> } }`. Each entry is
///   forwarded into the container by name, for the same reason.
///
/// An empty value from either function is a hard error: a build that silently
/// proceeds without its credential would install a reduced tool set and then be
/// tagged with the same content hash a credentialed build produces, so every
/// later run would reuse the broken image.
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
                Artifact::Region(_) => panic!("container group must contain owned files only"),
            })
            .collect()
    }

    #[test]
    fn group_is_exactly_three_files() {
        assert_eq!(paths(&all()), [RECIPE_PATH, DOCKERFILE_PATH, DOCKERIGNORE_PATH]);
    }

    #[test]
    fn image_installs_the_generated_toolset() {
        // The image must not carry a second tool list: it installs by running
        // the same recipe the checks use, from the same generated pins.
        assert!(DOCKERFILE.contains("just anvil-setup binstall"));
        assert!(DOCKERFILE.contains("COPY justfiles"));
        assert!(DOCKERFILE.contains("COPY rust-toolchain.toml"));
        // The re-entry guard the recipe relies on to avoid nesting.
        assert!(DOCKERFILE.contains("ENV ANVIL_IN_CONTAINER=1"));
    }

    #[test]
    fn base_image_is_digest_pinned() {
        // A floating base tag can change underneath an identity hash that
        // claims to name fixed content, which would make every cached image a
        // potential lie.
        let base = DOCKERFILE
            .lines()
            .find(|line| line.starts_with("ARG BASE_IMAGE="))
            .expect("the Dockerfile must declare a default BASE_IMAGE");
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
            "anvil-container-status:",
            "anvil-container-rebuild:",
            "anvil-container-down:",
        ] {
            assert!(RECIPE.contains(expected), "missing recipe: {expected}");
        }
    }

    #[test]
    fn every_interpolation_into_powershell_is_escaped() {
        // A `just` value pasted raw into a '…' literal ends the string on an
        // apostrophe: a repository path containing one breaks every recipe
        // here, and `target` would let the remainder run as host PowerShell.
        // The deleted runner.just escaped every interpolation; this guards
        // against losing that again.
        //
        // Two variables are exempt and checked explicitly below: the image name
        // is regex-sanitized at definition, and the workdir is a literal.
        const EXEMPT: [&str; 2] = ["{{anvil_container_name}}", "{{anvil_container_workdir}}"];
        for (index, _) in RECIPE.match_indices("'{{") {
            let tail = &RECIPE[index + 1..];
            let escaped = tail.starts_with("{{ replace(");
            assert!(
                escaped || EXEMPT.iter().any(|exempt| tail.starts_with(exempt)),
                "unescaped interpolation into a PowerShell literal at byte {index}: {}",
                &tail[..tail.len().min(60)]
            );
        }
        // And the escaping that is present uses just's own doubling form.
        assert!(RECIPE.contains(r#"replace(justfile_directory(), "'", "''")"#));
        assert!(RECIPE.contains(r#"replace(invocation_directory(), "'", "''")"#));
        assert!(RECIPE.contains(r#"replace(target, "'", "''")"#));
    }

    #[test]
    fn the_image_name_cannot_carry_an_apostrophe() {
        // What makes the exemption above safe.
        assert!(RECIPE.contains(r#"replace_regex(lowercase(file_name(justfile_directory())), '[^a-z0-9._-]', "-")"#));
    }

    #[test]
    fn a_recipe_argument_survives_as_its_own_word() {
        // `*target` joins with spaces, so passing it through as one string
        // would break `anvil-container anvil-setup binstall` -- and around
        // fifty generated recipes take a parameter.
        assert!(RECIPE.contains(r"-split '\s+'"));
        assert!(RECIPE.contains("just @targetParts"));
        assert!(RECIPE.contains("@('just') + $targetParts"));
    }

    #[test]
    fn podman_is_pointed_at_the_ignore_file() {
        // BuildKit finds `<dockerfile>.dockerignore` itself; buildah reads only
        // a context-root file, so without this the whole worktree is the build
        // context.
        assert!(RECIPE.contains(r#"if ($engineCmd[-1] -eq 'podman') { $buildCmd += @('--ignorefile'"#));
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
    fn recipe_excludes_itself_from_the_image_identity() {
        // Hashing the driver would make the tag depend on the tag.
        assert!(RECIPE.contains("justfiles/anvil/container.just"));
        assert!(RECIPE.contains("-cne 'justfiles/anvil/container.just'"));
    }

    #[test]
    fn hook_file_is_an_image_input_but_hook_output_is_not() {
        // A changed hook must rename the tag; a minted credential must not.
        assert!(RECIPE.contains("$inputs += $hookRel"));
        assert!(RECIPE.contains("id=$id,env=$name"));
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
        assert!(RECIPE.contains("returned an empty value for secret"));
        assert!(RECIPE.contains("returned an empty value for"));
    }

    #[test]
    fn hooks_constructor_uses_the_documented_path() {
        assert_eq!(paths(&[hooks("# body\n")]), [HOOKS_PATH]);
        assert_eq!(hooks("# body\n").body(), "# body\n");
    }

    #[test]
    fn dockerfile_body_can_be_replaced_by_a_fork() {
        let replaced = dockerfile().with_body("FROM example.invalid/base\n");
        assert_eq!(paths(std::slice::from_ref(&replaced)), [DOCKERFILE_PATH]);
        assert_eq!(replaced.body(), "FROM example.invalid/base\n");
    }
}
