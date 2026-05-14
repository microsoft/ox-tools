// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The three-checksum decision algorithm.
//!
//! For every owned file and every managed region, `cargo-ox-check` makes a
//! single decision per run by comparing three inputs:
//!
//! - `last_rendered` (`L`) — what the manifest says ox-check wrote last
//!   time, or `None` if never seen.
//! - `disk` (`D`) — what is on disk right now. `None` if the file is
//!   missing or the region's host file is missing.
//! - `template` (`T`) — what ox-check's current catalog would write right
//!   now.
//!
//! Plus a per-item `emptied` flag indicating the user has opted out by
//! emptying the file (or, for regions, the region body).
//!
//! See [updates.md §5](../../docs/design/updates.md) for the decision table.

/// Inputs to one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionInputs<'a> {
    /// Checksum of the last-rendered content per the manifest. `None` if
    /// the item is new (never tracked).
    pub last_rendered: Option<&'a str>,
    /// Checksum of the current on-disk content. `None` if the host file
    /// is missing entirely. For an empty file or empty region body, the
    /// `emptied` flag is set; this checksum should be `Some` in that case.
    pub disk: Option<&'a str>,
    /// Checksum of what the current template would render.
    pub template: &'a str,
    /// True iff the user has opted out: an empty file (for owned files)
    /// or an empty region body between sentinels (for regions).
    pub emptied: bool,
}

/// Decision the driver should carry out for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Item is in sync with the current template. Do nothing; manifest
    /// entry stays.
    InSync,
    /// Item is opted out by the user (empty stub). Do nothing; manifest
    /// entry stays. Acts the same as `InSync` for exit-code purposes.
    Skipped,
    /// Render the current template to disk and refresh the manifest.
    Write,
    /// User has diverged AND the template changed since last render —
    /// write a `.ox-check-proposed` sibling and leave the user's content
    /// alone. Manifest stays unchanged.
    Propose,
    /// User has diverged but the template hasn't changed; leave the user's
    /// content alone with no proposed file. Manifest stays unchanged.
    LeaveAlone,
}

impl Decision {
    /// Whether this decision means "everything is in sync" for `--dry-run`
    /// exit-code purposes.
    #[must_use]
    pub const fn is_in_sync(self) -> bool {
        matches!(self, Self::InSync | Self::Skipped | Self::LeaveAlone)
    }

    /// Whether this decision causes writes (the file or the manifest).
    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(self, Self::Write | Self::Propose)
    }
}

/// Compute the decision for one item.
#[must_use]
pub fn decide(inputs: &DecisionInputs<'_>) -> Decision {
    // Opt-out wins as long as it isn't a brand-new item the user has never
    // seen. If the user emptied the file/region, we always skip; whether we
    // *also* emit a proposed file when the template changes is handled by
    // the caller (it needs both the proposed content and the comparison
    // against `last_rendered`). See `should_emit_proposed_for_opt_out`.
    if inputs.emptied {
        return Decision::Skipped;
    }

    match (inputs.disk, inputs.last_rendered) {
        // The on-disk file (or host file) is missing entirely. Either the
        // user deleted it to re-bless, or it has never been written.
        // Either way, we render.
        (None, _) => Decision::Write,

        // Item exists on disk. Compare against template & manifest.
        (Some(d), _) if d == inputs.template => Decision::InSync,
        (Some(d), Some(l)) if d == l => {
            // User hasn't touched it since last render, and it doesn't
            // match the current template => template moved on, so write.
            Decision::Write
        }
        (Some(_), Some(l)) if l == inputs.template => {
            // User has diverged but the template hasn't changed since last
            // render. Don't pester them.
            Decision::LeaveAlone
        }
        (Some(_), Some(_)) => {
            // User diverged AND template moved. Propose.
            Decision::Propose
        }
        (Some(_), None) => {
            // First time we're tracking this item, but the user already
            // has content there that doesn't match the template. Treat as
            // adoption-with-divergence: propose.
            Decision::Propose
        }
    }
}

/// Whether to emit a proposed file for an opted-out item.
///
/// True exactly when the current template differs from what was last
/// rendered — i.e. there is genuine upstream churn the user might want
/// to see. For first-time encounters (no `last_rendered`) we propose so
/// the user can review.
#[must_use]
pub fn should_emit_proposed_for_opt_out(
    last_rendered: Option<&str>,
    template: &str,
) -> bool {
    match last_rendered {
        Some(l) => l != template,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        last: Option<&'a str>,
        disk: Option<&'a str>,
        template: &'a str,
        emptied: bool,
    ) -> DecisionInputs<'a> {
        DecisionInputs {
            last_rendered: last,
            disk,
            template,
            emptied,
        }
    }

    #[test]
    fn missing_disk_writes() {
        assert_eq!(decide(&inputs(None, None, "T", false)), Decision::Write);
        assert_eq!(decide(&inputs(Some("L"), None, "T", false)), Decision::Write);
    }

    #[test]
    fn disk_matches_template_in_sync() {
        assert_eq!(
            decide(&inputs(Some("L"), Some("T"), "T", false)),
            Decision::InSync
        );
        // Even with no manifest entry, a perfect match is in sync.
        assert_eq!(
            decide(&inputs(None, Some("T"), "T", false)),
            Decision::InSync
        );
    }

    #[test]
    fn disk_matches_last_template_changed_writes() {
        assert_eq!(
            decide(&inputs(Some("L"), Some("L"), "T", false)),
            Decision::Write
        );
    }

    #[test]
    fn user_diverged_template_unchanged_leaves_alone() {
        assert_eq!(
            decide(&inputs(Some("L"), Some("D"), "L", false)),
            Decision::LeaveAlone
        );
    }

    #[test]
    fn user_diverged_template_changed_proposes() {
        assert_eq!(
            decide(&inputs(Some("L"), Some("D"), "T", false)),
            Decision::Propose
        );
    }

    #[test]
    fn first_time_with_existing_user_content_proposes() {
        assert_eq!(
            decide(&inputs(None, Some("D"), "T", false)),
            Decision::Propose
        );
    }

    #[test]
    fn emptied_skips_regardless_of_other_checksums() {
        assert_eq!(
            decide(&inputs(Some("L"), Some(""), "T", true)),
            Decision::Skipped
        );
        assert_eq!(decide(&inputs(None, Some(""), "T", true)), Decision::Skipped);
    }

    #[test]
    fn opt_out_proposal_only_on_template_change() {
        assert!(should_emit_proposed_for_opt_out(Some("L"), "T"));
        assert!(!should_emit_proposed_for_opt_out(Some("L"), "L"));
        // First time seeing the item — propose so the user can review.
        assert!(should_emit_proposed_for_opt_out(None, "T"));
    }

    #[test]
    fn decision_in_sync_predicate() {
        assert!(Decision::InSync.is_in_sync());
        assert!(Decision::Skipped.is_in_sync());
        assert!(Decision::LeaveAlone.is_in_sync());
        assert!(!Decision::Write.is_in_sync());
        assert!(!Decision::Propose.is_in_sync());
    }

    #[test]
    fn decision_writes_predicate() {
        assert!(!Decision::InSync.writes());
        assert!(!Decision::Skipped.writes());
        assert!(!Decision::LeaveAlone.writes());
        assert!(Decision::Write.writes());
        assert!(Decision::Propose.writes());
    }
}
