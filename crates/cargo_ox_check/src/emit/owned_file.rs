// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driver for a single owned file.
//!
//! Given a repo-root-relative path and the rendered template, this module
//! reads the disk file (if any), consults the manifest, computes the
//! decision, and returns a [`PlanItem`] ready to be applied.

use std::path::Path;

use ohno::{AppError, IntoAppError as _};

use crate::checksum::checksum_str;
use crate::decision::{Decision, DecisionInputs, decide};
use crate::manifest::Manifest;
use crate::plan::{PlanItem, Target};

/// Compute the [`PlanItem`] for an owned file.
///
/// `relpath` is the repo-root-relative forward-slash path.
/// `rendered` is the byte-exact content the template would produce.
///
/// # Errors
///
/// Returns an error if the file exists but can't be read.
pub fn plan_owned_file(
    repo_root: &Path,
    manifest: &Manifest,
    relpath: &str,
    rendered: &str,
) -> Result<PlanItem, AppError> {
    let abs = repo_root.join(relpath);
    let on_disk = read_optional(&abs)?;
    let disk_checksum = on_disk.as_deref().map(checksum_str);
    let template_checksum = checksum_str(rendered);
    let last_rendered = manifest.files.get(relpath).map(String::as_str);

    let inputs = DecisionInputs {
        last_rendered,
        disk: disk_checksum.as_deref(),
        template: &template_checksum,
    };
    let decision = decide(&inputs);

    let target = Target::File {
        path: relpath.to_owned(),
    };
    let item = match decision {
        Decision::InSync => PlanItem::insync(target, template_checksum),
        Decision::LeaveAlone => PlanItem::noop(target, decision),
        Decision::Write => {
            PlanItem::write_file(relpath, rendered.to_owned(), template_checksum)
        }
        Decision::Propose => PlanItem::propose_file(relpath, rendered.to_owned(), template_checksum),
        Decision::Remove | Decision::OrphanedKept => {
            unreachable!("decide() never returns removal decisions; those come from plan_removals")
        }
    };

    Ok(item)
}

fn read_optional(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).into_app_err_with(|| format!("failed to read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn missing_file_writes() {
        let tmp = TempDir::new().unwrap();
        let item =
            plan_owned_file(tmp.path(), &Manifest::default(), "a.txt", "content\n").unwrap();
        assert_eq!(item.decision, Decision::Write);
    }

    #[test]
    fn matching_file_in_sync() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "content\n").unwrap();
        let item =
            plan_owned_file(tmp.path(), &Manifest::default(), "a.txt", "content\n").unwrap();
        assert_eq!(item.decision, Decision::InSync);
    }

    #[test]
    fn user_modified_after_render_proposes_when_template_changed() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "user-edited\n").unwrap();
        let mut manifest = Manifest::default();
        let old_template_checksum = checksum_str("old template\n");
        manifest.set_file("a.txt", &old_template_checksum);
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "new template\n").unwrap();
        assert_eq!(item.decision, Decision::Propose);
    }

    #[test]
    fn user_modified_template_unchanged_leaves_alone() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "user-edited\n").unwrap();
        let mut manifest = Manifest::default();
        let same_checksum = checksum_str("template\n");
        manifest.set_file("a.txt", &same_checksum);
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "template\n").unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[test]
    fn empty_file_opts_out_when_template_unchanged() {
        // After a previous render, user empties the file. Template hasn't
        // moved → LeaveAlone (silent, opt-out preserved).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        let mut manifest = Manifest::default();
        manifest.set_file("a.txt", checksum_str("template\n"));
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "template\n").unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[test]
    fn empty_file_with_changed_template_proposes() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        let mut manifest = Manifest::default();
        manifest.set_file("a.txt", checksum_str("old\n"));
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "new\n").unwrap();
        assert_eq!(item.decision, Decision::Propose);
    }

    #[test]
    fn whitespace_only_file_is_treated_as_user_divergence() {
        // No special-casing for whitespace any more — it's just user
        // content that diverges from the template. Steady-state with an
        // unchanged template is still LeaveAlone, so opt-out works.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "   \n\t\n").unwrap();
        let mut manifest = Manifest::default();
        manifest.set_file("a.txt", checksum_str("template\n"));
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "template\n").unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }
}
