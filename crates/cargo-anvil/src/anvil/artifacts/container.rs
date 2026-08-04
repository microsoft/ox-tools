// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The optional local container backend.
//!
//! The base catalog emits a public Docker Engine implementation. Downstream catalogs
//! replace only environment-specific artifacts such as the Containerfile and
//! add an optional `customize.sh`/`customize.ps1` runtime customization file.

use crate::catalog::Artifact;

const RECIPE: &str = include_str!("../../../templates/justfiles/anvil/container.just");
const CONTAINERFILE: &str = include_str!("../../../templates/anvil/container/Containerfile");
const IGNORE: &str = include_str!("../../../templates/anvil/container/Containerfile.dockerignore");
const ENTRYPOINT: &str = include_str!("../../../templates/anvil/container/entrypoint.sh");
const IMAGE_ID: &str = include_str!("../../../templates/anvil/container/image-id.ps1");
const SHELL_IMAGE_ID: &str = include_str!("../../../templates/anvil/container/image-id.sh");
const SHELL_DRIVER: &str = include_str!("../../../templates/anvil/container/run-in-container.sh");
const POWERSHELL_DRIVER: &str = include_str!("../../../templates/anvil/container/run-in-container.ps1");
const README: &str = include_str!("../../../templates/anvil/container/README.md");

const RECIPE_PATH: &str = "justfiles/anvil/container.just";
const CONTAINERFILE_PATH: &str = ".anvil/container/Containerfile";
const IGNORE_PATH: &str = ".anvil/container/Containerfile.dockerignore";
const ENTRYPOINT_PATH: &str = ".anvil/container/entrypoint.sh";
const IMAGE_ID_PATH: &str = ".anvil/container/image-id.ps1";
const SHELL_IMAGE_ID_PATH: &str = ".anvil/container/image-id.sh";
const SHELL_DRIVER_PATH: &str = ".anvil/container/run-in-container.sh";
const POWERSHELL_DRIVER_PATH: &str = ".anvil/container/run-in-container.ps1";
const README_PATH: &str = ".anvil/container/README.md";
const CUSTOMIZE_SHELL_PATH: &str = ".anvil/container/customize.sh";
const CUSTOMIZE_POWERSHELL_PATH: &str = ".anvil/container/customize.ps1";

/// The full public container artifact group.
#[must_use]
pub fn all() -> Vec<Artifact> {
    vec![
        recipe(),
        containerfile(),
        ignore_file(),
        entrypoint(),
        image_id(),
        shell_image_id(),
        shell_driver(),
        powershell_driver(),
        readme(),
    ]
}

/// The explicit `anvil-container` recipe.
#[must_use]
pub fn recipe() -> Artifact {
    Artifact::owned_file(RECIPE_PATH, RECIPE)
}

/// The public rustup/crates.io Containerfile.
#[must_use]
pub fn containerfile() -> Artifact {
    Artifact::owned_file(CONTAINERFILE_PATH, CONTAINERFILE)
}

/// The restricted Docker build-context ignore file.
#[must_use]
pub fn ignore_file() -> Artifact {
    Artifact::owned_file(IGNORE_PATH, IGNORE)
}

/// The generic non-root Cargo metadata entry point.
#[must_use]
pub fn entrypoint() -> Artifact {
    Artifact::owned_file(ENTRYPOINT_PATH, ENTRYPOINT)
}

/// The cross-platform content-addressed image-id helper.
#[must_use]
pub fn image_id() -> Artifact {
    Artifact::owned_file(IMAGE_ID_PATH, IMAGE_ID)
}

/// The Bash content-addressed image-id helper.
#[must_use]
pub fn shell_image_id() -> Artifact {
    Artifact::owned_file(SHELL_IMAGE_ID_PATH, SHELL_IMAGE_ID)
}

/// The Linux/WSL Docker Engine driver.
#[must_use]
pub fn shell_driver() -> Artifact {
    Artifact::owned_file(SHELL_DRIVER_PATH, SHELL_DRIVER)
}

/// The Windows-to-WSL Docker Engine driver.
#[must_use]
pub fn powershell_driver() -> Artifact {
    Artifact::owned_file(POWERSHELL_DRIVER_PATH, POWERSHELL_DRIVER)
}

/// User-facing prerequisites and troubleshooting.
#[must_use]
pub fn readme() -> Artifact {
    Artifact::owned_file(README_PATH, README)
}

/// Add a downstream shell customization file (`customize.sh`).
///
/// The public catalog does not emit this file. A regular repository can add
/// the standard path directly; a derived distribution can package the same
/// file through this constructor. The driver loads it whenever present,
/// regardless of provenance. See
/// [the container customization contract](../../../docs/design/containers.md)
/// for the runtime interface.
#[must_use]
pub fn customize_shell(body: impl Into<String>) -> Artifact {
    Artifact::owned_file(CUSTOMIZE_SHELL_PATH, body)
}

/// Add a downstream `PowerShell` customization file (`customize.ps1`).
///
/// See [`customize_shell`] for the shared contract and provenance-neutral
/// loading behavior.
#[must_use]
pub fn customize_powershell(body: impl Into<String>) -> Artifact {
    Artifact::owned_file(CUSTOMIZE_POWERSHELL_PATH, body)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;
    use crate::anvil::artifacts::justfile::dependency_recipe_sources;

    fn write(path: &Path, body: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("test path parent must be creatable");
        }
        std::fs::write(path, body).expect("test file must be writable");
    }

    fn reaches_aprz(recipe: &str, graph: &BTreeMap<String, BTreeSet<String>>, visiting: &mut BTreeSet<String>) -> bool {
        if recipe == "anvil-aprz" {
            return true;
        }
        if !visiting.insert(recipe.to_owned()) {
            return false;
        }
        let reaches = graph
            .get(recipe)
            .is_some_and(|dependencies| dependencies.iter().any(|dependency| reaches_aprz(dependency, graph, visiting)));
        visiting.remove(recipe);
        reaches
    }

    fn run_image_id_command(repo: &Path, command: &str, args: &[&str]) -> String {
        run_image_id_command_with_base(repo, command, args, None)
    }

    fn run_image_id_command_with_base(repo: &Path, command: &str, args: &[&str], base_image: Option<&str>) -> String {
        let mut command = Command::new(command);
        command.args(args).current_dir(repo).env_remove("ANVIL_CONTAINER_BASE_IMAGE");
        if let Some(base_image) = base_image {
            command.env("ANVIL_CONTAINER_BASE_IMAGE", base_image);
        }
        let output = command
            .output()
            .expect("native shell must be available for the container image-id helper");
        assert!(
            output.status.success(),
            "image-id helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("image ID must be UTF-8").trim().to_owned()
    }

    #[cfg(windows)]
    fn run_image_id(repo: &Path) -> String {
        run_image_id_command(repo, "pwsh", &["-NoProfile", "-File", ".anvil/container/image-id.ps1"])
    }

    #[cfg(unix)]
    fn run_image_id(repo: &Path) -> String {
        run_image_id_command(repo, "bash", &[".anvil/container/image-id.sh"])
    }

    fn write_image_id_fixture(root: &Path) {
        write(&root.join("rust-toolchain.toml"), "channel = \"1.93\"\n");
        write(&root.join("justfiles/anvil/versions.just"), "tool_version := \"1\"\n");
        write(
            &root.join(CONTAINERFILE_PATH),
            "ARG BASE_IMAGE=example.invalid/base@sha256:0000000000000000000000000000000000000000000000000000000000000000\nFROM ${BASE_IMAGE}\n",
        );
        write(&root.join(IMAGE_ID_PATH), IMAGE_ID);
        write(&root.join(SHELL_IMAGE_ID_PATH), SHELL_IMAGE_ID);
    }

    #[test]
    fn public_group_has_the_expected_files() {
        let paths: Vec<&str> = all()
            .iter()
            .map(|artifact| match artifact {
                Artifact::OwnedFile(spec) => spec.path,
                Artifact::Region(_) => panic!("container group must contain owned files only"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                RECIPE_PATH,
                CONTAINERFILE_PATH,
                IGNORE_PATH,
                ENTRYPOINT_PATH,
                IMAGE_ID_PATH,
                SHELL_IMAGE_ID_PATH,
                SHELL_DRIVER_PATH,
                POWERSHELL_DRIVER_PATH,
                README_PATH
            ]
        );
    }

    #[test]
    fn containerfile_installs_the_generated_toolset() {
        assert!(CONTAINERFILE.contains("just anvil-setup"));
        assert!(CONTAINERFILE.contains("COPY . ."));
        assert!(IGNORE.contains("!.anvil/container/*"));
        assert!(IGNORE.contains("!justfiles/anvil/checks/*.just"));
        assert!(CONTAINERFILE.contains("anvil_runner := \\\"native\\\""));
        assert!(CONTAINERFILE.contains("requires rust-toolchain.toml"));
        assert!(CONTAINERFILE.contains("anvil-container-entrypoint"));
    }

    /// The Docker build-context ignore evaluation, ported from
    /// `MatchesOrParentMatches` in `moby/patternmatcher`: patterns apply in
    /// order and the last match wins, an `!` pattern applies only while the
    /// candidate is ignored (and a plain pattern only while it is not), and
    /// every pattern is tested against the candidate path *and each of its
    /// parent directories*. Blank and `#` lines are dropped, as
    /// `ignorefile::ReadAll` drops them.
    ///
    /// Only the pattern vocabulary the template actually uses is modeled;
    /// anything else panics rather than silently matching differently from
    /// Docker.
    struct DockerIgnore {
        patterns: Vec<(bool, Vec<String>)>,
    }

    impl DockerIgnore {
        fn parse(text: &str) -> Self {
            let mut patterns = Vec::new();
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (exclusion, body) = line.strip_prefix('!').map_or((false, line), |rest| (true, rest));
                assert!(!body.is_empty(), "illegal exclusion pattern: \"!\"");
                let segments: Vec<String> = body.split('/').map(str::to_owned).collect();
                for segment in &segments {
                    assert!(
                        !segment.contains("**") || (segment == "**" && segments.len() == 1),
                        "only a bare `**` is modeled; `{body}` needs Docker's full regex translation"
                    );
                    assert!(
                        !segment.contains(['[', ']', '\\']),
                        "character classes and escapes are not modeled: {body}"
                    );
                }
                patterns.push((exclusion, segments));
            }
            Self { patterns }
        }

        /// Glob one path segment. `*` and `?` never cross a separator, which
        /// is already guaranteed because the caller splits on `/`.
        fn segment_matches(pattern: &str, segment: &str) -> bool {
            let pattern: Vec<char> = pattern.chars().collect();
            let segment: Vec<char> = segment.chars().collect();
            let (mut p, mut s) = (0, 0);
            let (mut star, mut retry) = (None, 0);
            while s < segment.len() {
                if p < pattern.len() && (pattern[p] == '?' || pattern[p] == segment[s]) {
                    p += 1;
                    s += 1;
                } else if p < pattern.len() && pattern[p] == '*' {
                    star = Some(p);
                    p += 1;
                    retry = s;
                } else if let Some(index) = star {
                    p = index + 1;
                    retry += 1;
                    s = retry;
                } else {
                    return false;
                }
            }
            pattern[p..].iter().all(|character| *character == '*')
        }

        fn pattern_matches(pattern: &[String], path: &str) -> bool {
            if pattern.len() == 1 && pattern[0] == "**" {
                return true;
            }
            let candidate: Vec<&str> = path.split('/').collect();
            pattern.len() == candidate.len()
                && pattern
                    .iter()
                    .zip(candidate)
                    .all(|(pattern, segment)| Self::segment_matches(pattern, segment))
        }

        fn is_ignored(&self, path: &str) -> bool {
            let segments: Vec<&str> = path.split('/').collect();
            let parents: Vec<String> = (1..segments.len()).map(|end| segments[..end].join("/")).collect();
            let mut ignored = false;
            for (exclusion, pattern) in &self.patterns {
                if *exclusion != ignored {
                    continue;
                }
                if Self::pattern_matches(pattern, path) || parents.iter().any(|parent| Self::pattern_matches(pattern, parent)) {
                    ignored = !exclusion;
                }
            }
            ignored
        }
    }

    #[test]
    fn ignore_file_admits_only_image_inputs_into_the_build_context() {
        let ignore = DockerIgnore::parse(IGNORE);

        for included in [
            "rust-toolchain.toml",
            "justfiles/anvil/mod.just",
            "justfiles/anvil/versions.just",
            "justfiles/anvil/checks/clippy.just",
            "justfiles/anvil/groups/pr-fast.just",
            // The entry recipe is not image content, but `mod.just` imports
            // it unconditionally, so `just anvil-setup` needs it present.
            RECIPE_PATH,
            CONTAINERFILE_PATH,
            IGNORE_PATH,
            ENTRYPOINT_PATH,
            IMAGE_ID_PATH,
            SHELL_IMAGE_ID_PATH,
            SHELL_DRIVER_PATH,
            POWERSHELL_DRIVER_PATH,
            README_PATH,
        ] {
            assert!(!ignore.is_ignored(included), "{included} must reach the build context");
        }

        for excluded in [
            // Trusted host orchestration: never image content, even though
            // the surrounding directory is admitted.
            CUSTOMIZE_SHELL_PATH,
            CUSTOMIZE_POWERSHELL_PATH,
            // Assets stranded at the pre-move location, including a
            // hand-authored customization file, must not re-enter through a
            // directory-level re-inclusion.
            "justfiles/anvil/container/customize.sh",
            "justfiles/anvil/container/customize.ps1",
            "justfiles/anvil/container/Containerfile",
            "justfiles/anvil/container/run-in-container.sh",
            // Everything else stays out: the working tree is bind-mounted at
            // run time rather than baked into the image.
            "Cargo.toml",
            "crates/example/src/lib.rs",
            "justfiles/basic.just",
            "justfiles/anvil/notes.md",
            ".anvil.lock",
            ".anvil/other/asset.txt",
            ".git/config",
        ] {
            assert!(ignore.is_ignored(excluded), "{excluded} must not reach the build context");
        }
    }

    #[test]
    fn ignore_file_excludes_customize_source_from_the_build_context() {
        // Order is load-bearing: the customize re-exclusions only win because
        // they come after the directory-wide re-inclusion.
        let include_position = IGNORE
            .find("!.anvil/container/*")
            .expect("the container directory inclusion is asserted above");
        let shell_exclude_position = IGNORE
            .find("\n.anvil/container/customize.sh")
            .expect("customize.sh must be excluded from the build context");
        let powershell_exclude_position = IGNORE
            .find("\n.anvil/container/customize.ps1")
            .expect("customize.ps1 must be excluded from the build context");
        assert!(
            include_position < shell_exclude_position && include_position < powershell_exclude_position,
            "the re-exclusion must come after the broad directory inclusion so it wins"
        );
    }

    #[test]
    fn drivers_use_docker_and_content_addressing() {
        assert!(RECIPE.contains("replace(recipe, \"'\", \"''\")"));
        for (driver, customization_source, build_command) in [
            (SHELL_DRIVER, "source \"$customize_script\"", "docker build \\"),
            (POWERSHELL_DRIVER, ". $customizeScript", "& wsl -e docker build"),
        ] {
            assert!(driver.contains("docker"));
            assert!(driver.contains("ANVIL_CONTAINER_NO_REBUILD"));
            assert!(driver.contains("ANVIL_CONTAINER_BASE_IMAGE"));
            assert!(driver.contains("ANVIL_CONTAINER_IMAGE"));
            assert!(driver.contains("ANVIL_IN_CONTAINER"));
            assert!(driver.contains("auth token --hostname github.com"));
            assert!(driver.contains("gh auth login --hostname github.com"));
            assert!(driver.contains("/run/secrets/anvil-github-token"));
            assert!(driver.contains("anvil-pr-fast"));
            assert!(driver.contains("anvil-scheduled-advisories"));
            assert!(driver.contains("PR_TITLE"));
            assert!(driver.contains("--pull=never"));
            assert!(driver.contains("linux/amd64"));
            assert!(driver.contains("ANVIL_APRZ_ALREADY_RAN"));
            assert!(!driver.contains("--env GITHUB_TOKEN"));
            let auth_position = driver
                .find("gh auth login --hostname github.com")
                .expect("GitHub login command is asserted present above");
            let image_position = driver
                .find("docker image inspect")
                .expect("Docker image check is asserted present above");
            let customization_position = driver
                .find(customization_source)
                .expect("customization source command must be present");
            let build_position = driver.find(build_command).expect("Docker build command must be present");
            assert!(
                image_position < customization_position && customization_position < auth_position && auth_position < build_position,
                "customization must load before GitHub authentication, and authentication must finish before image building"
            );
        }
        assert!(POWERSHELL_DRIVER.contains("image-id.ps1"));
        assert!(IMAGE_ID.contains("[StringComparer]::Ordinal"));
        assert!(POWERSHELL_DRIVER.contains("AnvilContainerPrepareCommand"));
        assert!(POWERSHELL_DRIVER.contains("wsl -e docker"));
        assert!(!POWERSHELL_DRIVER.contains("BuildInMachine"));
        assert!(POWERSHELL_DRIVER.contains("git rev-parse --show-toplevel 2>$null"));
        assert!(IMAGE_ID.contains("git rev-parse --show-toplevel 2>$null"));
        assert!(POWERSHELL_DRIVER.contains("Test-AnvilRecipeNeedsGitHubToken $recipeArg"));
        assert!(POWERSHELL_DRIVER.contains("foreach ($recipeArg in $Recipe)"));
        assert!(POWERSHELL_DRIVER.contains("[Console]::IsInputRedirected"));
        assert!(POWERSHELL_DRIVER.contains("Read-Host"));
        assert!(POWERSHELL_DRIVER.contains("ConvertTo-AnvilVersion"));
        assert!(POWERSHELL_DRIVER.contains("isolated anvil-aprz"));
        assert!(POWERSHELL_DRIVER.contains("docker volume create"));
        assert!(POWERSHELL_DRIVER.contains("--user', \"${containerUid}:${containerGid}\""));
        let token_file_create_position = POWERSHELL_DRIVER
            .find("[IO.File]::Create($githubTokenFile).Dispose()")
            .expect("the temporary GitHub token file must be created before permissions are restricted");
        let token_file_windows_restrict_position = POWERSHELL_DRIVER
            .find("& icacls.exe $githubTokenFile")
            .expect("the temporary GitHub token file must have a restricted Windows ACL");
        let token_file_unix_restrict_position = POWERSHELL_DRIVER
            .find("& chmod 600 $githubTokenFile")
            .expect("the temporary GitHub token file must have restricted Unix permissions");
        let token_file_write_position = POWERSHELL_DRIVER
            .find("[IO.File]::WriteAllText($githubTokenFile")
            .expect("the GitHub token must be written to the restricted temporary file");
        assert!(
            token_file_create_position < token_file_windows_restrict_position
                && token_file_create_position < token_file_unix_restrict_position
                && token_file_windows_restrict_position < token_file_write_position
                && token_file_unix_restrict_position < token_file_write_position,
            "the temporary GitHub token file must be restricted before the token is written"
        );
        assert!(SHELL_DRIVER.contains("anvil_recipe_needs_github_token \"$recipe_arg\""));
        assert!(SHELL_DRIVER.contains("for recipe_arg in \"$@\""));
        assert!(SHELL_DRIVER.contains("image-id.sh"));
        assert!(!SHELL_DRIVER.contains("pwsh"));
        assert!(SHELL_DRIVER.contains("anvil-container must run from a Git repository"));
        assert!(SHELL_DRIVER.contains("[[ ! -t 0 ]]"));
        assert!(SHELL_DRIVER.contains("read -r -p"));
        assert!(SHELL_DRIVER.contains("github_run_args"));
        assert!(SHELL_DRIVER.contains("just anvil-aprz"));
        assert!(SHELL_DRIVER.contains("docker volume create"));
        assert!(SHELL_DRIVER.contains("--user \"$container_uid:$container_gid\""));
    }

    #[test]
    fn github_token_recipe_lists_match_the_generated_dependency_graph() {
        fn anvil_recipe_tokens(text: &str) -> impl Iterator<Item = &str> {
            text.split(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-')))
                .filter(|token| token.starts_with("anvil-") || token.starts_with("_anvil-"))
        }

        let mut graph = BTreeMap::<String, BTreeSet<String>>::new();

        for source in dependency_recipe_sources() {
            let mut current = None::<String>;
            for line in source.lines() {
                if !line.chars().next().is_some_and(char::is_whitespace) {
                    current = line
                        .split_once(':')
                        .and_then(|(header, _)| header.split_whitespace().next())
                        .filter(|name| name.starts_with("anvil-") || name.starts_with("_anvil-"))
                        .map(str::to_owned);
                }
                let Some(recipe) = current.as_ref() else {
                    continue;
                };
                let dependency_text = line.split_once(':').map_or(line, |(_, dependencies)| dependencies);
                let dependencies = graph.entry(recipe.clone()).or_default();
                dependencies.extend(
                    anvil_recipe_tokens(dependency_text)
                        .map(str::to_owned)
                        .filter(|dependency| dependency != recipe),
                );
                if let Some((_, routed)) = dependency_text.split_once("_anvil-run \"")
                    && let Some(tier) = routed.split('"').next()
                {
                    dependencies.insert(format!("_anvil-{tier}"));
                }
            }
        }

        let mut expected = BTreeSet::from(["anvil-aprz".to_owned()]);
        expected.extend(
            graph
                .keys()
                .filter(|recipe| reaches_aprz(recipe, &graph, &mut BTreeSet::new()))
                .cloned(),
        );

        let driver_recipes = |driver: &str, start: &str, end: &str| {
            let body = driver
                .split_once(start)
                .and_then(|(_, remainder)| remainder.split_once(end).map(|(body, _)| body))
                .expect("driver token-classification function must have stable boundaries");
            anvil_recipe_tokens(body).map(str::to_owned).collect::<BTreeSet<_>>()
        };

        let shell = driver_recipes(SHELL_DRIVER, "anvil_recipe_needs_github_token() {", "}\n\nversion_at_least");
        let powershell = driver_recipes(
            POWERSHELL_DRIVER,
            "function Test-AnvilRecipeNeedsGitHubToken",
            "\n}\n\nfunction Get-AnvilGitHubToken",
        );
        assert_eq!(shell, expected, "Bash token routing must match APRZ reachability");
        assert_eq!(powershell, expected, "PowerShell token routing must match APRZ reachability");
    }

    #[test]
    fn drivers_implement_the_customization_contract() {
        assert!(SHELL_DRIVER.contains("customize.sh"));
        assert!(!SHELL_DRIVER.contains("auth.sh"));
        assert!(!SHELL_DRIVER.contains("CUSTOMIZATION_API_VERSION"));
        assert!(POWERSHELL_DRIVER.contains("customize.ps1"));
        assert!(!POWERSHELL_DRIVER.contains("auth.ps1"));
        assert!(!POWERSHELL_DRIVER.contains("CustomizationApiVersion"));

        for (driver, image_exists, requested_recipes, needs_github_token) in [
            (
                SHELL_DRIVER,
                "ANVIL_CONTAINER_IMAGE_EXISTS",
                "ANVIL_CONTAINER_REQUESTED_RECIPES",
                "ANVIL_CONTAINER_NEEDS_GITHUB_TOKEN",
            ),
            (
                POWERSHELL_DRIVER,
                "AnvilContainerImageExists",
                "AnvilContainerRequestedRecipes",
                "AnvilContainerNeedsGitHubToken",
            ),
        ] {
            assert!(driver.contains("ANVIL_CONTAINER_REPO_ROOT") || driver.contains("AnvilContainerRepoRoot"));
            assert!(driver.contains("ANVIL_CONTAINER_DIR") || driver.contains("AnvilContainerDir"));
            assert!(driver.contains("ANVIL_CONTAINER_RESOLVED_IMAGE") || driver.contains("AnvilContainerResolvedImage"));
            assert!(driver.contains(image_exists));
            assert!(driver.contains(requested_recipes));
            assert!(driver.contains(needs_github_token));

            // The image-exists check must be resolved before the
            // customization file is sourced, so warm-run state is available
            // to it.
            let image_exists_position = driver
                .find(image_exists)
                .unwrap_or_else(|| panic!("{image_exists} is asserted present above"));
            let source_position = driver
                .find("customize.sh")
                .or_else(|| driver.find("customize.ps1"))
                .expect("customize.* sourcing is asserted present above");
            assert!(
                image_exists_position < source_position,
                "image existence must be resolved before customization is sourced"
            );
        }

        assert!(POWERSHELL_DRIVER.contains("AnvilContainerHostIsWindows"));
        assert!(!SHELL_DRIVER.contains("ANVIL_CONTAINER_HOST_IS_WINDOWS"));

        // Preparation arguments without a preparation command must fail
        // validation before Docker build/run.
        assert!(SHELL_DRIVER.contains("ANVIL_CONTAINER_PREPARE_ARGS requires ANVIL_CONTAINER_PREPARE_COMMAND"));
        assert!(POWERSHELL_DRIVER.contains("$AnvilContainerPrepareArgs requires $AnvilContainerPrepareCommand"));

        // Cleanup callback shape is validated.
        assert!(SHELL_DRIVER.contains("must name a callable function"));
        assert!(POWERSHELL_DRIVER.contains("must be a script block"));

        // Output arrays are validated before Docker is invoked.
        for driver in [SHELL_DRIVER, POWERSHELL_DRIVER] {
            let validate_position = driver
                .find("must be a string array")
                .or_else(|| driver.find("anvil_container_validate_array"))
                .expect("output validation is present");
            let build_position = driver.find("docker build").expect("build invocation is present");
            assert!(
                validate_position < build_position,
                "output validation must occur before Docker build"
            );
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    fn image_id_excludes_customize_source_but_hashes_static_container_files() {
        let tmp = TempDir::new().expect("temporary repository must be creatable");
        let root = tmp.path();
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .expect("git must be available for the image-id helper");
        assert!(status.success(), "temporary Git repository must initialize");
        write_image_id_fixture(root);

        let base = run_image_id(root);

        // Customization source is runtime orchestration, not image content: it
        // must never affect the image ID, in either host-shell form.
        let customize_sh = root.join(CUSTOMIZE_SHELL_PATH);
        write(&customize_sh, "# customization\n");
        assert_eq!(base, run_image_id(root), "customize.sh source must not affect the image ID");

        let customize_ps1 = root.join(CUSTOMIZE_POWERSHELL_PATH);
        write(&customize_ps1, "# customization\n");
        assert_eq!(base, run_image_id(root), "customize.ps1 source must not affect the image ID");

        write(&customize_sh, "# different customization\n");
        write(&customize_ps1, "# different customization\n");
        assert_eq!(
            base,
            run_image_id(root),
            "changed customization source must still not affect the image ID"
        );

        write(&root.join(README_PATH), "runtime documentation change\n");
        assert_eq!(
            base,
            run_image_id(root),
            "execution-only documentation must not affect the image ID"
        );

        write(&root.join(RECIPE_PATH), "execution-only recipe change\n");
        assert_eq!(base, run_image_id(root), "the container entry recipe must not affect the image ID");

        let override_image = "example.invalid/bullseye@sha256:1111111111111111111111111111111111111111111111111111111111111111";
        #[cfg(windows)]
        let overridden = run_image_id_command_with_base(
            root,
            "pwsh",
            &["-NoProfile", "-File", ".anvil/container/image-id.ps1"],
            Some(override_image),
        );
        #[cfg(unix)]
        let overridden = run_image_id_command_with_base(root, "bash", &[".anvil/container/image-id.sh"], Some(override_image));
        assert_ne!(base, overridden, "the selected base image must affect the image ID");

        write(&root.join("justfiles/anvil/checks/extra.just"), "anvil-extra:\n    @echo extra\n");
        assert_ne!(
            base,
            run_image_id(root),
            "only the container entry recipe itself is execution-only; other recipes are hashed"
        );
        std::fs::remove_file(root.join("justfiles/anvil/checks/extra.just")).expect("test file must be removable");
        assert_eq!(base, run_image_id(root), "removing the extra recipe must restore the image ID");

        // Static, hashed image content must still affect the image ID.
        write(
            &root.join(CONTAINERFILE_PATH),
            "ARG BASE_IMAGE=example.invalid/base@sha256:0000000000000000000000000000000000000000000000000000000000000000\nFROM ${BASE_IMAGE}\nRUN echo changed\n",
        );
        assert_ne!(
            base,
            run_image_id(root),
            "changed static Containerfile content must affect the image ID"
        );
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "uses filesystem and subprocesses; miri isolation forbids them")]
    fn image_id_helpers_match_when_pwsh_is_available() {
        if Command::new("pwsh").arg("-Version").output().is_err() {
            return;
        }

        let tmp = TempDir::new().expect("temporary repository must be creatable");
        let root = tmp.path();
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .expect("git must be available for the image-id helpers");
        assert!(status.success(), "temporary Git repository must initialize");
        write_image_id_fixture(root);
        write(
            &root.join("justfiles/anvil/checks/custom.just"),
            "nested-custom-recipe:\n    @echo custom\n",
        );
        // The entry recipe is skipped by both helpers; seed it so the skip
        // itself is compared, not just the recipes they agree to hash.
        write(&root.join(RECIPE_PATH), "execution-only:\n    @echo entry\n");

        let shell = run_image_id_command(root, "bash", &[".anvil/container/image-id.sh"]);
        let powershell = run_image_id_command(root, "pwsh", &["-NoProfile", "-File", ".anvil/container/image-id.ps1"]);
        assert_eq!(shell, powershell);
    }

    #[test]
    fn shell_driver_supports_legacy_bash() {
        assert!(!SHELL_DRIVER.contains("sort -V"));
        assert!(!SHELL_DRIVER.contains("[[ -v"));
        assert!(SHELL_DRIVER.contains("version_at_least"));
        assert!(SHELL_DRIVER.contains("if command -v sha256sum"));
        assert!(SHELL_DRIVER.contains("shasum -a 256"));
        assert!(SHELL_DRIVER.contains("printenv"));
        assert!(SHELL_DRIVER.contains("declare -p"));
        assert!(SHELL_IMAGE_ID.contains("shasum -a 256"));
        assert!(SHELL_IMAGE_ID.contains("LC_ALL=C sort -u"));
        assert!(!SHELL_IMAGE_ID.contains("pwsh"));

        // Namerefs (`local -n`/`declare -n`) require Bash 4.3+. Array-name
        // validation must pass elements positionally instead.
        assert!(!SHELL_DRIVER.contains("local -n"), "namerefs are unsupported on Bash 3.2");
        assert!(!SHELL_DRIVER.contains("declare -n"), "namerefs are unsupported on Bash 3.2");

        // Every possibly-empty customization-output array must be expanded
        // with the `${arr[@]+"${arr[@]}"}` idiom, not a bare `"${arr[@]}"`:
        // under `set -u`, Bash versions before 4.4 raise "unbound variable"
        // when a declared-but-empty array is expanded bare. The guarded
        // idiom necessarily contains the bare form as a substring, so pin
        // safety by asserting every bare occurrence is part of a guarded
        // one (equal counts) rather than absent outright.
        for array in [
            "ANVIL_CONTAINER_BUILD_ARGS",
            "ANVIL_CONTAINER_PREPARE_ARGS",
            "ANVIL_CONTAINER_RUN_ARGS",
        ] {
            let guarded = format!("${{{array}[@]+\"${{{array}[@]}}\"}}");
            let bare = format!("\"${{{array}[@]}}\"");
            let guarded_count = SHELL_DRIVER.matches(&guarded).count();
            let bare_count = SHELL_DRIVER.matches(&bare).count();
            assert!(guarded_count > 0, "{array} must use the nounset-safe empty-array idiom: {guarded}");
            assert_eq!(
                guarded_count, bare_count,
                "{array} must never be expanded bare outside the nounset-safe idiom (unsafe under `set -u` on Bash <4.4)"
            );
        }
    }

    #[test]
    fn recipe_uses_native_host_interpreters() {
        assert!(RECIPE.contains("[windows]"));
        assert!(RECIPE.contains("[script(\"pwsh\")]"));
        assert!(RECIPE.contains("[unix]"));
        assert!(RECIPE.contains("[script(\"bash\")]"));
        assert!(!RECIPE.contains("$IsWindows"));
    }

    #[test]
    fn entrypoint_initializes_non_root_cargo_metadata() {
        for file in ["config.toml", ".crates.toml", ".crates2.json"] {
            assert!(ENTRYPOINT.contains(file));
        }
        assert!(ENTRYPOINT.contains("export CARGO_HOME"));
        assert!(ENTRYPOINT.contains("ln -sfn /usr/local/cargo/registry"));
        assert!(ENTRYPOINT.contains("ln -sfn /usr/local/cargo/git"));
        assert!(ENTRYPOINT.contains("exec \"$@\""));
    }

    #[test]
    fn drivers_support_interactive_shell_mode() {
        assert!(SHELL_DRIVER.contains("--interactive --tty"));
        assert!(SHELL_DRIVER.contains("\"$image\" bash"));
        assert!(POWERSHELL_DRIVER.contains("wsl -e docker @runArgs --interactive --tty $image bash"));
    }

    #[test]
    fn customize_helpers_use_the_standard_paths() {
        match customize_shell("# shell customization\n") {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, CUSTOMIZE_SHELL_PATH);
                assert_eq!(spec.body, "# shell customization\n");
            }
            Artifact::Region(_) => panic!("customization file must be an owned file"),
        }
        match customize_powershell("# PowerShell customization\n") {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, CUSTOMIZE_POWERSHELL_PATH);
                assert_eq!(spec.body, "# PowerShell customization\n");
            }
            Artifact::Region(_) => panic!("customization file must be an owned file"),
        }
    }
}
