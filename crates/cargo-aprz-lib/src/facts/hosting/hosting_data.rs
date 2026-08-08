// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::age_stats::AgeStats;
use super::time_window_stats::TimeWindowStats;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostingData {
    pub stars: u64,
    pub forks: u64,
    pub subscribers: u64,

    // Issues
    //
    // These cover ALL issues. Bug metrics below are a strict subset of these.

    pub open_issues: u64,
    pub open_issue_age: AgeStats,
    pub issues_opened: TimeWindowStats,
    pub issues_closed: TimeWindowStats,
    pub closed_issue_age: AgeStats,
    pub closed_issue_age_last_90_days: AgeStats,
    pub closed_issue_age_last_180_days: AgeStats,
    pub closed_issue_age_last_365_days: AgeStats,

    // Bugs
    //
    // Issues carrying a label that matches the configured `bug_labels` patterns.
    // Unlabeled issues are never counted as bugs; `labeled_issue_ratio` reports
    // how thoroughly the repository labels its issues.

    pub open_bugs: u64,
    pub open_bug_age: AgeStats,
    pub bugs_opened: TimeWindowStats,
    pub bugs_closed: TimeWindowStats,
    pub closed_bug_age: AgeStats,
    pub closed_bug_age_last_90_days: AgeStats,
    pub closed_bug_age_last_180_days: AgeStats,
    pub closed_bug_age_last_365_days: AgeStats,

    /// Percentage (0-100) of issues carrying at least one label.
    pub labeled_issue_ratio: u32,

    // Pull Requests


    pub open_prs: u64,
    pub open_pr_age: AgeStats,
    pub prs_opened: TimeWindowStats,
    pub prs_merged: TimeWindowStats,
    pub prs_closed: TimeWindowStats,
    pub merged_pr_age: AgeStats,
    pub merged_pr_age_last_90_days: AgeStats,
    pub merged_pr_age_last_180_days: AgeStats,
    pub merged_pr_age_last_365_days: AgeStats,
}
