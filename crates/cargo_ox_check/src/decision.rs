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
//! - `template` (`T`) — what ox-check's current catalog would render right
//!   now.
//!
//! Opt-out via emptying needs no separate flag: an empty file or
//! whitespace-only region body has a stable checksum that no template
//! ever produces, so `D ≠ L` lands the item in `LeaveAlone` (when the
//! template is unchanged) or `Propose` (when it has moved). Both
//! outcomes preserve the user's empty stub.
//!
//! See [updates.md §5](../../docs/design/updates.md) for the decision table.

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

/// Decision the driver should carry out for one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Item is in sync with the current template. Do nothing; manifest
    /// entry stays.
    InSync,
    /// Render the current template to disk and refresh the manifest.
    Write,
    /// User has diverged AND the template changed since last render —
    /// write a `.ox-check-proposed` sibling and leave the user's content
    /// alone. Manifest stays unchanged.
    Propose,
    /// User has diverged but the template hasn't changed; leave the
    /// user's content alone with no proposed file. Manifest stays
    /// unchanged. Also the steady-state outcome for opt-out (empty file
    /// or empty region body) when the template hasn't moved.
    LeaveAlone,
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
        matches!(self, Self::Write | Self::Propose)
    }
}

/// Compute the decision for one item.
#[must_use]
pub fn decide(inputs: &DecisionInputs<'_>) -> Decision {
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
            // User has diverged but the template hasn't changed since
            // last render. Don't pester them. (This is also the
            // steady-state outcome for opt-out: empty content stays
            // empty, template hasn't moved.)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs<'a>(
        last: Option<&'a str>,
        disk: Option<&'a str>,
        template: &'a str,
    ) -> DecisionInputs<'a> {
        DecisionInputs {
            last_rendered: last,
            disk,
            template,
        }
    }

    #[test]
    fn missing_disk_writes() {
        assert_eq!(decide(&inputs(None, None, "T")), Decision::Write);
        assert_eq!(decide(&inputs(Some("L"), None, "T")), Decision::Write);
    }

    #[test]
    fn disk_matches_template_in_sync() {
        assert_eq!(decide(&inputs(Some("L"), Some("T"), "T")), Decision::InSync);
        assert_eq!(decide(&inputs(None, Some("T"), "T")), Decision::InSync);
    }

    #[test]
    fn disk_matches_last_template_changed_writes() {
        assert_eq!(decide(&inputs(Some("L"), Some("L"), "T")), Decision::Write);
    }

    #[test]
    fn user_diverged_template_unchanged_leaves_alone() {
        assert_eq!(
            decide(&inputs(Some("L"), Some("D"), "L")),
            Decision::LeaveAlone
        );
    }

    #[test]
    fn user_diverged_template_changed_proposes() {
        assert_eq!(
            decide(&inputs(Some("L"), Some("D"), "T")),
            Decision::Propose
        );
    }

    #[test]
    fn first_time_with_existing_user_content_proposes() {
        assert_eq!(decide(&inputs(None, Some("D"), "T")), Decision::Propose);
    }

    #[test]
    fn opt_out_via_empty_steady_state_leaves_alone() {
        // After a successful render the manifest has L = template. The
        // user empties the file (D = empty-checksum, never equal to L).
        // Template hasn't moved (T == L). Result: LeaveAlone, silent.
        let empty = "sha256:empty";
        let tmpl = "sha256:template";
        assert_eq!(
            decide(&inputs(Some(tmpl), Some(empty), tmpl)),
            Decision::LeaveAlone
        );
    }

    #[test]
    fn opt_out_via_empty_with_template_change_proposes() {
        // Same as above but the template has since moved — the user gets
        // a proposed sibling so they can see what's new.
        let empty = "sha256:empty";
        assert_eq!(
            decide(&inputs(Some("sha256:old"), Some(empty), "sha256:new")),
            Decision::Propose
        );
    }

    #[test]
    fn decision_in_sync_predicate() {
        assert!(Decision::InSync.is_in_sync());
        assert!(Decision::LeaveAlone.is_in_sync());
        assert!(!Decision::Write.is_in_sync());
        assert!(!Decision::Propose.is_in_sync());
    }

    #[test]
    fn decision_writes_predicate() {
        assert!(!Decision::InSync.writes());
        assert!(!Decision::LeaveAlone.writes());
        assert!(Decision::Write.writes());
        assert!(Decision::Propose.writes());
    }
}
