// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The optional local container backend.
//!
//! The base catalog emits a public Podman implementation. Downstream catalogs
//! replace only environment-specific artifacts such as the Containerfile and
//! add optional auth hooks.

use crate::catalog::Artifact;

const RECIPE: &str = include_str!("../../../templates/justfiles/anvil/container/container.just");
const CONTAINERFILE: &str = include_str!("../../../templates/justfiles/anvil/container/Containerfile");
const IGNORE: &str = include_str!("../../../templates/justfiles/anvil/container/container.ignore");
const ENTRYPOINT: &str = include_str!("../../../templates/justfiles/anvil/container/entrypoint.sh");
const IMAGE_ID: &str = include_str!("../../../templates/justfiles/anvil/container/image-id.ps1");
const SHELL_DRIVER: &str = include_str!("../../../templates/justfiles/anvil/container/run-in-container.sh");
const POWERSHELL_DRIVER: &str = include_str!("../../../templates/justfiles/anvil/container/run-in-container.ps1");
const README: &str = include_str!("../../../templates/justfiles/anvil/container/README.md");

const RECIPE_PATH: &str = "justfiles/anvil/container/container.just";
const CONTAINERFILE_PATH: &str = "justfiles/anvil/container/Containerfile";
const IGNORE_PATH: &str = "justfiles/anvil/container/container.ignore";
const ENTRYPOINT_PATH: &str = "justfiles/anvil/container/entrypoint.sh";
const IMAGE_ID_PATH: &str = "justfiles/anvil/container/image-id.ps1";
const SHELL_DRIVER_PATH: &str = "justfiles/anvil/container/run-in-container.sh";
const POWERSHELL_DRIVER_PATH: &str = "justfiles/anvil/container/run-in-container.ps1";
const README_PATH: &str = "justfiles/anvil/container/README.md";
const AUTH_SHELL_PATH: &str = "justfiles/anvil/container/auth.sh";
const AUTH_POWERSHELL_PATH: &str = "justfiles/anvil/container/auth.ps1";

/// The full public container artifact group.
#[must_use]
pub fn all() -> Vec<Artifact> {
    vec![
        recipe(),
        containerfile(),
        ignore_file(),
        entrypoint(),
        image_id(),
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

/// Add a downstream shell authentication hook.
#[must_use]
pub fn auth_shell(body: impl Into<String>) -> Artifact {
    Artifact::owned_file(AUTH_SHELL_PATH, body)
}

/// Add a downstream `PowerShell` authentication hook.
#[must_use]
pub fn auth_powershell(body: impl Into<String>) -> Artifact {
    Artifact::owned_file(AUTH_POWERSHELL_PATH, body)
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

    fn run_image_id(repo: &Path) -> String {
        let output = Command::new("pwsh")
            .args(["-NoProfile", "-File", "justfiles/anvil/container/image-id.ps1"])
            .current_dir(repo)
            .output()
            .expect("pwsh must be available for the container image-id helper");
        assert!(
            output.status.success(),
            "image-id.ps1 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("image ID must be UTF-8").trim().to_owned()
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
    fn drivers_use_podman_and_content_addressing() {
        for driver in [SHELL_DRIVER, POWERSHELL_DRIVER] {
            assert!(driver.contains("podman"));
            assert!(driver.contains("image-id.ps1"));
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
        assert!(POWERSHELL_DRIVER.contains("AnvilContainerPrepareCommand"));
        assert!(POWERSHELL_DRIVER.contains("podman machine ssh"));
        assert!(POWERSHELL_DRIVER.contains("foreach ($name in $Recipe)"));
        assert!(POWERSHELL_DRIVER.contains("[Console]::IsInputRedirected"));
        assert!(POWERSHELL_DRIVER.contains("Read-Host"));
        assert!(POWERSHELL_DRIVER.contains("$singleQuote + $doubleQuote"));
        assert!(POWERSHELL_DRIVER.contains("ConvertTo-AnvilVersion"));
        assert!(POWERSHELL_DRIVER.contains("isolated anvil-aprz"));
        assert!(SHELL_DRIVER.contains("for recipe in \"$@\""));
        assert!(SHELL_DRIVER.contains("[[ ! -t 0 ]]"));
        assert!(SHELL_DRIVER.contains("read -r -p"));
        assert!(SHELL_DRIVER.contains("github_run_args"));
        assert!(SHELL_DRIVER.contains("just anvil-aprz"));
    }

    #[test]
    fn image_id_hashes_auth_source_but_not_execution_only_files() {
        let tmp = TempDir::new().expect("temporary repository must be creatable");
        let root = tmp.path();
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root)
            .status()
            .expect("git must be available for the image-id helper");
        assert!(status.success(), "temporary Git repository must initialize");
        write(&root.join("rust-toolchain.toml"), "channel = \"1.93\"\n");
        write(&root.join("justfiles/anvil/versions.just"), "tool_version := \"1\"\n");
        write(&root.join(CONTAINERFILE_PATH), "FROM example.invalid/base\n");
        write(&root.join(IMAGE_ID_PATH), IMAGE_ID);

        let base = run_image_id(root);
        let auth_path = root.join(AUTH_SHELL_PATH);
        write(&auth_path, "# build configuration\n");
        let auth_lf = run_image_id(root);
        assert_ne!(base, auth_lf, "auth-hook source must affect the image ID");

        write(&auth_path, b"# build configuration\r\n");
        let auth_crlf = run_image_id(root);
        assert_eq!(auth_lf, auth_crlf, "line endings must not affect the image ID");

        write(&root.join(README_PATH), "runtime documentation change\n");
        assert_eq!(
            auth_crlf,
            run_image_id(root),
            "execution-only documentation must not affect the image ID"
        );

        write(&auth_path, "# different build configuration\n");
        assert_ne!(
            auth_crlf,
            run_image_id(root),
            "changed auth-hook build configuration must affect the image ID"
        );
    }

    #[test]
    fn shell_driver_avoids_macos_incompatible_constructs() {
        assert!(!SHELL_DRIVER.contains("sort -V"));
        assert!(!SHELL_DRIVER.contains("[[ -v"));
        assert!(SHELL_DRIVER.contains("version_at_least"));
        assert!(SHELL_DRIVER.contains("if command -v sha256sum"));
        assert!(SHELL_DRIVER.contains("shasum -a 256"));
        assert!(SHELL_DRIVER.contains("declare -p"));
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
    fn no_transparent_just_shim_is_emitted() {
        for body in [RECIPE, SHELL_DRIVER, POWERSHELL_DRIVER, README] {
            assert!(!body.contains("activate.ps1"));
            assert!(!body.contains("find-real-just"));
        }
    }

    #[test]
    fn auth_helpers_use_the_standard_paths() {
        match auth_shell("# shell auth\n") {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, AUTH_SHELL_PATH);
                assert_eq!(spec.body, "# shell auth\n");
            }
            Artifact::Region(_) => panic!("auth hook must be an owned file"),
        }
        match auth_powershell("# PowerShell auth\n") {
            Artifact::OwnedFile(spec) => {
                assert_eq!(spec.path, AUTH_POWERSHELL_PATH);
                assert_eq!(spec.body, "# PowerShell auth\n");
            }
            Artifact::Region(_) => panic!("auth hook must be an owned file"),
        }
    }
}
