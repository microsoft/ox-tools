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
const IMAGE_ID: &str = include_str!("../../../templates/justfiles/anvil/container/image-id.ps1");
const SHELL_DRIVER: &str = include_str!("../../../templates/justfiles/anvil/container/run-in-container.sh");
const POWERSHELL_DRIVER: &str = include_str!("../../../templates/justfiles/anvil/container/run-in-container.ps1");
const README: &str = include_str!("../../../templates/justfiles/anvil/container/README.md");

const RECIPE_PATH: &str = "justfiles/anvil/container/container.just";
const CONTAINERFILE_PATH: &str = "justfiles/anvil/container/Containerfile";
const IGNORE_PATH: &str = "justfiles/anvil/container/container.ignore";
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
    use super::*;

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
    }

    #[test]
    fn drivers_use_podman_and_content_addressing() {
        for driver in [SHELL_DRIVER, POWERSHELL_DRIVER] {
            assert!(driver.contains("podman"));
            assert!(driver.contains("image-id.ps1"));
            assert!(driver.contains("ANVIL_CONTAINER_NO_REBUILD"));
            assert!(driver.contains("ANVIL_CONTAINER_IMAGE"));
            assert!(driver.contains("ANVIL_IN_CONTAINER"));
            assert!(driver.contains("ANVIL_CONTAINER_FORWARD_GITHUB_TOKEN"));
            assert!(driver.contains("PR_TITLE"));
            assert!(driver.contains("--pull=never"));
        }
        assert!(POWERSHELL_DRIVER.contains("AnvilContainerBuildInMachine"));
        assert!(POWERSHELL_DRIVER.contains("AnvilContainerPrepareCommand"));
        assert!(POWERSHELL_DRIVER.contains("podman machine ssh"));
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
            Artifact::OwnedFile(spec) => assert_eq!(spec.path, AUTH_SHELL_PATH),
            Artifact::Region(_) => panic!("auth hook must be an owned file"),
        }
        match auth_powershell("# PowerShell auth\n") {
            Artifact::OwnedFile(spec) => assert_eq!(spec.path, AUTH_POWERSHELL_PATH),
            Artifact::Region(_) => panic!("auth hook must be an owned file"),
        }
    }
}
