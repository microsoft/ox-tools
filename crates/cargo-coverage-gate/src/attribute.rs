// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File-to-crate attribution by longest-prefix path match.
//!
//! Every file path that appears in the coverage JSON is mapped to at
//! most one workspace member, by comparing the path against each
//! member's manifest directory. The match is component-aware (so we
//! don't, for example, fold `crates/alpha-extras` into `crates/alpha`)
//! and longest-prefix-first, so a nested member crate wins over an
//! enclosing one.
//!
//! Files that match no member — synthesized paths from proc-macros,
//! generated code outside the workspace tree, build-script outputs in
//! `target/` — are reported via [`AttributionOutcome::unattributed`].
//! The caller emits a single aggregated warning.

use std::collections::BTreeMap;

use crate::llvm_cov::FileEntry;
use crate::workspace::Member;

/// The result of attributing a list of coverage entries to workspace
/// members.
#[derive(Debug, Default)]
pub(crate) struct AttributionOutcome<'f> {
    /// File entries grouped by owning member name.
    pub(crate) by_member: BTreeMap<String, Vec<&'f FileEntry>>,
    /// Files whose path did not match any member's manifest directory.
    pub(crate) unattributed: Vec<&'f FileEntry>,
}

/// Group `files` by owning workspace member.
pub(crate) fn attribute<'f>(
    files: &'f [FileEntry],
    members: &[Member],
) -> AttributionOutcome<'f> {
    // Longest-prefix-first: when paths nest, the deeper member wins.
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(members[i].manifest_dir.components().count()));

    let mut out = AttributionOutcome::default();
    'files: for f in files {
        for &i in &order {
            if f.filename.starts_with(&members[i].manifest_dir) {
                out.by_member
                    .entry(members[i].name.clone())
                    .or_default()
                    .push(f);
                continue 'files;
            }
        }
        out.unattributed.push(f);
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::llvm_cov::{LineCounters, SummaryBlock};

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            filename: PathBuf::from(path),
            summary: SummaryBlock {
                lines: LineCounters {
                    count: 10,
                    covered: 5,
                },
            },
        }
    }

    fn member(name: &str, manifest_dir: &str) -> Member {
        Member {
            name: name.to_owned(),
            manifest_dir: PathBuf::from(manifest_dir),
            min_lines: None,
        }
    }

    #[test]
    fn assigns_files_to_matching_crates() {
        let files = vec![
            entry("/repo/crates/alpha/src/lib.rs"),
            entry("/repo/crates/alpha/src/util.rs"),
            entry("/repo/crates/beta/src/lib.rs"),
        ];
        let members = [
            member("alpha", "/repo/crates/alpha"),
            member("beta", "/repo/crates/beta"),
        ];
        let out = attribute(&files, &members);
        assert_eq!(out.by_member["alpha"].len(), 2);
        assert_eq!(out.by_member["beta"].len(), 1);
        assert!(out.unattributed.is_empty());
    }

    #[test]
    fn unmatched_files_are_reported_separately() {
        let files = vec![
            entry("/repo/crates/alpha/src/lib.rs"),
            entry("/elsewhere/build-script.rs"),
        ];
        let members = [member("alpha", "/repo/crates/alpha")];
        let out = attribute(&files, &members);
        assert_eq!(out.by_member["alpha"].len(), 1);
        assert_eq!(out.unattributed.len(), 1);
        assert_eq!(
            out.unattributed[0].filename,
            PathBuf::from("/elsewhere/build-script.rs")
        );
    }

    #[test]
    fn longest_prefix_wins_for_nested_members() {
        let files = vec![entry("/repo/crates/alpha/sub/src/lib.rs")];
        let members = [
            member("alpha-outer", "/repo/crates/alpha"),
            member("alpha-sub", "/repo/crates/alpha/sub"),
        ];
        let out = attribute(&files, &members);
        // The nested member's path is longer, so it wins.
        assert!(!out.by_member.contains_key("alpha-outer"));
        assert_eq!(out.by_member["alpha-sub"].len(), 1);
    }

    #[test]
    fn similar_named_member_does_not_steal_files() {
        // `alpha-extras` shares a textual prefix with `alpha` but is a
        // separate path component; `starts_with` must be component-aware.
        let files = vec![entry("/repo/crates/alpha-extras/src/lib.rs")];
        let members = [
            member("alpha", "/repo/crates/alpha"),
            member("alpha-extras", "/repo/crates/alpha-extras"),
        ];
        let out = attribute(&files, &members);
        assert!(!out.by_member.contains_key("alpha"));
        assert_eq!(out.by_member["alpha-extras"].len(), 1);
    }

    #[test]
    fn empty_inputs() {
        let files: Vec<FileEntry> = Vec::new();
        let members: Vec<Member> = Vec::new();
        let out = attribute(&files, &members);
        assert!(out.by_member.is_empty());
        assert!(out.unattributed.is_empty());
    }
}
