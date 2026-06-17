// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `justfiles/anvil/` recipe tree and the `Justfile` imports region.
//!
//! Holds the embedded templates for the owned `.just` files, the registry
//! functions that wrap them as [`Artifact`]s, and the `Justfile`-specific
//! reconciliation the engine consults at plan time (case resolution and the
//! one-time legacy lowercase-`justfile` manifest migration).
//!
//! See [`local.md`](../../../docs/design/local.md) for the recipe surface.

use std::path::Path;

use crate::catalog::Artifact;
use crate::manifest::{Manifest, RegionKey};

/// Contents of `justfiles/anvil/mod.just` baked into the binary.
///
/// This is the single-import entry point: it pulls in the sibling recipe
/// files and defines `alias anvil := anvil-pr`.
pub const MOD_JUST: &str = include_str!("../../../templates/justfiles/anvil/mod.just");

/// Repo-root-relative path of the entry-point recipe file.
pub const MOD_JUST_PATH: &str = "justfiles/anvil/mod.just";

/// Contents of `justfiles/anvil/versions.just` baked into the binary.
pub const VERSIONS_JUST: &str = include_str!("../../../templates/justfiles/anvil/versions.just");

/// Repo-root-relative path of the pinned-versions recipe file.
pub const VERSIONS_JUST_PATH: &str = "justfiles/anvil/versions.just";

/// Contents of `justfiles/anvil/tools.just` baked into the binary.
pub const TOOLS_JUST: &str = include_str!("../../../templates/justfiles/anvil/tools.just");

/// Repo-root-relative path of the tools recipe file.
pub const TOOLS_JUST_PATH: &str = "justfiles/anvil/tools.just";

/// Contents of `justfiles/anvil/checks.just` baked into the binary.
pub const CHECKS_JUST: &str = include_str!("../../../templates/justfiles/anvil/checks.just");

/// Repo-root-relative path of the per-check recipe file.
pub const CHECKS_JUST_PATH: &str = "justfiles/anvil/checks.just";

/// Contents of `justfiles/anvil/groups.just` baked into the binary.
pub const GROUPS_JUST: &str = include_str!("../../../templates/justfiles/anvil/groups.just");

/// Repo-root-relative path of the group recipe file.
pub const GROUPS_JUST_PATH: &str = "justfiles/anvil/groups.just";

/// Contents of `justfiles/anvil/tiers.just` baked into the binary.
pub const TIERS_JUST: &str = include_str!("../../../templates/justfiles/anvil/tiers.just");

/// Repo-root-relative path of the tier aggregator file.
pub const TIERS_JUST_PATH: &str = "justfiles/anvil/tiers.just";

/// Embedded body of the `anvil-imports` region in the user's Justfile.
pub const JUSTFILE_IMPORTS_BODY: &str = include_str!("../../../templates/regions/justfile-imports.just");

/// Region id for the imports block in the user's `Justfile`.
pub const JUSTFILE_REGION_ID: &str = "anvil-imports";

/// Canonical repo-root-relative path of the user's `Justfile`.
///
/// Capitalized to match the dominant Unix convention for repo-root
/// build-config files (`Makefile`, `Dockerfile`, `Rakefile`, ...). `just`
/// itself accepts either case.
pub const JUSTFILE_PATH: &str = "Justfile";

/// Alternative lowercase form. Recognized when looking for an existing file
/// on disk, but never written by anvil; new files always use the canonical
/// [`JUSTFILE_PATH`] capitalization.
const JUSTFILE_PATH_LOWERCASE: &str = "justfile";

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

/// Resolve the on-disk Justfile path for `repo_root`.
///
/// Prefers an existing lowercase `justfile` if (and only if) the canonical
/// `Justfile` doesn't already exist. This guards against the
/// case-sensitivity footgun on Linux, where an adopter with a lowercase
/// `justfile` would otherwise silently get a sibling `Justfile`.
pub(crate) fn resolve_justfile_path(repo_root: &Path) -> &'static str {
    if repo_root.join(JUSTFILE_PATH).exists() {
        JUSTFILE_PATH
    } else if repo_root.join(JUSTFILE_PATH_LOWERCASE).exists() {
        JUSTFILE_PATH_LOWERCASE
    } else {
        JUSTFILE_PATH
    }
}

/// Move a legacy lowercase `justfile` manifest entry to the canonical
/// `Justfile` capital key. No-op when no migration is needed.
///
/// Earlier versions of cargo-anvil tracked the imports region under the
/// lowercase host. Without this migration the orphan-detection pass would
/// see the lowercase entry as an orphan and splice the region out. The
/// migration only fires when the lowercase entry is present AND the
/// canonical entry is absent, so it never overwrites a legitimate
/// case-sensitive setup where both files were intentionally tracked.
pub(crate) fn migrate_legacy_justfile_case(manifest: &mut Manifest) {
    let legacy = RegionKey {
        host: "justfile".to_owned(),
        id: JUSTFILE_REGION_ID.to_owned(),
    };
    let canonical = RegionKey {
        host: JUSTFILE_PATH.to_owned(),
        id: JUSTFILE_REGION_ID.to_owned(),
    };
    if manifest.regions.contains_key(&canonical) {
        return;
    }
    if let Some(hash) = manifest.regions.remove(&legacy) {
        manifest.regions.insert(canonical, hash);
    }
}

#[cfg(test)]
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
        assert!(GROUPS_JUST.contains("anvil-pr-slow: anvil-pr-test anvil-pr-runtime-analysis anvil-pr-mutants"));
    }

    #[test]
    fn tiers_just_template_has_three_tiers() {
        for needle in ["anvil-pr:", "anvil-scheduled:", "anvil-full:"] {
            assert!(TIERS_JUST.contains(needle), "tiers.just missing '{needle}'");
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

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn resolve_justfile_path_returns_canonical_when_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(resolve_justfile_path(tmp.path()), "Justfile");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn resolve_justfile_path_prefers_existing_capital() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Justfile"), "# existing\n").unwrap();
        assert_eq!(resolve_justfile_path(tmp.path()), "Justfile");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn resolve_justfile_path_honors_existing_lowercase() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("justfile"), "# existing\n").unwrap();
        let resolved = resolve_justfile_path(tmp.path());
        assert!(
            resolved == "Justfile" || resolved == "justfile",
            "unexpected resolution: {resolved}"
        );
    }

    #[test]
    fn migrate_legacy_justfile_case_moves_lowercase_entry() {
        use crate::checksum::checksum_str;
        let mut m = Manifest::default();
        m.set_region("justfile", JUSTFILE_REGION_ID, checksum_str("body\n"));
        migrate_legacy_justfile_case(&mut m);
        assert!(m.regions.contains_key(&RegionKey {
            host: "Justfile".to_owned(),
            id: JUSTFILE_REGION_ID.to_owned(),
        }));
        assert!(!m.regions.contains_key(&RegionKey {
            host: "justfile".to_owned(),
            id: JUSTFILE_REGION_ID.to_owned(),
        }));
    }

    #[test]
    fn migrate_legacy_justfile_case_noop_when_canonical_present() {
        use crate::checksum::checksum_str;
        let mut m = Manifest::default();
        m.set_region("Justfile", JUSTFILE_REGION_ID, checksum_str("canonical\n"));
        m.set_region("justfile", JUSTFILE_REGION_ID, checksum_str("legacy\n"));
        migrate_legacy_justfile_case(&mut m);
        assert!(m.regions.contains_key(&RegionKey {
            host: "Justfile".to_owned(),
            id: JUSTFILE_REGION_ID.to_owned(),
        }));
        assert!(m.regions.contains_key(&RegionKey {
            host: "justfile".to_owned(),
            id: JUSTFILE_REGION_ID.to_owned(),
        }));
    }

    #[test]
    fn migrate_legacy_justfile_case_noop_when_neither_present() {
        let mut m = Manifest::default();
        migrate_legacy_justfile_case(&mut m);
        assert!(m.regions.is_empty());
    }
}
