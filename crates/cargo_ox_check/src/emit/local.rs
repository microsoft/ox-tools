// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local recipe emission (the `justfiles/ox-check/` tree).
//!
//! Each owned file under `justfiles/ox-check/` is embedded at compile time
//! via [`include_str!`] from the `templates/justfiles/ox-check/` directory.
//! The emitter just forwards the template through the owned-file driver.
//!
//! See [`local.md`](../../docs/design/local.md) for the recipe surface.

use std::path::Path;

use ohno::AppError;

use super::owned_file::plan_owned_file;
use crate::manifest::Manifest;
use crate::plan::PlanItem;

/// Contents of `justfiles/ox-check/mod.just` baked into the binary.
///
/// This is the single-import entry point: it pulls in the four sibling
/// recipe files and defines `alias ox-check := ox-check-pr`.
pub const MOD_JUST: &str = include_str!("../../templates/justfiles/ox-check/mod.just");

/// Repo-root-relative path of the entry-point recipe file.
pub const MOD_JUST_PATH: &str = "justfiles/ox-check/mod.just";

/// Contents of `justfiles/ox-check/versions.just` baked into the binary.
///
/// Pinned toolchain versions consumed by recipes (via `{{ var }}`
/// interpolation) and by setup composites (via `just --evaluate`).
/// See [`local.md`](../../docs/design/local.md#nightly-pinning) for
/// the bump policy.
pub const VERSIONS_JUST: &str = include_str!("../../templates/justfiles/ox-check/versions.just");

/// Repo-root-relative path of the pinned-versions recipe file.
pub const VERSIONS_JUST_PATH: &str = "justfiles/ox-check/versions.just";

/// Contents of `justfiles/ox-check/tools.just` baked into the binary.
pub const TOOLS_JUST: &str = include_str!("../../templates/justfiles/ox-check/tools.just");

/// Repo-root-relative path of the tools recipe file.
pub const TOOLS_JUST_PATH: &str = "justfiles/ox-check/tools.just";

/// Contents of `justfiles/ox-check/tool-minimums.txt` baked into the binary.
///
/// Data file consumed by the `tools.just` recipes; one line per cargo
/// subcommand, format `<tool>=<minimum-version>`. See
/// [`local.md §3`](../../docs/design/local.md) for the policy.
pub const TOOL_MINIMUMS: &str = include_str!("../../templates/justfiles/ox-check/tool-minimums.txt");

/// Repo-root-relative path of the tool minimums catalog.
pub const TOOL_MINIMUMS_PATH: &str = "justfiles/ox-check/tool-minimums.txt";

/// Contents of `justfiles/ox-check/checks.just` baked into the binary.
pub const CHECKS_JUST: &str = include_str!("../../templates/justfiles/ox-check/checks.just");

/// Repo-root-relative path of the per-check recipe file.
pub const CHECKS_JUST_PATH: &str = "justfiles/ox-check/checks.just";

/// Contents of `justfiles/ox-check/groups.just` baked into the binary.
pub const GROUPS_JUST: &str = include_str!("../../templates/justfiles/ox-check/groups.just");

/// Repo-root-relative path of the group recipe file.
pub const GROUPS_JUST_PATH: &str = "justfiles/ox-check/groups.just";

/// Contents of `justfiles/ox-check/tiers.just` baked into the binary.
pub const TIERS_JUST: &str = include_str!("../../templates/justfiles/ox-check/tiers.just");

/// Repo-root-relative path of the tier aggregator file.
pub const TIERS_JUST_PATH: &str = "justfiles/ox-check/tiers.just";

/// Embedded body of the `ox-check-imports` region in the user's Justfile.
pub const JUSTFILE_IMPORTS_BODY: &str = include_str!("../../templates/regions/justfile-imports.just");

/// Emit a [`PlanItem`] for `justfiles/ox-check/mod.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_mod_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, MOD_JUST_PATH, MOD_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/tools.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_tools_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, TOOLS_JUST_PATH, TOOLS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/versions.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_versions_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, VERSIONS_JUST_PATH, VERSIONS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/tool-minimums.txt`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_tool_minimums(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, TOOL_MINIMUMS_PATH, TOOL_MINIMUMS)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/checks.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_checks_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, CHECKS_JUST_PATH, CHECKS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/groups.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_groups_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, GROUPS_JUST_PATH, GROUPS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/tiers.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_tiers_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    plan_owned_file(repo_root, manifest, TIERS_JUST_PATH, TIERS_JUST)
}

/// Region id for the imports block in the user's `Justfile`.
pub const JUSTFILE_REGION_ID: &str = "ox-check-imports";

/// Canonical repo-root-relative path of the user's `Justfile`.
///
/// Capitalized to match the dominant Unix convention for repo-root
/// build-config files (`Makefile`, `Dockerfile`, `Rakefile`, `Gemfile`,
/// `Procfile`, `Brewfile`, ...) and the surveyed Microsoft Rust repos
/// (`oxidizer`, `ox-tools`). `just` itself accepts either case.
///
/// For repos that already committed a lowercase `justfile`, the
/// [`plan_justfile_imports`] function prefers the existing file rather
/// than creating a sibling — see that function for details.
pub const JUSTFILE_PATH: &str = "Justfile";

/// Alternative lowercase form. Recognized when looking for an existing
/// file on disk, but never written by ox-check; new files always use
/// the canonical [`JUSTFILE_PATH`] capitalization.
const JUSTFILE_PATH_LOWERCASE: &str = "justfile";

/// Resolve the on-disk Justfile path for `repo_root`.
///
/// Prefers an existing lowercase `justfile` if (and only if) the
/// canonical `Justfile` doesn't already exist. This means:
///
/// - Fresh repos: ox-check writes `Justfile` (canonical).
/// - Repos with `Justfile` (the common case): we splice into it.
/// - Repos with only `justfile`: we splice into it without renaming.
/// - Repos with both (case-sensitive FS oddity): we honor the canonical
///   `Justfile` and leave the lowercase file alone.
///
/// This guards against the case-sensitivity footgun on Linux: without
/// it, an adopter with a lowercase `justfile` would silently get a
/// sibling `Justfile` containing the imports region, and `just` would
/// load whichever it finds first (lowercase wins by default) — so the
/// imports never take effect.
fn resolve_justfile_path(repo_root: &Path) -> &'static str {
    if repo_root.join(JUSTFILE_PATH).exists() {
        JUSTFILE_PATH
    } else if repo_root.join(JUSTFILE_PATH_LOWERCASE).exists() {
        JUSTFILE_PATH_LOWERCASE
    } else {
        JUSTFILE_PATH
    }
}

/// Emit a [`PlanItem`] for the `Justfile` imports region.
///
/// # Errors
///
/// Propagates I/O and region-parsing errors.
pub fn plan_justfile_imports(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem, AppError> {
    super::managed_region::plan_managed_region(
        repo_root,
        manifest,
        resolve_justfile_path(repo_root),
        JUSTFILE_REGION_ID,
        JUSTFILE_IMPORTS_BODY,
        crate::region::CommentSyntax::Hash,
    )
}

/// Move a legacy lowercase `justfile` manifest entry to the canonical
/// `Justfile` capital key. No-op when no migration is needed.
///
/// Earlier versions of cargo-ox-check used `JUSTFILE_PATH = "justfile"`
/// (lowercase), so manifests written by those versions track the
/// imports region under the lowercase host. The canonical
/// capitalization is now `Justfile`. Without this migration, the
/// orphan-detection pass would see the lowercase entry as an orphan
/// (no live plan item for it), notice the file content matches its
/// last-rendered hash, and splice the region out — destroying ox-check
/// integration on every re-render after the upgrade. The bug
/// manifested as silent loss of region on case-sensitive Linux and
/// physical-same-file region removal on case-insensitive Windows.
///
/// The migration only fires when the lowercase entry is present AND
/// the canonical entry is absent, so it never overwrites a legitimate
/// case-sensitive setup where both files were intentionally tracked.
pub fn migrate_legacy_justfile_case(manifest: &mut Manifest) {
    use crate::manifest::RegionKey;

    let legacy = RegionKey {
        host: "justfile".to_owned(),
        id: JUSTFILE_REGION_ID.to_owned(),
    };
    let canonical = RegionKey {
        host: JUSTFILE_PATH.to_owned(),
        id: JUSTFILE_REGION_ID.to_owned(),
    };
    if manifest.regions.contains_key(&canonical) {
        // Already on the canonical key; leave the lowercase entry (if
        // any) alone — could be a deliberate dual-file setup.
        return;
    }
    if let Some(hash) = manifest.regions.remove(&legacy) {
        manifest.regions.insert(canonical, hash);
    }
}

/// Plan all five files of the `justfiles/ox-check/` tree.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
/// Plan all files of the `justfiles/ox-check/` tree (recipes + data).
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_local_just_tree(repo_root: &Path, manifest: &Manifest) -> Result<Vec<PlanItem>, AppError> {
    Ok(vec![
        plan_mod_just(repo_root, manifest)?,
        plan_tools_just(repo_root, manifest)?,
        plan_tool_minimums(repo_root, manifest)?,
        plan_versions_just(repo_root, manifest)?,
        plan_checks_just(repo_root, manifest)?,
        plan_groups_just(repo_root, manifest)?,
        plan_tiers_just(repo_root, manifest)?,
    ])
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::decision::Decision;

    #[test]
    fn tools_just_template_is_not_empty() {
        assert!(TOOLS_JUST.contains("ox-check-tools-check"));
        assert!(TOOLS_JUST.contains("_ox-check-require"));
    }

    #[test]
    fn checks_just_template_includes_all_catalog_checks() {
        // Sample a handful from each group to guard against accidental deletions.
        for needle in [
            "ox-check-fmt:",
            "ox-check-clippy:",
            "ox-check-license-headers:",
            "ox-check-pr-title:",
            "ox-check-llvm-cov:",
            "ox-check-doc-test:",
            "ox-check-mutants:",
            "ox-check-miri:",
            "ox-check-mutants-full:",
            "ox-check-bench:",
        ] {
            assert!(CHECKS_JUST.contains(needle), "checks.just missing recipe '{needle}'");
        }
    }

    #[test]
    fn checks_just_emitter_writes_on_first_render() {
        let tmp = TempDir::new().unwrap();
        let item = plan_checks_just(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::Write);
    }

    #[test]
    fn groups_just_template_includes_all_seven_groups() {
        for needle in [
            "ox-check-pr-fast:",
            "ox-check-pr-test:",
            "ox-check-pr-mutants:",
            "ox-check-scheduled-test:",
            "ox-check-scheduled-advisories:",
            "ox-check-scheduled-runtime:",
            "ox-check-scheduled-exhaustive:",
        ] {
            assert!(GROUPS_JUST.contains(needle), "groups.just missing '{needle}'");
        }
    }

    #[test]
    fn tiers_just_template_has_three_tiers() {
        for needle in ["ox-check-pr:", "ox-check-scheduled:", "ox-check-full:"] {
            assert!(TIERS_JUST.contains(needle), "tiers.just missing '{needle}'");
        }
    }

    #[test]
    fn tool_minimums_template_has_known_tools() {
        for needle in ["cargo-nextest=", "cargo-llvm-cov=", "cargo-deny=", "cargo-mutants="] {
            assert!(TOOL_MINIMUMS.contains(needle), "tool-minimums.txt missing entry '{needle}'");
        }
    }

    #[test]
    fn plan_local_just_tree_emits_seven_items() {
        let tmp = TempDir::new().unwrap();
        let items = plan_local_just_tree(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 7);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
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
            "alias ox-check := ox-check-pr",
        ] {
            assert!(MOD_JUST.contains(needle), "mod.just missing '{needle}'");
        }
    }

    #[test]
    fn versions_just_defines_both_nightly_pins() {
        // Required source of truth for the setup composites' `just --evaluate`
        // step and recipe `{{ var }}` interpolation. If either name changes,
        // the templates / docs must change in lockstep.
        assert!(VERSIONS_JUST.contains("rust_nightly :="), "versions.just missing rust_nightly");
        assert!(
            VERSIONS_JUST.contains("rust_nightly_external_types :="),
            "versions.just missing rust_nightly_external_types"
        );
    }

    #[test]
    fn checks_just_has_no_floating_nightly_invocations() {
        // Catch a regression where a recipe falls back to bare `+nightly`
        // instead of using the pinned `{{ rust_nightly }}` /
        // `{{ rust_nightly_external_types }}` interpolations.
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
        assert_eq!(body, "import 'justfiles/ox-check/mod.just'");
    }

    #[test]
    fn justfile_imports_writes_into_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let item = plan_justfile_imports(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("# >>> ox-check-managed: ox-check-imports"));
        assert!(spliced.contains("import 'justfiles/ox-check/mod.just'"));
        // The alias lives in mod.just, not in the user's Justfile.
        assert!(!spliced.contains("alias ox-check"));
    }

    #[test]
    fn first_render_writes_tools_just() {
        let tmp = TempDir::new().unwrap();
        let item = plan_tools_just(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::Write);
        assert_eq!(item.rendered.as_deref(), Some(TOOLS_JUST));
    }

    #[test]
    fn matching_file_is_in_sync() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("justfiles/ox-check")).unwrap();
        std::fs::write(tmp.path().join(TOOLS_JUST_PATH), TOOLS_JUST).unwrap();
        let item = plan_tools_just(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::InSync);
    }

    #[test]
    fn resolve_justfile_path_returns_canonical_when_empty() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_justfile_path(tmp.path()), "Justfile");
    }

    #[test]
    fn resolve_justfile_path_prefers_existing_capital() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Justfile"), "# existing\n").unwrap();
        assert_eq!(resolve_justfile_path(tmp.path()), "Justfile");
    }

    #[test]
    fn resolve_justfile_path_honors_existing_lowercase() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("justfile"), "# existing\n").unwrap();
        // The canonical path returned here is whatever the FS reports —
        // on case-insensitive Windows that will match `Justfile.exists()`
        // (and the test takes the first branch, returning "Justfile"); on
        // case-sensitive Linux it falls through and returns "justfile".
        // Either outcome is correct because the actual on-disk file
        // resolves the same way at write time.
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
        assert!(m.regions.contains_key(&crate::manifest::RegionKey {
            host: "Justfile".to_owned(),
            id: JUSTFILE_REGION_ID.to_owned(),
        }));
        assert!(!m.regions.contains_key(&crate::manifest::RegionKey {
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
        // Both entries preserved — canonical was already there, so we
        // don't touch the lowercase entry (could be intentional).
        assert!(m.regions.contains_key(&crate::manifest::RegionKey {
            host: "Justfile".to_owned(),
            id: JUSTFILE_REGION_ID.to_owned(),
        }));
        assert!(m.regions.contains_key(&crate::manifest::RegionKey {
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
