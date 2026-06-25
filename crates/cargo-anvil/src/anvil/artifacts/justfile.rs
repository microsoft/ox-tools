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

/// Contents of `justfiles/anvil/checks.just` baked into the binary.
const CHECKS_JUST: &str = include_str!("../../../templates/justfiles/anvil/checks.just");

/// Repo-root-relative path of the per-check recipe file.
const CHECKS_JUST_PATH: &str = "justfiles/anvil/checks.just";

/// Contents of `justfiles/anvil/groups.just` baked into the binary.
const GROUPS_JUST: &str = include_str!("../../../templates/justfiles/anvil/groups.just");

/// Repo-root-relative path of the group recipe file.
const GROUPS_JUST_PATH: &str = "justfiles/anvil/groups.just";

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

/// `justfiles/anvil/checks.just` — the per-check recipes.
#[must_use]
pub fn checks() -> Artifact {
    Artifact::owned_file(CHECKS_JUST_PATH, CHECKS_JUST)
}

/// `justfiles/anvil/groups.just` — the group recipes.
#[must_use]
pub fn groups() -> Artifact {
    Artifact::owned_file(GROUPS_JUST_PATH, GROUPS_JUST)
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

    #[test]
    fn checks_just_template_includes_all_catalog_checks() {
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
            assert!(CHECKS_JUST.contains(needle), "checks.just missing recipe '{needle}'");
        }
    }

    #[test]
    fn groups_just_template_includes_all_groups_and_pr_slow_sub_recipes() {
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
            assert!(GROUPS_JUST.contains(needle), "groups.just missing '{needle}'");
        }
        for needle in ["anvil-pr-slow1:", "anvil-pr-slow2:", "anvil-pr-slow3:"] {
            assert!(!GROUPS_JUST.contains(needle), "groups.just still contains stale '{needle}'");
        }
        assert!(
            GROUPS_JUST.contains("anvil-pr-slow: anvil-pr-slow-validate-prereqs anvil-pr-test anvil-pr-runtime-analysis anvil-pr-mutants")
        );
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
                GROUPS_JUST.contains(needle),
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
            assert!(
                TIERS_JUST.contains(needle),
                "scheduled tier must reference group '{needle}'"
            );
        }
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
            "import 'checks.just'",
            "import 'groups.just'",
            "import 'tiers.just'",
            "import 'tools.just'",
            "import 'versions.just'",
            "alias anvil := anvil-pr",
        ] {
            assert!(MOD_JUST.contains(needle), "mod.just missing '{needle}'");
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
        for line in CHECKS_JUST.lines() {
            let stripped = line.split('#').next().unwrap_or("");
            assert!(
                !stripped.contains("+nightly "),
                "checks.just has a floating '+nightly' invocation: {line}"
            );
            assert!(
                !stripped.contains("'+nightly'"),
                "checks.just has a floating '+nightly' invocation: {line}"
            );
        }
    }

    #[test]
    fn justfile_imports_body_is_a_single_import_line() {
        let body = JUSTFILE_IMPORTS_BODY.trim();
        assert_eq!(body, "import 'justfiles/anvil/mod.just'");
    }
}
