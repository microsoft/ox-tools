// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `justfiles/anvil/` recipe tree and the `Justfile` imports region.
//!
//! Holds the embedded templates for the owned `.just` files and the registry
//! functions that wrap them as [`Artifact`]s. The engine resolves the on-disk
//! casing of every path generically, so `Justfile` / `justfile` need no
//! special handling here.
//!
//! See [`local.md`](../../../docs/design/local.md) for the recipe surface.

use crate::catalog::Artifact;

/// Contents of `justfiles/anvil/mod.just` baked into the binary.
///
/// This is the single-import entry point: it pulls in the sibling recipe
/// files and defines `alias anvil := anvil-pr`.
const MOD_JUST: &str = include_str!("../../../templates/justfiles/anvil/mod.just");

/// Repo-root-relative path of the entry-point recipe file.
const MOD_JUST_PATH: &str = "justfiles/anvil/mod.just";

/// Contents of `justfiles/anvil/versions.just` baked into the binary.
const VERSIONS_JUST: &str = include_str!("../../../templates/justfiles/anvil/versions.just");

/// Repo-root-relative path of the pinned-versions recipe file.
const VERSIONS_JUST_PATH: &str = "justfiles/anvil/versions.just";

/// Contents of `justfiles/anvil/tools.just` baked into the binary.
const TOOLS_JUST: &str = include_str!("../../../templates/justfiles/anvil/tools.just");

/// Repo-root-relative path of the tools recipe file.
const TOOLS_JUST_PATH: &str = "justfiles/anvil/tools.just";

/// Contents of `justfiles/anvil/helpers.just` baked into the binary.
///
/// Holds the shared helper recipes (`_anvil-base-ref`,
/// `_anvil-impact-format`) and the impact env-var contract that the
/// per-check recipes rely on.
const HELPERS_JUST: &str = include_str!("../../../templates/justfiles/anvil/helpers.just");

/// Repo-root-relative path of the shared-helpers recipe file.
const HELPERS_JUST_PATH: &str = "justfiles/anvil/helpers.just";

/// Emits `(path, include_str!)` pairs for a set of split recipe files that
/// live under a subdirectory of `justfiles/anvil/`. Each file is one owned
/// artifact, so the recipe tree is one file per check / per group rather
/// than a single monolithic `checks.just` / `groups.just`.
macro_rules! split_recipe_files {
    ($subdir:literal, [$($name:literal),* $(,)?]) => {
        &[$(
            (
                concat!("justfiles/anvil/", $subdir, "/", $name, ".just"),
                include_str!(concat!("../../../templates/justfiles/anvil/", $subdir, "/", $name, ".just")),
            ),
        )*]
    };
}

/// One `justfiles/anvil/checks/<check>.just` file per catalog check
/// (the check recipe plus its paired `*-setup` / `*-validate-prereqs`).
const CHECK_FILES: &[(&str, &str)] = split_recipe_files!(
    "checks",
    [
        "aprz",
        "audit",
        "bench",
        "bolero",
        "careful",
        "cargo-hack",
        "cargo-sort",
        "clippy",
        "deny",
        "doc-build",
        "doc-test",
        "ensure-no-cyclic-deps",
        "ensure-no-default-features",
        "examples",
        "external-types",
        "fmt",
        "license-headers",
        "llvm-cov",
        "loom",
        "miri",
        "miri-race-coverage",
        "miri-strict-provenance",
        "miri-tree-borrows",
        "mutants-diff",
        "mutants-full",
        "pr-title",
        "readme-check",
        "semver-check",
        "spellcheck",
        "udeps",
    ]
);

/// One `justfiles/anvil/groups/<group>.just` file per group (the group
/// recipe plus its paired `*-setup` / `*-validate-prereqs`).
const GROUP_FILES: &[(&str, &str)] = split_recipe_files!(
    "groups",
    [
        "pr-fast",
        "pr-slow",
        "pr-test",
        "pr-runtime-analysis",
        "pr-mutants",
        "scheduled-test",
        "scheduled-advisories",
        "scheduled-runtime-analysis",
        "scheduled-exhaustive",
    ]
);

/// Contents of `justfiles/anvil/tiers.just` baked into the binary.
const TIERS_JUST: &str = include_str!("../../../templates/justfiles/anvil/tiers.just");

/// Repo-root-relative path of the tier aggregator file.
const TIERS_JUST_PATH: &str = "justfiles/anvil/tiers.just";

/// Embedded body of the `anvil-imports` region in the user's Justfile.
pub(crate) const JUSTFILE_IMPORTS_BODY: &str = include_str!("../../../templates/regions/justfile-imports.just");

/// Region id for the imports block in the user's `Justfile`.
pub(crate) const JUSTFILE_REGION_ID: &str = "anvil-imports";

/// Canonical repo-root-relative path of the user's `Justfile`.
///
/// Capitalized to match the dominant Unix convention for repo-root
/// build-config files (`Makefile`, `Dockerfile`, `Rakefile`, ...). `just`
/// accepts either case, and the engine reuses whatever casing a repo already
/// has on disk.
pub(crate) const JUSTFILE_PATH: &str = "Justfile";

/// `justfiles/anvil/mod.just` — the single-import entry point.
#[must_use]
pub fn entry() -> Artifact {
    Artifact::owned_file(MOD_JUST_PATH, MOD_JUST)
}

/// `justfiles/anvil/versions.just` — pinned toolchain versions.
#[must_use]
pub fn versions() -> Artifact {
    Artifact::owned_file(VERSIONS_JUST_PATH, VERSIONS_JUST)
}

/// `justfiles/anvil/tools.just` — tool install / prereq recipes.
#[must_use]
pub fn tools() -> Artifact {
    Artifact::owned_file(TOOLS_JUST_PATH, TOOLS_JUST)
}

/// `justfiles/anvil/helpers.just` — shared helper recipes + impact contract.
#[must_use]
pub fn helpers() -> Artifact {
    Artifact::owned_file(HELPERS_JUST_PATH, HELPERS_JUST)
}

/// The `justfiles/anvil/checks/<check>.just` files — one owned artifact
/// per catalog check.
#[must_use]
pub fn check_files() -> Vec<Artifact> {
    CHECK_FILES.iter().map(|&(path, body)| Artifact::owned_file(path, body)).collect()
}

/// The `justfiles/anvil/groups/<group>.just` files — one owned artifact
/// per group.
#[must_use]
pub fn group_files() -> Vec<Artifact> {
    GROUP_FILES.iter().map(|&(path, body)| Artifact::owned_file(path, body)).collect()
}

/// `justfiles/anvil/tiers.just` — the tier aggregators.
#[must_use]
pub fn tiers() -> Artifact {
    Artifact::owned_file(TIERS_JUST_PATH, TIERS_JUST)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tools_just_template_is_not_empty() {
        assert!(TOOLS_JUST.contains("anvil-system-deps-check"));
        assert!(TOOLS_JUST.contains("anvil-tool-cargo-deny-install"));
        assert!(TOOLS_JUST.contains("anvil-tool-cargo-deny-validate-prereqs"));
        assert!(TOOLS_JUST.contains("anvil-component-default-clippy-install"));
        assert!(TOOLS_JUST.contains("anvil-toolchain-nightly-install"));
    }

    /// All `checks/*.just` bodies concatenated, for content assertions.
    fn all_check_bodies() -> String {
        CHECK_FILES.iter().map(|(_, b)| *b).collect::<Vec<_>>().join("\n")
    }

    /// All `groups/*.just` bodies concatenated, for content assertions.
    fn all_group_bodies() -> String {
        GROUP_FILES.iter().map(|(_, b)| *b).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn checks_just_template_includes_all_catalog_checks() {
        let checks = all_check_bodies();
        for needle in [
            "anvil-fmt:",
            "anvil-clippy:",
            "anvil-license-headers:",
            "anvil-pr-title:",
            "anvil-llvm-cov:",
            "anvil-doc-test:",
            "anvil-mutants-diff:",
            "anvil-miri:",
            "anvil-mutants-full:",
            "anvil-bench:",
        ] {
            assert!(checks.contains(needle), "checks tree missing recipe '{needle}'");
        }
    }

    #[test]
    fn each_check_file_defines_its_own_check_recipe() {
        // The file `checks/<name>.just` must define `anvil-<name>:` -- guards
        // against a mis-split that files a check's recipe under the wrong name.
        for (path, body) in CHECK_FILES {
            let stem = path
                .strip_prefix("justfiles/anvil/checks/")
                .and_then(|p| p.strip_suffix(".just"))
                .expect("check file path has the expected shape");
            let needle = format!("anvil-{stem}:");
            assert!(body.contains(&needle), "{path} must define '{needle}'");
        }
    }

    #[test]
    fn groups_just_template_includes_all_groups_and_pr_slow_sub_recipes() {
        let groups = all_group_bodies();
        for needle in [
            "anvil-pr-fast:",
            "anvil-pr-slow:",
            "anvil-pr-test:",
            "anvil-pr-runtime-analysis:",
            "anvil-pr-mutants:",
            "anvil-scheduled-test:",
            "anvil-scheduled-advisories:",
            "anvil-scheduled-exhaustive:",
        ] {
            assert!(groups.contains(needle), "groups tree missing '{needle}'");
        }
        for needle in ["anvil-pr-slow1:", "anvil-pr-slow2:", "anvil-pr-slow3:"] {
            assert!(!groups.contains(needle), "groups tree still contains stale '{needle}'");
        }
        assert!(groups.contains("anvil-pr-slow: anvil-pr-slow-validate-prereqs anvil-pr-test anvil-pr-runtime-analysis anvil-pr-mutants"));
        // Every group recipe lists its own validate-prereqs aggregate first so
        // all tool checks run up front (just dedups the per-check ones).
        for needle in [
            "anvil-pr-fast: anvil-pr-fast-validate-prereqs",
            "anvil-pr-test: anvil-pr-test-validate-prereqs",
            "anvil-pr-runtime-analysis: anvil-pr-runtime-analysis-validate-prereqs",
            "anvil-pr-mutants: anvil-pr-mutants-validate-prereqs",
            "anvil-scheduled-test: anvil-scheduled-test-validate-prereqs",
            "anvil-scheduled-advisories: anvil-scheduled-advisories-validate-prereqs",
            "anvil-scheduled-runtime-analysis: anvil-scheduled-runtime-analysis-validate-prereqs",
            "anvil-scheduled-exhaustive: anvil-scheduled-exhaustive-validate-prereqs",
        ] {
            assert!(
                groups.contains(needle),
                "group recipe must run its validate-prereqs first: '{needle}'"
            );
        }
    }

    #[test]
    fn tiers_just_template_has_three_tiers() {
        for needle in ["anvil-pr:", "anvil-scheduled:", "anvil-full:"] {
            assert!(TIERS_JUST.contains(needle), "tiers.just missing '{needle}'");
        }
        // Each tier runs its validate-prereqs aggregate first so a missing
        // tool fails up front rather than mid-run.
        for needle in [
            "anvil-pr: anvil-pr-validate-prereqs",
            "anvil-scheduled: anvil-scheduled-validate-prereqs",
            "anvil-full: anvil-full-validate-prereqs",
        ] {
            assert!(
                TIERS_JUST.contains(needle),
                "tier recipe must run its validate-prereqs first: '{needle}'"
            );
        }
        // The scheduled tier must fan out to every scheduled group, including
        // runtime-analysis (a separate group from exhaustive).
        for needle in [
            "anvil-scheduled-test",
            "anvil-scheduled-advisories",
            "anvil-scheduled-runtime-analysis",
            "anvil-scheduled-exhaustive",
        ] {
            assert!(TIERS_JUST.contains(needle), "scheduled tier must reference group '{needle}'");
        }
        // The catch-all `anvil-setup` / `anvil-validate-prereqs` must also
        // install / verify cargo-delta (the impact tool), so a local
        // `just anvil-setup` provisions a complete environment even though
        // cargo-delta isn't wired into any per-group setup.
        assert!(
            TIERS_JUST
                .contains("anvil-setup installer=\"install\": (anvil-full-setup installer) (anvil-tool-cargo-delta-install installer)"),
            "anvil-setup must install cargo-delta"
        );
        assert!(
            TIERS_JUST.contains("anvil-validate-prereqs: anvil-full-validate-prereqs anvil-tool-cargo-delta-validate-prereqs"),
            "anvil-validate-prereqs must verify cargo-delta"
        );
    }

    #[test]
    fn versions_just_has_known_tools() {
        for needle in [
            "cargo_nextest_version",
            "cargo_llvm_cov_version",
            "cargo_deny_version",
            "cargo_mutants_version",
        ] {
            assert!(VERSIONS_JUST.contains(needle), "versions.just missing variable '{needle}'");
        }
    }

    #[test]
    fn mod_just_imports_siblings_and_defines_alias() {
        for needle in [
            "import 'helpers.just'",
            "import 'checks/fmt.just'",
            "import 'checks/miri.just'",
            "import 'container/container.just'",
            "import 'groups/pr-fast.just'",
            "import 'groups/scheduled-exhaustive.just'",
            "import 'tiers.just'",
            "import 'tools.just'",
            "import 'versions.just'",
            "alias anvil := anvil-pr",
        ] {
            assert!(MOD_JUST.contains(needle), "mod.just missing '{needle}'");
        }
        // Every split recipe file must be imported by mod.just.
        for (path, _) in CHECK_FILES.iter().chain(GROUP_FILES.iter()) {
            let import = format!(
                "import '{}'",
                path.strip_prefix("justfiles/anvil/").expect("path under justfiles/anvil/")
            );
            assert!(MOD_JUST.contains(&import), "mod.just missing '{import}'");
        }
    }

    #[test]
    fn versions_just_defines_both_nightly_pins() {
        assert!(VERSIONS_JUST.contains("rust_nightly :="), "versions.just missing rust_nightly");
        assert!(
            VERSIONS_JUST.contains("rust_nightly_external_types :="),
            "versions.just missing rust_nightly_external_types"
        );
    }

    #[test]
    fn checks_just_has_no_floating_nightly_invocations() {
        let checks = all_check_bodies();
        for line in checks.lines() {
            let stripped = line.split('#').next().unwrap_or("");
            assert!(
                !stripped.contains("+nightly "),
                "checks tree has a floating '+nightly' invocation: {line}"
            );
            assert!(
                !stripped.contains("'+nightly'"),
                "checks tree has a floating '+nightly' invocation: {line}"
            );
        }
    }

    #[test]
    fn justfile_imports_body_is_a_single_import_line() {
        let body = JUSTFILE_IMPORTS_BODY.trim();
        assert_eq!(body, "import 'justfiles/anvil/mod.just'");
    }
}
