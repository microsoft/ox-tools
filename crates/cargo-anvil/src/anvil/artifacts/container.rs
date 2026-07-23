// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The optional local container backend.
//!
//! The base catalog emits a public Podman implementation. Downstream catalogs
//! replace only environment-specific artifacts such as the Containerfile and
//! add an optional `customize.sh`/`customize.ps1` runtime customization file.

use crate::catalog::Artifact;

const RECIPE: &str = include_str!("../../../templates/justfiles/anvil/container/container.just");
const CONTAINERFILE: &str = include_str!("../../../templates/justfiles/anvil/container/Containerfile");
const IGNORE: &str = include_str!("../../../templates/justfiles/anvil/container/container.ignore");
const ENTRYPOINT: &str = include_str!("../../../templates/justfiles/anvil/container/entrypoint.sh");
const IMAGE_ID: &str = include_str!("../../../templates/justfiles/anvil/container/image-id.ps1");
const SHELL_IMAGE_ID: &str = include_str!("../../../templates/justfiles/anvil/container/image-id.sh");
const SHELL_DRIVER: &str = include_str!("../../../templates/justfiles/anvil/container/run-in-container.sh");
const POWERSHELL_DRIVER: &str = include_str!("../../../templates/justfiles/anvil/container/run-in-container.ps1");
const README: &str = include_str!("../../../templates/justfiles/anvil/container/README.md");

const RECIPE_PATH: &str = "justfiles/anvil/container/container.just";
const CONTAINERFILE_PATH: &str = "justfiles/anvil/container/Containerfile";
const IGNORE_PATH: &str = "justfiles/anvil/container/container.ignore";
const ENTRYPOINT_PATH: &str = "justfiles/anvil/container/entrypoint.sh";
const IMAGE_ID_PATH: &str = "justfiles/anvil/container/image-id.ps1";
const SHELL_IMAGE_ID_PATH: &str = "justfiles/anvil/container/image-id.sh";
const SHELL_DRIVER_PATH: &str = "justfiles/anvil/container/run-in-container.sh";
const POWERSHELL_DRIVER_PATH: &str = "justfiles/anvil/container/run-in-container.ps1";
const README_PATH: &str = "justfiles/anvil/container/README.md";
const CUSTOMIZE_SHELL_PATH: &str = "justfiles/anvil/container/customize.sh";
const CUSTOMIZE_POWERSHELL_PATH: &str = "justfiles/anvil/container/customize.ps1";

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

/// The restricted Podman build-context ignore file.
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

/// The Linux/WSL Podman driver.
#[must_use]
pub fn shell_driver() -> Artifact {
    Artifact::owned_file(SHELL_DRIVER_PATH, SHELL_DRIVER)
}

/// The native-Windows Podman driver.
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
/// for the runtime interface and its compatibility version.
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
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    fn write(path: &Path, body: impl AsRef<[u8]>) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("test path parent must be creatable");
        }
        std::fs::write(path, body).expect("test file must be writable");
    }

    fn run_image_id_command(repo: &Path, command: &str, args: &[&str]) -> String {
        let output = Command::new(command)
            .args(args)
            .current_dir(repo)
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
        run_image_id_command(repo, "pwsh", &["-NoProfile", "-File", "justfiles/anvil/container/image-id.ps1"])
    }

    #[cfg(unix)]
    fn run_image_id(repo: &Path) -> String {
        run_image_id_command(repo, "bash", &["justfiles/anvil/container/image-id.sh"])
    }

    fn write_image_id_fixture(root: &Path) {
        write(&root.join("rust-toolchain.toml"), "channel = \"1.93\"\n");
        write(&root.join("justfiles/anvil/versions.just"), "tool_version := \"1\"\n");
        write(&root.join(CONTAINERFILE_PATH), "FROM example.invalid/base\n");
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
        assert!(IGNORE.contains("!justfiles/anvil/container/*"));
        assert!(IGNORE.contains("!justfiles/anvil/checks/*.just"));
        assert!(CONTAINERFILE.contains("anvil_runner := \\\"native\\\""));
        assert!(CONTAINERFILE.contains("requires rust-toolchain.toml"));
        assert!(CONTAINERFILE.contains("anvil-container-entrypoint"));
    }

    #[test]
    fn ignore_file_excludes_customize_source_from_the_build_context() {
        // Customization is trusted host orchestration, not image content: it
        // must never reach the build context, even though the broader
        // container directory is included above.
        let include_position = IGNORE
            .find("!justfiles/anvil/container/*")
            .expect("the container directory inclusion is asserted above");
        let shell_exclude_position = IGNORE
            .find("justfiles/anvil/container/customize.sh")
            .expect("customize.sh must be excluded from the build context");
        let powershell_exclude_position = IGNORE
            .find("justfiles/anvil/container/customize.ps1")
            .expect("customize.ps1 must be excluded from the build context");
        assert!(
            !IGNORE[shell_exclude_position..].starts_with('!'),
            "customize.sh must be a re-exclusion, not an inclusion"
        );
        assert!(
            !IGNORE[powershell_exclude_position..].starts_with('!'),
            "customize.ps1 must be a re-exclusion, not an inclusion"
        );
        assert!(
            include_position < shell_exclude_position && include_position < powershell_exclude_position,
            "the re-exclusion must come after the broad directory inclusion so it wins"
        );
    }

    #[test]
    fn drivers_use_podman_and_content_addressing() {
        for driver in [SHELL_DRIVER, POWERSHELL_DRIVER] {
            assert!(driver.contains("podman"));
            assert!(driver.contains("ANVIL_CONTAINER_NO_REBUILD"));
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
                .find("podman image exists")
                .expect("Podman image check is asserted present above");
            assert!(
                auth_position < image_position,
                "GitHub authentication must be checked before image building"
            );
        }
        assert!(POWERSHELL_DRIVER.contains("AnvilContainerBuildInMachine"));
        assert!(POWERSHELL_DRIVER.contains("image-id.ps1"));
        assert!(IMAGE_ID.contains("[StringComparer]::Ordinal"));
        assert!(POWERSHELL_DRIVER.contains("AnvilContainerPrepareCommand"));
        assert!(POWERSHELL_DRIVER.contains("podman machine ssh"));
        assert!(POWERSHELL_DRIVER.contains("git rev-parse --show-toplevel 2>$null"));
        assert!(IMAGE_ID.contains("git rev-parse --show-toplevel 2>$null"));
        assert!(POWERSHELL_DRIVER.contains("Test-AnvilRecipeNeedsGitHubToken $Recipe[0]"));
        assert!(!POWERSHELL_DRIVER.contains("foreach ($name in $Recipe)"));
        assert!(POWERSHELL_DRIVER.contains("[Console]::IsInputRedirected"));
        assert!(POWERSHELL_DRIVER.contains("Read-Host"));
        assert!(POWERSHELL_DRIVER.contains("$singleQuote + $doubleQuote"));
        assert!(POWERSHELL_DRIVER.contains("ConvertTo-AnvilVersion"));
        assert!(POWERSHELL_DRIVER.contains("isolated anvil-aprz"));
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
        assert!(SHELL_DRIVER.contains("anvil_recipe_needs_github_token \"$1\""));
        assert!(!SHELL_DRIVER.contains("for recipe in \"$@\""));
        assert!(SHELL_DRIVER.contains("image-id.sh"));
        assert!(!SHELL_DRIVER.contains("pwsh"));
        assert!(SHELL_DRIVER.contains("anvil-container must run from a Git repository"));
        assert!(SHELL_DRIVER.contains("[[ ! -t 0 ]]"));
        assert!(SHELL_DRIVER.contains("read -r -p"));
        assert!(SHELL_DRIVER.contains("github_run_args"));
        assert!(SHELL_DRIVER.contains("just anvil-aprz"));
    }

    #[test]
    fn drivers_implement_the_versioned_customization_contract() {
        assert!(SHELL_DRIVER.contains("customize.sh"));
        assert!(!SHELL_DRIVER.contains("auth.sh"));
        assert!(POWERSHELL_DRIVER.contains("customize.ps1"));
        assert!(!POWERSHELL_DRIVER.contains("auth.ps1"));

        for (driver, api_version, image_exists, requested_recipes) in [
            (
                SHELL_DRIVER,
                "ANVIL_CONTAINER_CUSTOMIZATION_API_VERSION=1",
                "ANVIL_CONTAINER_IMAGE_EXISTS",
                "ANVIL_CONTAINER_REQUESTED_RECIPES",
            ),
            (
                POWERSHELL_DRIVER,
                "AnvilContainerCustomizationApiVersion -Value 1",
                "AnvilContainerImageExists",
                "AnvilContainerRequestedRecipes",
            ),
        ] {
            assert!(driver.contains(api_version), "{api_version} must be present");
            assert!(driver.contains("ANVIL_CONTAINER_REPO_ROOT") || driver.contains("AnvilContainerRepoRoot"));
            assert!(driver.contains("ANVIL_CONTAINER_DIR") || driver.contains("AnvilContainerDir"));
            assert!(driver.contains("ANVIL_CONTAINER_RESOLVED_IMAGE") || driver.contains("AnvilContainerResolvedImage"));
            assert!(driver.contains(image_exists));
            assert!(driver.contains(requested_recipes));

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
        // validation before Podman build/run.
        assert!(SHELL_DRIVER.contains("ANVIL_CONTAINER_PREPARE_ARGS requires ANVIL_CONTAINER_PREPARE_COMMAND"));
        assert!(POWERSHELL_DRIVER.contains("$AnvilContainerPrepareArgs requires $AnvilContainerPrepareCommand"));

        // Cleanup callback shape is validated.
        assert!(SHELL_DRIVER.contains("must name a callable function"));
        assert!(POWERSHELL_DRIVER.contains("must be a script block"));

        // Output arrays are validated before Podman is invoked.
        for driver in [SHELL_DRIVER, POWERSHELL_DRIVER] {
            let validate_position = driver
                .find("must be a string array")
                .or_else(|| driver.find("anvil_container_validate_array"))
                .expect("output validation is present");
            let build_position = driver.find("podman build").expect("build invocation is present");
            assert!(
                validate_position < build_position,
                "output validation must occur before Podman build"
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

        // Static, hashed image content must still affect the image ID.
        write(&root.join(CONTAINERFILE_PATH), "FROM example.invalid/different-base\n");
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
            &root.join("justfiles/anvil/container/custom.just"),
            "custom-recipe:\n    @echo custom\n",
        );

        let shell = run_image_id_command(root, "bash", &["justfiles/anvil/container/image-id.sh"]);
        let powershell = run_image_id_command(root, "pwsh", &["-NoProfile", "-File", "justfiles/anvil/container/image-id.ps1"]);
        assert_eq!(shell, powershell);
    }

    #[test]
    fn shell_driver_avoids_macos_incompatible_constructs() {
        assert!(!SHELL_DRIVER.contains("sort -V"));
        assert!(!SHELL_DRIVER.contains("[[ -v"));
        assert!(SHELL_DRIVER.contains("version_at_least"));
        assert!(SHELL_DRIVER.contains("if command -v sha256sum"));
        assert!(SHELL_DRIVER.contains("shasum -a 256"));
        assert!(SHELL_DRIVER.contains("declare -p"));
        assert!(SHELL_IMAGE_ID.contains("shasum -a 256"));
        assert!(SHELL_IMAGE_ID.contains("LC_ALL=C sort -u"));
        assert!(!SHELL_IMAGE_ID.contains("pwsh"));

        // Namerefs (`local -n`/`declare -n`) require Bash 4.3+; macOS's system
        // Bash is 3.2. Array-name validation must pass elements positionally
        // instead.
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
        assert!(POWERSHELL_DRIVER.contains("@runArgs --interactive --tty $image bash"));
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
