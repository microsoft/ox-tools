// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Package selection: parse cargo-style selectors and resolve them against
//! a [`Workspace`].
//!
//! Mirrors `cargo build`'s selection surface: `-p`/`--package` (with glob
//! support and optional `@version` qualifier), `--workspace`/`--all`, and
//! `--exclude`, plus the `cargo-each`-specific `--none` (explicit empty set).
//! When nothing is named the default is cargo's `default-members`, exactly
//! like `cargo build`.
//!
//! A computed selection (e.g. an impact tier) is fed in as ordinary flags via
//! shell expansion by the caller; this module has no notion of files or
//! environment variables.

use std::collections::HashSet;

use crate::error::{EachError, UnknownSelectorError};
use crate::workspace::{Member, Workspace};

/// A parsed package selection, before it is resolved against a workspace.
///
/// Populated from command-line flags. A caller with a computed selection
/// (e.g. an impact tier) passes it as ordinary `-p` / `--workspace` / `--none`
/// flags via shell expansion.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    /// `-p` / `--package` selectors (name, `name@version`, or glob).
    pub packages: Vec<String>,
    /// `--workspace` / `--all`: select every member.
    pub all: bool,
    /// `--exclude` selectors (only meaningful with `all`).
    pub exclude: Vec<String>,
    /// `--none`: explicitly resolve to the empty set.
    pub none: bool,
}

impl Selection {
    /// Whether the resolved set is the *entire* workspace, selected via
    /// `--workspace` / `--all` with no narrowing excludes.
    ///
    /// This is the condition under which the `{packages}` placeholder emits
    /// a bare `--workspace` rather than an explicit `--package` list.
    #[must_use]
    pub fn is_whole_workspace(&self) -> bool {
        self.all && self.packages.is_empty() && self.exclude.is_empty() && !self.none
    }

    /// Resolve this selection against `workspace`.
    ///
    /// Returns the selected members in the workspace's alphabetical order.
    /// A `-p` / `--exclude` selector that matches no member is an error, so
    /// typos fail loudly rather than silently skipping.
    ///
    /// # Errors
    ///
    /// Returns [`EachError`] if any `-p` / `--exclude` selector matches no
    /// workspace member.
    pub fn resolve<'w>(&self, workspace: &'w Workspace) -> Result<Vec<&'w Member>, EachError> {
        if self.none {
            return Ok(Vec::new());
        }

        let mut base: Vec<&Member> = if self.all {
            workspace.members.iter().collect()
        } else if !self.packages.is_empty() {
            resolve_selectors(workspace, &self.packages)?
        } else {
            workspace
                .members
                .iter()
                .filter(|m| workspace.default_member_names.contains(&m.name))
                .collect()
        };

        if !self.exclude.is_empty() {
            let excluded: HashSet<&str> = resolve_selectors(workspace, &self.exclude)?
                .into_iter()
                .map(|m| m.name.as_str())
                .collect();
            base.retain(|m| !excluded.contains(m.name.as_str()));
        }

        Ok(base)
    }
}

/// Resolve a list of selectors against the workspace, deduplicating and
/// preserving the workspace's member order. Each selector must match at
/// least one member.
fn resolve_selectors<'w>(workspace: &'w Workspace, selectors: &[String]) -> Result<Vec<&'w Member>, EachError> {
    let mut matched: HashSet<&str> = HashSet::new();
    for selector in selectors {
        // A `name@version` spec matches on the name part only.
        let name_pat = selector.split_once('@').map_or(selector.as_str(), |(n, _)| n);
        let hits: Vec<&Member> = workspace.members.iter().filter(|m| glob_matches(name_pat, &m.name)).collect();
        if hits.is_empty() {
            return Err(UnknownSelectorError::new(selector.clone()).into());
        }
        for m in hits {
            matched.insert(m.name.as_str());
        }
    }
    Ok(workspace.members.iter().filter(|m| matched.contains(m.name.as_str())).collect())
}

/// Tiny Unix-style glob matcher: `*` matches any run of characters
/// (including empty), `?` matches exactly one character. Everything else
/// matches literally.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob_inner(&p, 0, &n, 0)
}

// The position-counter arithmetic (`pi += 1`, `k in ni..=n.len()`) produces
// non-terminating cargo-mutants mutants for patterns containing `*`; the
// behavioral tests cover every observable case, so skip mutating the body.
#[mutants::skip]
fn glob_inner(p: &[char], mut pi: usize, n: &[char], mut ni: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' => {
                while pi < p.len() && p[pi] == '*' {
                    pi += 1;
                }
                if pi == p.len() {
                    return true;
                }
                for k in ni..=n.len() {
                    if glob_inner(p, pi, n, k) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if ni >= n.len() {
                    return false;
                }
                pi += 1;
                ni += 1;
            }
            c => {
                if ni >= n.len() || n[ni] != c {
                    return false;
                }
                pi += 1;
                ni += 1;
            }
        }
    }
    ni == n.len()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde_json::Value;

    use super::*;

    fn member(name: &str) -> Member {
        Member {
            name: name.to_owned(),
            version: "0.1.0".to_owned(),
            manifest_path: PathBuf::from(format!("/ws/{name}/Cargo.toml")),
            has_lib: true,
            has_bin: false,
            dependencies: BTreeSet::new(),
            metadata: Value::Null,
        }
    }

    fn workspace(defaults: &[&str]) -> Workspace {
        Workspace {
            members: vec![member("alpha"), member("beta"), member("gamma")],
            default_member_names: defaults.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn names(members: &[&Member]) -> Vec<String> {
        members.iter().map(|m| m.name.clone()).collect()
    }

    #[test]
    fn none_resolves_empty() {
        let ws = workspace(&["alpha", "beta", "gamma"]);
        let sel = Selection {
            none: true,
            ..Selection::default()
        };
        assert!(sel.resolve(&ws).expect("resolve").is_empty());
    }

    #[test]
    fn all_selects_every_member() {
        let ws = workspace(&["alpha"]);
        let sel = Selection {
            all: true,
            ..Selection::default()
        };
        assert_eq!(names(&sel.resolve(&ws).expect("resolve")), ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn empty_selection_uses_default_members() {
        let ws = workspace(&["alpha", "gamma"]);
        let sel = Selection::default();
        assert_eq!(names(&sel.resolve(&ws).expect("resolve")), ["alpha", "gamma"]);
    }

    #[test]
    fn package_glob_and_version_spec_match_on_name() {
        let ws = workspace(&["alpha", "beta", "gamma"]);
        let sel = Selection {
            packages: vec!["beta@0.1.0".to_owned(), "gam*".to_owned()],
            ..Selection::default()
        };
        assert_eq!(names(&sel.resolve(&ws).expect("resolve")), ["beta", "gamma"]);
    }

    #[test]
    fn exclude_removes_from_workspace() {
        let ws = workspace(&["alpha", "beta", "gamma"]);
        let sel = Selection {
            all: true,
            exclude: vec!["beta".to_owned()],
            ..Selection::default()
        };
        assert_eq!(names(&sel.resolve(&ws).expect("resolve")), ["alpha", "gamma"]);
    }

    #[test]
    fn unknown_selector_errors() {
        let ws = workspace(&["alpha"]);
        let sel = Selection {
            packages: vec!["nope-*".to_owned()],
            ..Selection::default()
        };
        let err = sel.resolve(&ws).expect_err("unknown selector must error");
        assert!(err.to_string().contains("nope-*"));
    }

    #[test]
    fn is_whole_workspace_detects_pass_through() {
        let all = Selection {
            all: true,
            ..Selection::default()
        };
        assert!(all.is_whole_workspace());
        let narrowed = Selection {
            all: true,
            exclude: vec!["beta".to_owned()],
            ..Selection::default()
        };
        assert!(!narrowed.is_whole_workspace());
    }

    #[test]
    fn glob_matcher_handles_wildcards() {
        assert!(glob_matches("alpha*", "alpha"));
        assert!(glob_matches("*macros", "alpha_macros"));
        assert!(glob_matches("a?pha", "alpha"));
        assert!(!glob_matches("alpha", "alphax"));
        assert!(!glob_matches("a?", "a"));
        // A `*` pattern whose trailing literal cannot be matched returns false.
        assert!(!glob_matches("a*b", "ac"));
    }
}
