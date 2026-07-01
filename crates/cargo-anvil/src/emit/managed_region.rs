// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Driver for a single managed region.
//!
//! Given the host file's current text, a region id, and the rendered
//! region body, this module locates the region (if present), consults the
//! manifest, computes the decision, and returns a [`PlanItem`] ready to be
//! applied.
//!
//! The host text is supplied by the caller rather than read here, so that
//! multiple regions targeting the same host file compose: the caller
//! threads an accumulating in-memory host text (seeded from disk) through
//! every region, and each region splices on top of the previous one's
//! result instead of re-reading the original disk state. See
//! [`crate::run`]'s `HostTextCache` and `updates.md §4`.

use ohno::AppError;

use crate::checksum::checksum_str;
use crate::decision::{Decision, DecisionInputs, UpdateDecision, decide};
use crate::manifest::{Manifest, RegionKey};
use crate::plan::{PlanItem, Target};
use crate::region::{CommentSyntax, find_region, upsert_region};

/// Compute the [`PlanItem`] for a managed region.
///
/// `host_relpath` is the repo-root-relative forward-slash path of the
/// host file. `host_text` is the host file's current content — `None`
/// when the host file does not (yet) exist — which for the second and
/// later regions in one host is the in-memory result of splicing the
/// earlier regions, not the original disk state. `region_id` is the
/// stable region id. `rendered_body` is the byte-exact content the
/// template would render between the sentinels. `syntax` is the host's
/// comment flavor.
///
/// If the host text is `None`, the region is treated as a `Write` and
/// the spliced output will be just the rendered region (sentinels + body).
///
/// # Errors
///
/// Returns an error if the region in the host is malformed.
pub fn plan_managed_region(
    manifest: &Manifest,
    host_relpath: &str,
    host_text: Option<&str>,
    region_id: &str,
    rendered_body: &str,
    syntax: CommentSyntax,
) -> Result<PlanItem, AppError> {
    let template_checksum = checksum_str(rendered_body);
    let key = RegionKey {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    let last_rendered = manifest.regions.get(&key).map(String::as_str);

    let disk_checksum = match host_text {
        None => None,
        Some(text) => find_region(text, region_id, syntax)?.map(|region| checksum_str(region.body_str())),
    };

    let inputs = DecisionInputs {
        last_rendered,
        disk: disk_checksum.as_deref(),
        template: &template_checksum,
    };

    let target = Target::Region {
        host: host_relpath.to_owned(),
        id: region_id.to_owned(),
    };
    let item = match decide(&inputs) {
        UpdateDecision::InSync => PlanItem::insync(target, template_checksum),
        UpdateDecision::LeaveAlone => PlanItem::noop(target, Decision::LeaveAlone),
        UpdateDecision::Write => {
            let spliced = splice(host_text, region_id, rendered_body, syntax)?;
            PlanItem::write_region(host_relpath, region_id, rendered_body.to_owned(), spliced, template_checksum)
        }
        UpdateDecision::Propose => {
            let spliced = splice(host_text, region_id, rendered_body, syntax)?;
            PlanItem::propose_region(host_relpath, region_id, rendered_body.to_owned(), spliced, template_checksum)
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
    use super::*;

    const SYN: CommentSyntax = CommentSyntax::Hash;

    #[test]
    fn missing_host_writes_new_file() {
        let item = plan_managed_region(&Manifest::default(), "Justfile", None, "r", "body line\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("# >>> anvil-managed: r"));
        assert!(spliced.contains("body line"));
    }

    #[test]
    fn existing_host_without_region_appends_region() {
        let item = plan_managed_region(&Manifest::default(), "Justfile", Some("user content\n"), "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.starts_with("user content\n"));
        assert!(spliced.contains("# >>> anvil-managed: r"));
    }

    #[test]
    fn matching_region_is_in_sync() {
        let host = "before\n\
                    # >>> anvil-managed: r\n\
                    body\n\
                    # <<< anvil-managed: r\n\
                    after\n";
        let item = plan_managed_region(&Manifest::default(), "Justfile", Some(host), "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::InSync);
    }

    #[test]
    fn user_modified_proposes_when_template_changed() {
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old body\n"));
        let item = plan_managed_region(&manifest, "Justfile", Some(host), "r", "new body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Propose);
        assert!(item.spliced_host.is_some());
    }

    #[test]
    fn user_modified_template_unchanged_leaves_alone() {
        let host = "# >>> anvil-managed: r\nuser body\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(&manifest, "Justfile", Some(host), "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[test]
    fn empty_region_opts_out_when_template_unchanged() {
        // Steady-state opt-out: user emptied the region, template hasn't moved.
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("body\n"));
        let item = plan_managed_region(&manifest, "Justfile", Some(host), "r", "body\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::LeaveAlone);
    }

    #[test]
    fn empty_region_with_new_template_proposes() {
        let host = "# >>> anvil-managed: r\n# <<< anvil-managed: r\n";
        let mut manifest = Manifest::default();
        manifest.set_region("Justfile", "r", checksum_str("old\n"));
        let item = plan_managed_region(&manifest, "Justfile", Some(host), "r", "new\n", SYN).unwrap();
        // Opt-out remains in place but the user gets a proposed host file.
        assert_eq!(item.decision, Decision::Propose);
    }

    #[test]
    fn composes_onto_existing_region_in_host_text() {
        // A second region planned against host text that already carries a
        // first region must preserve the first and append the second —
        // this is the in-memory composition that lets several regions
        // share one host file (e.g. the sections of deny.toml).
        let host = "# >>> anvil-managed: a\nbody-a\n# <<< anvil-managed: a\n";
        let item = plan_managed_region(&Manifest::default(), "deny.toml", Some(host), "b", "body-b\n", SYN).unwrap();
        assert_eq!(item.decision, Decision::Write);
        let spliced = item.spliced_host.as_deref().unwrap();
        assert!(spliced.contains("anvil-managed: a"), "first region preserved");
        assert!(spliced.contains("body-a"), "first region body preserved");
        assert!(spliced.contains("anvil-managed: b"), "second region appended");
        assert!(spliced.contains("body-b"), "second region body appended");
    }
}
