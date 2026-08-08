// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Raw, unaggregated repository data as stored in the cache.
//!
//! The cache deliberately stores raw issue records rather than computed statistics.
//! Issue classification (notably which labels mark an issue as a bug) is user-configurable,
//! so aggregating at fetch time would bake the configuration into the cache and silently
//! serve stale statistics after a configuration change. Computing statistics on load keeps
//! configuration changes free of network cost.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::bug_labels::BugLabelMatcher;
use super::client::{Issue, IssueState};

/// A single issue or pull request, retaining only the fields needed to compute statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedIssue {
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub is_open: bool,
    pub is_pr: bool,
    pub merged_at: Option<DateTime<Utc>>,
    pub labels: Vec<String>,
}

impl CachedIssue {
    /// Returns true if any of this issue's labels matches one of the configured bug patterns.
    ///
    /// Patterns are case-insensitive regular expressions matched anywhere within the label, so
    /// conventional prefixed labels such as `C-bug`, `type: bug`, and `kind/bug` all match the
    /// pattern `bug`.
    #[must_use]
    pub fn is_bug(&self, bug_labels: &BugLabelMatcher) -> bool {
        self.labels.iter().any(|label| bug_labels.is_match(label))
    }
}

impl From<Issue> for CachedIssue {
    fn from(issue: Issue) -> Self {
        Self {
            created_at: issue.created_at,
            closed_at: issue.closed_at,
            is_open: issue.state == IssueState::Open,
            is_pr: issue.pull_request.is_some(),
            merged_at: issue.pull_request.as_ref().and_then(|pr| pr.merged_at),
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
        }
    }
}

/// Raw repository data as cached on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRepo {
    pub stars: u64,
    pub forks: u64,
    pub subscribers: u64,
    pub issues: Vec<CachedIssue>,
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::super::client::{Label, PullRequestMarker};
    use super::*;

    fn bug_patterns() -> BugLabelMatcher {
        BugLabelMatcher::new(&[
            "bug".to_string(),
            "crash".to_string(),
            "defect".to_string(),
            "regression".to_string(),
        ])
        .unwrap()
    }

    fn issue_with_labels(labels: &[&str]) -> CachedIssue {
        CachedIssue {
            created_at: Utc::now(),
            closed_at: None,
            is_open: true,
            is_pr: false,
            merged_at: None,
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
        }
    }

    #[test]
    fn unlabeled_issue_is_not_a_bug() {
        assert!(!issue_with_labels(&[]).is_bug(&bug_patterns()));
    }

    #[test]
    fn exact_bug_label_matches() {
        assert!(issue_with_labels(&["bug"]).is_bug(&bug_patterns()));
    }

    #[test]
    fn prefixed_bug_labels_match() {
        for label in ["C-bug", "type: bug", "kind/bug", "Bug", "BUG"] {
            assert!(issue_with_labels(&[label]).is_bug(&bug_patterns()), "expected '{label}' to match");
        }
    }

    #[test]
    fn non_bug_labels_do_not_match() {
        for label in ["enhancement", "documentation", "question", "good first issue"] {
            assert!(
                !issue_with_labels(&[label]).is_bug(&bug_patterns()),
                "expected '{label}' not to match"
            );
        }
    }

    #[test]
    fn mixed_labels_match_when_any_is_a_bug() {
        assert!(issue_with_labels(&["enhancement", "C-bug", "P-low"]).is_bug(&bug_patterns()));
    }

    #[test]
    fn other_default_patterns_match() {
        assert!(issue_with_labels(&["crash"]).is_bug(&bug_patterns()));
        assert!(issue_with_labels(&["defect"]).is_bug(&bug_patterns()));
        assert!(issue_with_labels(&["regression"]).is_bug(&bug_patterns()));
    }

    #[test]
    fn empty_pattern_list_matches_nothing() {
        assert!(!issue_with_labels(&["bug"]).is_bug(&BugLabelMatcher::default()));
    }

    #[test]
    fn custom_patterns_are_honored() {
        let patterns = BugLabelMatcher::new(&["crash".to_string()]).unwrap();
        assert!(issue_with_labels(&["crash"]).is_bug(&patterns));
        assert!(!issue_with_labels(&["bug"]).is_bug(&patterns));
    }

    #[test]
    fn regex_patterns_are_honored() {
        let patterns = BugLabelMatcher::new(&["^(c|kind)[-/]bug$".to_string()]).unwrap();
        assert!(issue_with_labels(&["C-bug"]).is_bug(&patterns));
        assert!(issue_with_labels(&["kind/bug"]).is_bug(&patterns));
        assert!(!issue_with_labels(&["type: bug"]).is_bug(&patterns));
    }

    #[test]
    fn converts_open_issue_from_api_type() {
        let created = Utc::now();
        let issue = Issue {
            created_at: created,
            closed_at: None,
            state: IssueState::Open,
            pull_request: None,
            labels: vec![Label { name: "C-bug".to_string() }],
        };

        let cached = CachedIssue::from(issue);
        assert!(cached.is_open);
        assert!(!cached.is_pr);
        assert_eq!(cached.merged_at, None);
        assert_eq!(cached.labels, vec!["C-bug".to_string()]);
    }

    #[test]
    fn converts_merged_pr_from_api_type() {
        let created = Utc::now();
        let merged = created + chrono::Duration::days(1);
        let issue = Issue {
            created_at: created,
            closed_at: Some(merged),
            state: IssueState::Closed,
            pull_request: Some(PullRequestMarker { merged_at: Some(merged) }),
            labels: Vec::new(),
        };

        let cached = CachedIssue::from(issue);
        assert!(!cached.is_open);
        assert!(cached.is_pr);
        assert_eq!(cached.merged_at, Some(merged));
    }

    #[test]
    fn round_trips_through_messagepack() {
        let repo = CachedRepo {
            stars: 10,
            forks: 2,
            subscribers: 3,
            issues: vec![issue_with_labels(&["bug"])],
        };

        let bytes = rmp_serde::to_vec(&repo).unwrap();
        let restored: CachedRepo = rmp_serde::from_slice(&bytes).unwrap();

        assert_eq!(restored.stars, 10);
        assert_eq!(restored.issues.len(), 1);
        assert_eq!(restored.issues[0].labels, vec!["bug".to_string()]);
    }
}
