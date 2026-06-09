// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local recipe emission (the `justfiles/ox-check/` tree).
//!
//! Each owned file under `justfiles/ox-check/` is embedded at compile time
//! via [`include_str!`] from the `templates/justfiles/ox-check/` directory.
//! The emitter just forwards the template through the owned-file driver.
//!
//! See [local.md](../../docs/design/local.md) for the recipe surface.

use std::path::Path;

use anyhow::Result;

use crate::manifest::Manifest;
use crate::plan::PlanItem;

use super::owned_file::plan_owned_file;

/// Contents of `justfiles/ox-check/tools.just` baked into the binary.
pub const TOOLS_JUST: &str =
    include_str!("../../templates/justfiles/ox-check/tools.just");

/// Repo-root-relative path of the tools recipe file.
pub const TOOLS_JUST_PATH: &str = "justfiles/ox-check/tools.just";

/// Contents of `justfiles/ox-check/checks.just` baked into the binary.
pub const CHECKS_JUST: &str =
    include_str!("../../templates/justfiles/ox-check/checks.just");

/// Repo-root-relative path of the per-check recipe file.
pub const CHECKS_JUST_PATH: &str = "justfiles/ox-check/checks.just";

/// Contents of `justfiles/ox-check/groups.just` baked into the binary.
pub const GROUPS_JUST: &str =
    include_str!("../../templates/justfiles/ox-check/groups.just");

/// Repo-root-relative path of the group recipe file.
pub const GROUPS_JUST_PATH: &str = "justfiles/ox-check/groups.just";

/// Contents of `justfiles/ox-check/tiers.just` baked into the binary.
pub const TIERS_JUST: &str =
    include_str!("../../templates/justfiles/ox-check/tiers.just");

/// Repo-root-relative path of the tier aggregator file.
pub const TIERS_JUST_PATH: &str = "justfiles/ox-check/tiers.just";

/// Emit a [`PlanItem`] for `justfiles/ox-check/tools.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_tools_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem> {
    plan_owned_file(repo_root, manifest, TOOLS_JUST_PATH, TOOLS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/checks.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_checks_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem> {
    plan_owned_file(repo_root, manifest, CHECKS_JUST_PATH, CHECKS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/groups.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_groups_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem> {
    plan_owned_file(repo_root, manifest, GROUPS_JUST_PATH, GROUPS_JUST)
}

/// Emit a [`PlanItem`] for `justfiles/ox-check/tiers.just`.
///
/// # Errors
///
/// Propagates I/O errors from [`plan_owned_file`].
pub fn plan_tiers_just(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem> {
    plan_owned_file(repo_root, manifest, TIERS_JUST_PATH, TIERS_JUST)
}

/// Region id for the imports block in the user's `Justfile`.
pub const JUSTFILE_REGION_ID: &str = "ox-check-imports";

/// Repo-root-relative path of the user's `Justfile`.
pub const JUSTFILE_PATH: &str = "Justfile";

/// Render the body of the Justfile imports region.
///
/// The four `import` lines plus the `ox-check` alias.
#[must_use]
pub fn render_justfile_imports() -> String {
    "import 'justfiles/ox-check/checks.just'\n\
     import 'justfiles/ox-check/groups.just'\n\
     import 'justfiles/ox-check/tiers.just'\n\
     import 'justfiles/ox-check/tools.just'\n\
     alias ox-check := ox-check-pr\n"
        .to_owned()
}

/// Emit a [`PlanItem`] for the `Justfile` imports region.
///
/// # Errors
///
/// Propagates I/O and region-parsing errors.
pub fn plan_justfile_imports(repo_root: &Path, manifest: &Manifest) -> Result<PlanItem> {
    let body = render_justfile_imports();
    super::managed_region::plan_managed_region(
        repo_root,
        manifest,
        JUSTFILE_PATH,
        JUSTFILE_REGION_ID,
        &body,
        crate::region::CommentSyntax::Hash,
    )
}

/// Plan all four files of the `justfiles/ox-check/` tree.
///
/// # Errors
///
/// Propagates I/O errors from any per-file emitter.
pub fn plan_local_just_tree(
    repo_root: &Path,
    manifest: &Manifest,
) -> Result<Vec<PlanItem>> {
    Ok(vec![
        plan_tools_just(repo_root, manifest)?,
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
            assert!(
                CHECKS_JUST.contains(needle),
                "checks.just missing recipe '{needle}'"
            );
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
            "ox-check-nightly-test:",
            "ox-check-nightly-advisories:",
            "ox-check-nightly-runtime:",
            "ox-check-nightly-exhaustive:",
        ] {
            assert!(GROUPS_JUST.contains(needle), "groups.just missing '{needle}'");
        }
    }

    #[test]
    fn tiers_just_template_has_three_tiers() {
        for needle in ["ox-check-pr:", "ox-check-nightly:", "ox-check-full:"] {
            assert!(TIERS_JUST.contains(needle), "tiers.just missing '{needle}'");
        }
    }

    #[test]
    fn plan_local_just_tree_emits_four_items() {
        let tmp = TempDir::new().unwrap();
        let items = plan_local_just_tree(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(items.len(), 4);
        for item in &items {
            assert_eq!(item.decision, Decision::Write);
        }
    }

    #[test]
    fn justfile_imports_renders_expected_lines() {
        let body = render_justfile_imports();
        assert!(body.contains("import 'justfiles/ox-check/checks.just'"));
        assert!(body.contains("import 'justfiles/ox-check/groups.just'"));
        assert!(body.contains("import 'justfiles/ox-check/tiers.just'"));
        assert!(body.contains("import 'justfiles/ox-check/tools.just'"));
        assert!(body.contains("alias ox-check := ox-check-pr"));
    }

    #[test]
    fn justfile_imports_writes_into_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let item = plan_justfile_imports(tmp.path(), &Manifest::default()).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("# >>> ox-check-managed: ox-check-imports"));
        assert!(spliced.contains("alias ox-check := ox-check-pr"));
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
}
