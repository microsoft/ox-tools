// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The three-checksum decision algorithm.
//!
//! For every owned file and every managed region, `cargo-anvil` makes a
//! single decision per run by comparing three inputs:
//!
//! - `last_rendered` (`L`) — what the manifest says anvil wrote last
//!   time, or `None` if never seen.
//! - `disk` (`D`) — what is on disk right now. `None` if the file is
//!   missing or the region's host file is missing.
//! - `template` (`T`) — what anvil's current catalog would render right
//!   now.
//!
//! Opt-out via emptying needs no separate flag: an empty file or
//! whitespace-only region body has a stable checksum that no template
//! ever produces, so `D ≠ L` lands the item in `LeaveAlone` (when the
//! template is unchanged) or `Propose` (when it has moved). Both
//! outcomes preserve the user's empty stub.
//!
//! See [`updates.md §5`](../../docs/design/updates.md) for the decision table.

/// Inputs to one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionInputs<'a> {
    /// Checksum of the last-rendered content per the manifest. `None` if
    /// the item is new (never tracked).
    pub last_rendered: Option<&'a str>,
    /// Checksum of the current on-disk content. `None` if the host file
    /// is missing entirely.
    pub disk: Option<&'a str>,
    /// Checksum of what the current template would render.
    pub template: &'a str,
}

/// Decision for one item that is still in the catalog (an *update* pass).
///
/// This is the return type of [`decide`]; it deliberately omits the
/// removal outcomes so callers never need an unreachable arm. The
/// variants map 1:1 onto the matching [`Decision`] values when a plan
/// item is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Item is in sync with the current template. Do nothing; manifest
    /// entry stays.
    InSync,
    /// Render the current template to disk and refresh the manifest.
    Write,
    /// User has diverged AND the template changed since last render —
    /// write a `.anvil-proposed` sibling and leave the user's content
    /// alone. Manifest stays unchanged.
    Propose,
    /// User has diverged but the template hasn't changed; leave the
    /// user's content alone with no proposed file. Manifest stays
    /// unchanged. Also the steady-state outcome for opt-out (empty file
    /// or empty region body) when the template hasn't moved.
    LeaveAlone,
}

/// Decision for a previously-tracked item no longer in the catalog (a
/// *removal* pass).
///
/// Returned by [`decide_removal`]; omitting the update outcomes lets
/// callers match exhaustively without an unreachable arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalDecision {
    /// On-disk content is already missing; just purge the residual
    /// manifest entry (no file action).
    AlreadyGone,
    /// On-disk content still matches `last_rendered`; safe to delete the
    /// file or splice out the region. Manifest entry is dropped.
    Remove,
    /// User customized the content since the last render; leave it in
    /// place but drop the manifest entry so ownership transfers.
    OrphanedKept,
}

/// Decision the driver should carry out for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Item is in sync with the current template. Do nothing; manifest
    /// entry stays.
    InSync,
    /// Render the current template to disk and refresh the manifest.
    Write,
    /// User has diverged AND the template changed since last render —
    /// write a `.anvil-proposed` sibling and leave the user's content
    /// alone. Manifest stays unchanged.
    Propose,
    /// User has diverged but the template hasn't changed; leave the
    /// user's content alone with no proposed file. Manifest stays
    /// unchanged. Also the steady-state outcome for opt-out (empty file
    /// or empty region body) when the template hasn't moved.
    LeaveAlone,
    /// Item was in the previous manifest but is no longer in the
    /// catalog. The on-disk content still matches `last_rendered`, so
    /// the user has not customized it — safe to delete (file) or
    /// splice out (region). Manifest entry is dropped.
    Remove,
    /// Item was in the previous manifest, is no longer in the catalog,
    /// and the user has customized it since the last render. Leave the
    /// file/region in place but drop the manifest entry so ownership
    /// transfers to the user (no more anvil tracking).
    OrphanedKept,
}

impl Decision {
    /// Whether this decision means "everything is in sync" for `--dry-run`
    /// exit-code purposes.
    #[must_use]
    pub const fn is_in_sync(self) -> bool {
        matches!(self, Self::InSync | Self::LeaveAlone)
    }

    /// Whether this decision causes writes (the file or the manifest).
    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(self, Self::Write | Self::Propose | Self::Remove | Self::OrphanedKept)
    }
}

/// Compute the decision for one item.
#[must_use]
pub fn decide(inputs: &DecisionInputs<'_>) -> UpdateDecision {
    match (inputs.disk, inputs.last_rendered) {
        // The on-disk file (or host file) is missing entirely. Either the
        // user deleted it to re-bless, or it has never been written.
        // Either way, we render.
        (None, _) => UpdateDecision::Write,

        // Item exists on disk. Compare against template & manifest.
        (Some(d), _) if d == inputs.template => UpdateDecision::InSync,
        (Some(d), Some(l)) if d == l => {
            // User hasn't touched it since last render, and it doesn't
            // match the current template => template moved on, so write.
            UpdateDecision::Write
        }
        (Some(_), Some(l)) if l == inputs.template => {
            // User has diverged but the template hasn't changed since
            // last render. Don't pester them. (This is also the
            // steady-state outcome for opt-out: empty content stays
            // empty, template hasn't moved.)
            UpdateDecision::LeaveAlone
        }
        (Some(_), Some(_)) => {
            // User diverged AND template moved. Propose.
            UpdateDecision::Propose
        }
        (Some(_), None) => {
            // First time we're tracking this item, but the user already
            // has content there that doesn't match the template. Treat as
            // adoption-with-divergence: propose.
            UpdateDecision::Propose
        }
    }
}

/// Compute the decision for one previously-tracked item that is no
/// longer in the catalog.
///
/// - If the on-disk content is missing, the item is already gone;
///   treat the residual manifest entry as already-gone (no plan item
///   needed beyond purging the manifest).
/// - If the on-disk content matches `last_rendered`, the user hasn't
///   touched it since anvil wrote it. Safe to remove.
/// - Otherwise the user has customized; transfer ownership and leave
///   the content in place.
#[must_use]
pub fn decide_removal(last_rendered: &str, disk: Option<&str>) -> RemovalDecision {
    match disk {
        None => RemovalDecision::AlreadyGone,
        Some(d) if d == last_rendered => RemovalDecision::Remove,
        Some(_) => RemovalDecision::OrphanedKept,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn inputs<'a>(last: Option<&'a str>, disk: Option<&'a str>, template: &'a str) -> DecisionInputs<'a> {
        DecisionInputs {
            last_rendered: last,
            disk,
            template,
        }
    }

    #[test]
    fn missing_disk_writes() {
        assert_eq!(decide(&inputs(None, None, "T")), UpdateDecision::Write);
        assert_eq!(decide(&inputs(Some("L"), None, "T")), UpdateDecision::Write);
    }

    #[test]
    fn disk_matches_template_in_sync() {
        assert_eq!(decide(&inputs(Some("L"), Some("T"), "T")), UpdateDecision::InSync);
        assert_eq!(decide(&inputs(None, Some("T"), "T")), UpdateDecision::InSync);
    }

    #[test]
    fn disk_matches_last_template_changed_writes() {
        assert_eq!(decide(&inputs(Some("L"), Some("L"), "T")), UpdateDecision::Write);
    }

    #[test]
    fn user_diverged_template_unchanged_leaves_alone() {
        assert_eq!(decide(&inputs(Some("L"), Some("D"), "L")), UpdateDecision::LeaveAlone);
    }

    #[test]
    fn user_diverged_template_changed_proposes() {
        assert_eq!(decide(&inputs(Some("L"), Some("D"), "T")), UpdateDecision::Propose);
    }

    #[test]
    fn first_time_with_existing_user_content_proposes() {
        assert_eq!(decide(&inputs(None, Some("D"), "T")), UpdateDecision::Propose);
    }

    #[test]
    fn opt_out_via_empty_steady_state_leaves_alone() {
        // After a successful render the manifest has L = template. The
        // user empties the file (D = empty-checksum, never equal to L).
        // Template hasn't moved (T == L). Result: LeaveAlone, silent.
        let empty = "sha256:empty";
        let tmpl = "sha256:template";
        assert_eq!(decide(&inputs(Some(tmpl), Some(empty), tmpl)), UpdateDecision::LeaveAlone);
    }

    #[test]
    fn opt_out_via_empty_with_template_change_proposes() {
        // Same as above but the template has since moved — the user gets
        // a proposed sibling so they can see what's new.
        let empty = "sha256:empty";
        assert_eq!(
            decide(&inputs(Some("sha256:old"), Some(empty), "sha256:new")),
            UpdateDecision::Propose
        );
    }

    #[test]
    fn decision_in_sync_predicate() {
        assert!(Decision::InSync.is_in_sync());
        assert!(Decision::LeaveAlone.is_in_sync());
        assert!(!Decision::Write.is_in_sync());
        assert!(!Decision::Propose.is_in_sync());
        assert!(!Decision::Remove.is_in_sync());
        assert!(!Decision::OrphanedKept.is_in_sync());
    }

    #[test]
    fn decision_writes_predicate() {
        assert!(!Decision::InSync.writes());
        assert!(!Decision::LeaveAlone.writes());
        assert!(Decision::Write.writes());
        assert!(Decision::Propose.writes());
        assert!(Decision::Remove.writes());
        assert!(Decision::OrphanedKept.writes());
    }

    #[test]
    fn removal_missing_disk_is_already_gone() {
        // Already gone — no action needed beyond manifest purge.
        assert_eq!(decide_removal("sha256:abc", None), RemovalDecision::AlreadyGone);
    }

    #[test]
    fn removal_untouched_disk_removes() {
        // Disk matches what we wrote last; safe to delete.
        assert_eq!(decide_removal("sha256:abc", Some("sha256:abc")), RemovalDecision::Remove);
    }

    #[test]
    fn removal_customized_disk_orphans() {
        // User edited since last render — keep, transfer ownership.
        assert_eq!(decide_removal("sha256:abc", Some("sha256:xyz")), RemovalDecision::OrphanedKept);
    }
}
