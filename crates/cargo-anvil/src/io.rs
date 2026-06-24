// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trivial filesystem helpers shared across emit/plan paths.

use std::path::Path;

use ohno::{AppError, IntoAppError as _};

/// Read a file's contents to a string, returning `Ok(None)` if the file
/// does not exist. Any other I/O error is propagated as an `AppError`.
///
/// # Errors
///
/// Returns an error if reading the file fails for a reason other than
/// `NotFound` (e.g., permissions, invalid UTF-8).
#[mutants::skip] // Trivial `fs::read_to_string` + `NotFound` passthrough; mutations on its match guard / Ok arms are not behavior-meaningful and exhaustively exercised via every plan/emit path.
pub fn read_file_if_present(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err::<Option<String>, _>(e).into_app_err_with(|| format!("failed to read {}", path.display())),
    }
}

/// Resolve a repo-root-relative forward-slash path against the on-disk tree,
/// matching each path segment **ASCII**-case-insensitively and returning the
/// **actual** stored casing.
///
/// The catalog declares canonical paths (e.g. `Justfile`), but adopters may
/// already carry a different casing (`justfile`). Every file anvil touches is
/// therefore resolved to whatever is already on disk: an exact-case match
/// wins, otherwise an ASCII-case-insensitive match returns the real name, and
/// a segment that doesn't exist (a file anvil is about to create) keeps the
/// canonical casing. Returning the real on-disk name — rather than just
/// testing existence of the literal — is what keeps drift tracking correct on
/// case-insensitive filesystems, where the literal `Justfile` would otherwise
/// "exist" even when the file is named `justfile`.
///
/// ASCII case folding is sufficient and deliberate: every catalog path is a
/// fixed ASCII literal (`Justfile`, `Cargo.toml`, `deny.toml`, …), so there is
/// no non-ASCII segment for which Unicode case folding could matter.
#[must_use]
pub fn resolve_existing_case_insensitive(repo_root: &Path, relpath: &str) -> String {
    let segments: Vec<&str> = relpath.split('/').filter(|s| !s.is_empty()).collect();
    let mut resolved: Vec<String> = Vec::with_capacity(segments.len());

    for (index, segment) in segments.iter().enumerate() {
        let current_dir = repo_root.join(resolved.join("/"));
        if let Some(actual) = find_entry_case_insensitive(&current_dir, segment) {
            resolved.push(actual);
        } else {
            // This segment (and everything below it) isn't on disk yet;
            // keep the canonical casing for the remainder.
            resolved.extend(segments[index..].iter().map(|s| (*s).to_owned()));
            break;
        }
    }

    resolved.join("/")
}

/// Find a directory entry of `dir` whose name equals `name`, preferring an
/// exact-case match and falling back to an ASCII-case-insensitive one. Returns
/// the entry's real on-disk name.
fn find_entry_case_insensitive(dir: &Path, name: &str) -> Option<String> {
    let mut case_insensitive: Option<String> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let entry_name = entry.file_name().to_string_lossy().into_owned();
        if entry_name == name {
            return Some(entry_name);
        }
        if case_insensitive.is_none() && entry_name.eq_ignore_ascii_case(name) {
            case_insensitive = Some(entry_name);
        }
    }
    case_insensitive
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "").unwrap();
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn nonexistent_path_keeps_canonical_casing() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(resolve_existing_case_insensitive(tmp.path(), "Justfile"), "Justfile");
        assert_eq!(
            resolve_existing_case_insensitive(tmp.path(), "justfiles/anvil/mod.just"),
            "justfiles/anvil/mod.just"
        );
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn exact_match_is_returned() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "Justfile");
        assert_eq!(resolve_existing_case_insensitive(tmp.path(), "Justfile"), "Justfile");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn case_insensitive_match_returns_real_name() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "justfile");
        // Catalog asks for `Justfile`; the real on-disk name is `justfile`.
        assert_eq!(resolve_existing_case_insensitive(tmp.path(), "Justfile"), "justfile");
    }

    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    #[test]
    fn resolves_intermediate_directory_casing() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "Crates/alpha/Cargo.toml");
        // Existing dir `Crates` is reused even though the catalog said `crates`;
        // the not-yet-existing leaf keeps its canonical casing.
        assert_eq!(
            resolve_existing_case_insensitive(tmp.path(), "crates/alpha/new.toml"),
            "Crates/alpha/new.toml"
        );
    }
}
