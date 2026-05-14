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
use crate::decision::{Decision, DecisionInputs, decide, should_emit_proposed_for_opt_out};
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
    let emptied = on_disk
        .as_deref()
        .is_some_and(|s| s.trim().is_empty());

    let inputs = DecisionInputs {
        last_rendered,
        disk: disk_checksum.as_deref(),
        template: &template_checksum,
        emptied,
    };
    let decision = decide(&inputs);

    let target = Target::File {
        path: relpath.to_owned(),
    };
    let item = match decision {
        Decision::InSync | Decision::LeaveAlone => PlanItem::noop(target, decision),
        Decision::Skipped => {
            if should_emit_proposed_for_opt_out(last_rendered, &template_checksum) {
                // Opt-out with new template content. Emit a proposed sibling
                // so the user can see upstream churn; the opt-out (empty
                // file) survives in place.
                PlanItem::propose_file(relpath, rendered.to_owned())
            } else {
                PlanItem::noop(target, decision)
            }
        }
        Decision::Write => {
            PlanItem::write_file(relpath, rendered.to_owned(), template_checksum)
        }
        Decision::Propose => PlanItem::propose_file(relpath, rendered.to_owned()),
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
    fn empty_file_opts_out_with_unchanged_template() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        let mut manifest = Manifest::default();
        manifest.set_file("a.txt", checksum_str("template\n"));
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "template\n").unwrap();
        assert_eq!(item.decision, Decision::Skipped);
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
    fn whitespace_only_file_is_opt_out() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "   \n\t\n").unwrap();
        let mut manifest = Manifest::default();
        manifest.set_file("a.txt", checksum_str("template\n"));
        let item = plan_owned_file(tmp.path(), &manifest, "a.txt", "template\n").unwrap();
        assert_eq!(item.decision, Decision::Skipped);
    }
}
