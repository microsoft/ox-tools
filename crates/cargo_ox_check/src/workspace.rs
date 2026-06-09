// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Repo discovery and Cargo workspace introspection.
//!
//! `cargo-ox-check` operates from the *workspace root*: the directory whose
//! `Cargo.toml` contains a `[workspace]` table (or a single-crate `[package]`
//! that is implicitly its own workspace of one). This module locates that
//! directory by walking up from a starting path, then enumerates the
//! workspace members so emitters can write per-crate managed regions.
//!
//! See [`design.md §6`](../../docs/design/design.md) for the file layout.

use std::path::{Component, Path, PathBuf};

use ohno::{AppError, IntoAppError as _, app_err, bail};
use toml_edit::DocumentMut;

/// A discovered Cargo workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Absolute path to the workspace root directory.
    pub root: PathBuf,
    /// Workspace members. Always at least one entry (the root crate, for
    /// single-crate repos that don't declare `[workspace]`, or each explicit
    /// member otherwise).
    pub members: Vec<WorkspaceMember>,
    /// Whether the root `Cargo.toml` carries a `[workspace]` table.
    ///
    /// Used by the emitter for [`design.md §6`]: multi-crate workspaces get
    /// `[workspace.lints]` with the catalog plus `[lints] workspace = true`
    /// in each member; single-crate repos get the catalog directly in
    /// `[lints]`.
    ///
    /// [`design.md §6`]: ../../docs/design/design.md
    pub has_workspace_table: bool,
}

/// One workspace member.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceMember {
    /// Path to the member's `Cargo.toml`, relative to the workspace root,
    /// with forward slashes.
    pub manifest_relpath: String,
}

/// Find the workspace root by walking up from `start`.
///
/// Returns the first ancestor directory whose `Cargo.toml` declares a
/// `[workspace]` table. If no such ancestor exists, falls back to the
/// nearest `Cargo.toml` (single-crate repo).
///
/// # Errors
///
/// Returns an error if no `Cargo.toml` is found at or above `start`.
pub fn find_workspace_root(start: &Path) -> Result<PathBuf, AppError> {
    let start = start
        .canonicalize()
        .into_app_err_with(|| format!("cannot canonicalize start path '{}'", start.display()))?;

    let mut nearest: Option<PathBuf> = None;
    for ancestor in start.ancestors() {
        let manifest = ancestor.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest).into_app_err_with(|| format!("failed to read {}", manifest.display()))?;
        let doc: DocumentMut = text
            .parse::<DocumentMut>()
            .into_app_err_with(|| format!("failed to parse {} as TOML", manifest.display()))?;
        if doc.get("workspace").is_some() {
            return Ok(ancestor.to_path_buf());
        }
        if nearest.is_none() {
            nearest = Some(ancestor.to_path_buf());
        }
    }

    nearest.ok_or_else(|| {
        app_err!(
            "no Cargo.toml found at or above '{}'; cargo-ox-check must be run inside a Cargo workspace",
            start.display()
        )
    })
}

/// Load and parse the workspace at `root`.
///
/// # Errors
///
/// Returns an error if the manifest can't be read, parsed, or its
/// `[workspace] members` glob list can't be resolved.
pub fn load_workspace(root: &Path) -> Result<Workspace, AppError> {
    let manifest_path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path).into_app_err_with(|| format!("failed to read {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse::<DocumentMut>()
        .into_app_err_with(|| format!("failed to parse {} as TOML", manifest_path.display()))?;

    let has_workspace_table = doc.get("workspace").is_some();
    let members = if has_workspace_table {
        resolve_workspace_members(root, &doc)?
    } else if doc.get("package").is_some() {
        vec![WorkspaceMember {
            manifest_relpath: "Cargo.toml".to_owned(),
        }]
    } else {
        bail!(
            "{} has neither [workspace] nor [package] — not a recognizable Cargo manifest",
            manifest_path.display()
        );
    };

    if members.is_empty() {
        bail!(
            "workspace at {} resolved to zero members; check `members` in {}",
            root.display(),
            manifest_path.display()
        );
    }

    Ok(Workspace {
        root: root.to_path_buf(),
        members,
        has_workspace_table,
    })
}

fn resolve_workspace_members(root: &Path, doc: &DocumentMut) -> Result<Vec<WorkspaceMember>, AppError> {
    let members_item = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .ok_or_else(|| app_err!("[workspace] is missing the `members` array"))?;

    let array = members_item
        .as_array()
        .ok_or_else(|| app_err!("[workspace] `members` must be an array"))?;

    let mut out = Vec::new();
    for entry in array {
        let pattern = entry.as_str().ok_or_else(|| app_err!("`members` entries must be strings"))?;
        expand_member_pattern(root, pattern, &mut out)?;
    }

    // De-duplicate by relpath while preserving discovery order.
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|m| seen.insert(m.manifest_relpath.clone()));
    Ok(out)
}

/// Expand one entry from `members = [...]`.
///
/// Supports literal directory names and a single trailing `*` glob in the
/// last path segment (the form Cargo uses in the surveyed repos: `crates/*`).
/// More elaborate globbing isn't observed in the wild and can be added
/// later if needed.
fn expand_member_pattern(root: &Path, pattern: &str, out: &mut Vec<WorkspaceMember>) -> Result<(), AppError> {
    let pattern = pattern.trim_end_matches('/');
    if pattern.is_empty() {
        bail!("workspace member pattern is empty");
    }

    if let Some(parent) = pattern.strip_suffix("/*") {
        let parent_path = root.join(parent);
        if !parent_path.is_dir() {
            // Pattern with no matches is not an error — Cargo itself tolerates this.
            return Ok(());
        }
        let mut entries: Vec<_> = std::fs::read_dir(&parent_path)
            .into_app_err_with(|| format!("failed to read directory {}", parent_path.display()))?
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .filter(|e| e.path().join("Cargo.toml").is_file())
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let name_str = name
                .to_str()
                .ok_or_else(|| app_err!("non-UTF-8 directory name in {}", parent_path.display()))?;
            let relpath = format!("{parent}/{name_str}/Cargo.toml");
            out.push(WorkspaceMember {
                manifest_relpath: normalize_relpath(&relpath),
            });
        }
        return Ok(());
    }

    if pattern.contains('*') {
        bail!("unsupported glob pattern in workspace members: '{pattern}' (only a trailing '/*' is supported)");
    }

    let manifest = root.join(pattern).join("Cargo.toml");
    if !manifest.is_file() {
        bail!("workspace member '{pattern}' has no Cargo.toml at {}", manifest.display());
    }
    out.push(WorkspaceMember {
        manifest_relpath: normalize_relpath(&format!("{pattern}/Cargo.toml")),
    });
    Ok(())
}

fn normalize_relpath(relpath: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for component in Path::new(relpath).components() {
        if let Component::Normal(s) = component
            && let Some(s) = s.to_str()
        {
            parts.push(s);
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovers_single_crate_workspace() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[package]
name = "demo"
version = "0.1.0"
"#,
        );
        write(&root.join("src/lib.rs"), "");

        let found = find_workspace_root(&root.join("src")).unwrap();
        assert_eq!(found.canonicalize().unwrap(), root.canonicalize().unwrap());

        let ws = load_workspace(&found).unwrap();
        assert!(!ws.has_workspace_table);
        assert_eq!(
            ws.members,
            vec![WorkspaceMember {
                manifest_relpath: "Cargo.toml".into()
            }]
        );
    }

    #[test]
    fn discovers_glob_workspace() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[workspace]
resolver = "2"
members = ["crates/*"]
"#,
        );
        write(&root.join("crates/alpha/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");
        write(&root.join("crates/beta/Cargo.toml"), "[package]\nname='b'\nversion='0.1.0'\n");
        // Non-crate directory in the glob target — should be ignored.
        write(&root.join("crates/notacrate/README.md"), "no manifest");

        let ws = load_workspace(root).unwrap();
        assert!(ws.has_workspace_table);
        let paths: Vec<_> = ws.members.iter().map(|m| m.manifest_relpath.as_str()).collect();
        assert_eq!(paths, vec!["crates/alpha/Cargo.toml", "crates/beta/Cargo.toml"]);
    }

    #[test]
    fn discovers_explicit_members() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[workspace]
members = ["alpha", "nested/beta"]
"#,
        );
        write(&root.join("alpha/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");
        write(&root.join("nested/beta/Cargo.toml"), "[package]\nname='b'\nversion='0.1.0'\n");

        let ws = load_workspace(root).unwrap();
        let paths: Vec<_> = ws.members.iter().map(|m| m.manifest_relpath.as_str()).collect();
        assert_eq!(paths, vec!["alpha/Cargo.toml", "nested/beta/Cargo.toml"]);
    }

    #[test]
    fn walks_up_to_workspace_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/alpha"]
"#,
        );
        write(&root.join("crates/alpha/Cargo.toml"), "[package]\nname='a'\nversion='0.1.0'\n");
        write(&root.join("crates/alpha/src/lib.rs"), "");

        // Starting deep inside a member should still find the workspace root.
        let found = find_workspace_root(&root.join("crates/alpha/src")).unwrap();
        assert_eq!(found.canonicalize().unwrap(), root.canonicalize().unwrap());
    }

    #[test]
    fn errors_when_no_cargo_toml_above() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("foo/bar.txt"), "");
        let err = find_workspace_root(&root.join("foo")).unwrap_err();
        assert!(err.to_string().contains("no Cargo.toml"));
    }

    #[test]
    fn errors_when_explicit_member_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[workspace]
members = ["alpha"]
"#,
        );
        let err = load_workspace(root).unwrap_err();
        assert!(err.to_string().contains("has no Cargo.toml"));
    }

    #[test]
    fn unsupported_glob_pattern_errors() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[workspace]
members = ["cra*tes"]
"#,
        );
        let err = load_workspace(root).unwrap_err();
        assert!(err.to_string().contains("unsupported glob pattern"));
    }

    #[test]
    fn manifest_without_workspace_or_package_errors() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("Cargo.toml"), "# empty\n");
        let err = load_workspace(root).unwrap_err();
        assert!(err.to_string().contains("neither [workspace] nor [package]"));
    }

    #[test]
    fn normalize_relpath_uses_forward_slashes() {
        assert_eq!(normalize_relpath("a/b/c"), "a/b/c");
        assert_eq!(normalize_relpath("a\\b\\c"), if cfg!(windows) { "a/b/c" } else { "a\\b\\c" });
    }
}
