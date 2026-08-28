// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! One mutant's most recent verdict, and when it was earned.

use crate::elements::MutantResult;

/// One mutant's most recent verdict, and when it was earned.
///
/// Borrowed from the reports that were read, rather than owning a copy of each. Every mutant of
/// every input passes through here, and all but the winner of each identity is discarded — so
/// copying on the way in pays for the losers as well, and the winners are copied again when the
/// merged document is built. The reports outlive the merge, which is what makes the borrow possible.
#[derive(Debug, Clone)]
pub(super) struct Verdict<'reports> {
    /// The mutant as reported.
    pub(super) mutant: &'reports MutantResult,

    /// The file it belongs to.
    pub(super) file: &'reports str,

    /// When the run that produced this verdict started.
    pub(super) tested_at: u64,

    /// The name of the report this verdict was read from.
    ///
    /// Kept only to break an equal-timestamp tie the same way every other choice in the merge does,
    /// so that which of two simultaneous verdicts wins cannot depend on the order the inputs were
    /// listed on the command line.
    pub(super) origin: &'reports str,

    /// A deterministic identity retained through staged merges.
    pub(super) lineage: String,

    /// The rendering details compatible with the merged file's selected source.
    ///
    /// `Pending` does not replace an informative status, but a newer listing still describes the
    /// current source generation. The merged report must render its older verdict at that newer
    /// location rather than point its overlay into text that has since moved.
    ///
    /// A verdict with no compatible presentation is excluded from the rebuilt report.
    pub(super) presentation: Option<&'reports MutantResult>,

    /// The source generation carrying [`Self::presentation`].
    pub(super) presentation_rank: Option<(u64, &'reports str, String)>,
}
