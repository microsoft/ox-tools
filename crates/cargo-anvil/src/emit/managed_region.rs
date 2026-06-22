// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driver for a single managed region.
//!
//! Given a host file path, region id, and the rendered region body, this
//! module reads the host file (if any), locates the region (if present),
//! consults the manifest, computes the decision, and returns a
//! [`PlanItem`] ready to be applied.

use std::path::Path;

use ohno::AppError;

use crate::checksum::checksum_str;
use crate::decision::{Decision, DecisionInputs, decide};
use crate::io::read_file_if_present;
use crate::manifest::{Manifest, RegionKey};
use crate::plan::{PlanItem, Target};
use crate::region::{CommentSyntax, find_region, upsert_region};

/// Compute the [`PlanItem`] for a managed region.
///
/// `host_relpath` is the repo-root-relative forward-slash path of the
/// host file. `region_id` is the stable region id. `rendered_body` is
/// the byte-exact content the template would render between the
/// sentinels. `syntax` is the host's comment flavor.
///
/// If the host file is missing, the region is treated as a `Write` and
/// the spliced output will be just the rendered region (sentinels + body).
///
/// # Errors
///
/// Returns an error if the host file exists but can't be read, or if the
/// region in the host is malformed.
pub fn plan_managed_region(
    repo_root: &Path,
    manifest: &Manifest,
    host_relpath: &str,
    region_id: &str,
    rendered_body: &str,
    syntax: CommentSyntax,
) -> Result<PlanItem, AppError> {
    let abs = repo_root.join(host_relpath);
    let host_text = read_file_if_present(&abs)?;

    let template_checksum = checksum_str(rendered_body);
    let key = RegionKey {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    let last_rendered = manifest.regions.get(&key).map(String::as_str);

    let disk_checksum = match host_text.as_deref() {
        None => None,
        Some(text) => find_region(text, region_id, syntax)?.map(|region| checksum_str(region.body_str())),
    };

    let inputs = DecisionInputs {
        last_rendered,
        disk: disk_checksum.as_deref(),
        template: &template_checksum,
    };
    let decision = decide(&inputs);

    let target = Target::Region {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    let item = match decision {
        Decision::InSync => PlanItem::insync(target, template_checksum),
        Decision::LeaveAlone => PlanItem::noop(target, decision),
        Decision::Write => {
            let spliced = splice(host_text.as_deref(), region_id, rendered_body, syntax)?;
            PlanItem::write_region(host_relpath, region_id, rendered_body.to_owned(), spliced, template_checksum)
        }
        Decision::Propose => {
            let spliced = splice(host_text.as_deref(), region_id, rendered_body, syntax)?;
            PlanItem::propose_region(host_relpath, region_id, spliced, template_checksum)
        }
        Decision::Remove | Decision::OrphanedKept => {
            unreachable!("decide() never returns removal decisions; those come from plan_removals")
        }
    };

    Ok(item)
}

fn splice(host_text: Option<&str>, region_id: &str, rendered_body: &str, syntax: CommentSyntax) -> Result<String, AppError> {
    let base = host_text.unwrap_or("");
    upsert_region(base, region_id, rendered_body, syntax)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const SYN: CommentSyntax = CommentSyntax::Hash;

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn missing_host_writes_new_file() {
        let tmp = TempDir::new().unwrap();
        let item = plan_managed_region(tmp.path(), &Manifest::default(), "Justfile", "r", "body line\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("# >>> anvil-managed: r"));
        assert!(spliced.contains("body line"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn existing_host_without_region_appends_region() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Justfile"), "user content\n").unwrap();
        let item = plan_managed_region(tmp.path(), &Manifest::default(), "Justfile", "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.starts_with("user content\n"));
        assert!(spliced.contains("# >>> anvil-managed: r"));
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn matching_region_is_in_sync() {
        let tmp = TempDir::new().unwrap();
        let host = "before\n\
                    # >>> anvil-managed: r\n\
                    body\n\
                    # <<< anvil-managed: r\n\
                    after\n";
        std::fs::write(tmp.path().join("Justfile"), host).unwrap();
        let item = plan_managed_region(tmp.path(), &Manifest::default(), "Justfile", "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::InSync);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn user_modified_proposes_when_template_changed() {
        let tmp = TempDir::new().unwrap();
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        std::fs::write(tmp.path().join("Justfile"), host).unwrap();
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old body\n"));
        let item = plan_managed_region(tmp.path(), &manifest, "Justfile", "r", "new body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Propose);
        assert!(item.spliced_host.is_some());
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn user_modified_template_unchanged_leaves_alone() {
        let tmp = TempDir::new().unwrap();
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        std::fs::write(tmp.path().join("Justfile"), host).unwrap();
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(tmp.path(), &manifest, "Justfile", "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn empty_region_opts_out_when_template_unchanged() {
        // Steady-state opt-out: user emptied the region, template hasn't moved.
        let tmp = TempDir::new().unwrap();
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        std::fs::write(tmp.path().join("Justfile"), host).unwrap();
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(tmp.path(), &manifest, "Justfile", "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn empty_region_with_new_template_proposes() {
        let tmp = TempDir::new().unwrap();
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        std::fs::write(tmp.path().join("Justfile"), host).unwrap();
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old\n"));
        let item = plan_managed_region(tmp.path(), &manifest, "Justfile", "r", "new\n", SYN).unwrap();
        // Opt-out remains in place but the user gets a proposed host file.
        assert_eq!(item.decision, Decision::Propose);
    }
}
