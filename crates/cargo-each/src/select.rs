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

use cargo_metadata::semver::Version;

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
    ///
    /// `-p` / `--package` selectors do **not** disqualify it: when `--all` is
    /// set, [`resolve`](Self::resolve) ignores `packages` and returns the full
    /// workspace (matching `cargo build`, where `--workspace` wins over `-p`),
    /// so the resolved set is still the whole workspace.
    #[must_use]
    pub fn is_whole_workspace(&self) -> bool {
        self.all && self.exclude.is_empty() && !self.none
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
        // A `name@version` spec matches on the name (glob) *and*, when a
        // version is supplied, on the version per cargo's package-id-spec:
        // the qualifier may be partial, so `alpha@0.1` matches member `0.1.0`
        // but `alpha@9` does not (a loud error rather than silently resolving
        // to `alpha`). A bare name or glob matches any version.
        let (name_pat, version) = selector.split_once('@').map_or((selector.as_str(), None), |(n, v)| (n, Some(v)));
        let hits: Vec<&Member> = workspace
            .members
            .iter()
            .filter(|m| glob_matches(name_pat, &m.name) && version.is_none_or(|v| version_matches(v, &member_version(m))))
            .collect();
        if hits.is_empty() {
            return Err(UnknownSelectorError::new(selector.clone()).into());
        }
        for m in hits {
            matched.insert(m.name.as_str());
        }
    }
    Ok(workspace.members.iter().filter(|m| matched.contains(m.name.as_str())).collect())
}

/// Parse a member's version string into a [`Version`].
///
/// The string comes from `cargo metadata`, which only ever reports valid
/// semver, so the parse cannot fail in practice.
fn member_version(member: &Member) -> Version {
    member
        .version
        .parse()
        .expect("member versions come from cargo metadata and are always valid semver")
}

/// Whether a supplied `name@version` qualifier matches a member's version,
/// following cargo's package-id-spec `PartialVersion` semantics:
///
/// - The qualifier may omit trailing release components: `0.1` matches
///   `0.1.z` for any patch `z`, and `1` matches `1.y.z`. `0.30` does not
///   match `0.3.0` (component `30` is not `3`); more than three release
///   components never matches.
/// - A prerelease or build metadata is only valid on a *complete* version
///   (all three release components present); on a partial version it makes
///   the qualifier match nothing (e.g. `1.2-beta` and `1.2+build` never
///   match), matching cargo's rejection of those forms.
/// - Prerelease is significant: a qualifier without a prerelease matches only
///   versions that have none (so `1.2.3` does not match `1.2.3-beta`), and a
///   qualifier with a prerelease must match it exactly (`1.2.3-beta` does not
///   match `1.2.3-beta.1`).
/// - Build metadata, when supplied, is an exact constraint (`1.2.3+a` matches
///   only `1.2.3+a`, not `1.2.3+b`); when omitted, the member's build metadata
///   is ignored.
fn version_matches(supplied: &str, actual: &Version) -> bool {
    // Split off build metadata, then the optional prerelease.
    let (rest, build) = supplied.split_once('+').map_or((supplied, None), |(r, b)| (r, Some(b)));
    let (release, pre) = rest.split_once('-').map_or((rest, None), |(r, p)| (r, Some(p)));

    let components: Vec<&str> = release.split('.').collect();
    if components.is_empty() || components.len() > 3 {
        return false;
    }
    // A prerelease or build metadata is only meaningful on a full version.
    if (pre.is_some() || build.is_some()) && components.len() < 3 {
        return false;
    }
    let actual_release = [actual.major, actual.minor, actual.patch];
    for (i, component) in components.iter().enumerate() {
        if component.parse::<u64>() != Ok(actual_release[i]) {
            return false;
        }
    }
    // A supplied prerelease must match exactly; its absence requires the
    // member to have no prerelease either.
    let pre_ok = match pre {
        Some(pre) => actual.pre.as_str() == pre,
        None => actual.pre.is_empty(),
    };
    if !pre_ok {
        return false;
    }
    // Supplied build metadata is an exact constraint; omitting it ignores the
    // member's build metadata.
    match build {
        Some(build) => actual.build.as_str() == build,
        None => true,
    }
}

/// Tiny Unix-style glob matcher: `*` matches any run of characters
/// (including empty), `?` matches exactly one character. Everything else
/// matches literally.
///
/// Uses the standard iterative two-pointer algorithm with a single
/// backtrack point for the most recent `*`, so matching is linear-ish
/// (`O(len(pattern) * len(name))` worst case) rather than the exponential
/// blow-up a naive recursive backtracker exhibits on inputs like `*a*a*a…`.
#[mutants::skip] // Position-counter arithmetic mutants can loop forever; behavioral tests cover every observable case.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    // The pattern index of the last `*` seen, and the name index it was
    // matched against — the single point we backtrack to on a mismatch.
    let mut star: Option<usize> = None;
    let mut star_ni = 0usize;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ni = ni;
            pi += 1;
        } else if let Some(sp) = star {
            // Mismatch after a `*`: let that `*` absorb one more name char.
            pi = sp + 1;
            star_ni += 1;
            ni = star_ni;
        } else {
            return false;
        }
    }
    // Trailing `*`s in the pattern match the empty remainder.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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
    fn version_spec_must_match_member_version() {
        let ws = workspace(&["alpha", "beta", "gamma"]);
        // A wrong version selects nothing -> loud error (not a silent match).
        let sel = Selection {
            packages: vec!["beta@9.9.9".to_owned()],
            ..Selection::default()
        };
        let err = sel.resolve(&ws).expect_err("wrong version must error");
        assert!(err.to_string().contains("beta@9.9.9"));
        // The correct full version resolves the member.
        let ok = Selection {
            packages: vec!["beta@0.1.0".to_owned()],
            ..Selection::default()
        };
        assert_eq!(names(&ok.resolve(&ws).expect("resolve")), ["beta"]);
        // A partial version qualifier (cargo package-id-spec) also resolves.
        let partial = Selection {
            packages: vec!["beta@0.1".to_owned()],
            ..Selection::default()
        };
        assert_eq!(names(&partial.resolve(&ws).expect("resolve")), ["beta"]);
    }

    #[test]
    fn version_matches_follows_cargo_partial_version_rule() {
        fn ver(s: &str) -> Version {
            s.parse().expect("valid semver")
        }
        // Exact and partial release matches on a plain version.
        assert!(version_matches("0.1.0", &ver("0.1.0"))); // exact
        assert!(version_matches("0.1", &ver("0.1.0"))); // partial prefix
        assert!(version_matches("0", &ver("0.1.0"))); // single component
        assert!(!version_matches("0.30", &ver("0.3.0"))); // component 30 is not 3
        assert!(!version_matches("0.2", &ver("0.1.0"))); // mismatch
        assert!(!version_matches("0.1.0.0", &ver("0.1.0"))); // too many components
        assert!(!version_matches("1.x", &ver("1.0.0"))); // non-numeric component
        // Prerelease is significant; a supplied prerelease must match exactly.
        assert!(version_matches("1.2.3-beta.1", &ver("1.2.3-beta.1+build"))); // exact pre, build not constrained
        assert!(!version_matches("1.2", &ver("1.2.3-beta.1"))); // partial does not match a prerelease
        assert!(!version_matches("1.2.3-beta", &ver("1.2.3-beta.1"))); // prerelease must match exactly
        assert!(!version_matches("1.2.3", &ver("1.2.3-beta"))); // no-pre qualifier rejects a prerelease
        assert!(version_matches("1.2.3", &ver("1.2.3"))); // no-pre matches no-pre
        // Build metadata: exact constraint when supplied, ignored when omitted.
        assert!(version_matches("1.2.3+build", &ver("1.2.3+build"))); // exact build match
        assert!(!version_matches("1.2.3+wrong", &ver("1.2.3+build"))); // build mismatch
        assert!(version_matches("1.2.3", &ver("1.2.3+build"))); // omitted build ignores member build
        // Prerelease / build metadata are invalid on a partial version. Use
        // members whose prerelease/build would otherwise match, so it is the
        // partial-version guard (not a downstream mismatch) that rejects them.
        assert!(!version_matches("1.2-beta", &ver("1.2.0-beta"))); // partial + prerelease
        assert!(!version_matches("1.2+build", &ver("1.2.0+build"))); // partial + build metadata
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
        // `--all` combined with `-p` is still the whole workspace: resolve()
        // ignores packages when all is set, so {packages} must emit --workspace.
        let all_with_pkg = Selection {
            all: true,
            packages: vec!["alpha".to_owned()],
            ..Selection::default()
        };
        assert!(all_with_pkg.is_whole_workspace());
        let narrowed = Selection {
            all: true,
            exclude: vec!["beta".to_owned()],
            ..Selection::default()
        };
        assert!(!narrowed.is_whole_workspace());
        // --none is never the whole workspace even with --all.
        let none = Selection {
            all: true,
            none: true,
            ..Selection::default()
        };
        assert!(!none.is_whole_workspace());
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
        // Additional cases exercising the iterative backtrack.
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("**", "anything"));
        assert!(glob_matches("a*b*c", "axxbyyc"));
        assert!(glob_matches("*a*a*a", "banana_aaa"));
        assert!(!glob_matches("a*b*c", "axxbyy"));
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "x"));
        assert!(glob_matches("*", ""));
    }
}
