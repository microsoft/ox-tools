// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File scanner for discovering source files in a project.
//!
//! Walks the project directory tree, collecting all supported source files
//! while skipping build artifacts and hidden directories.

use std::path::{Path, PathBuf};

use cargo_heather::CommentStyle;
use walkdir::WalkDir;

use crate::config::HeatherConfig;

/// Directories to always skip when scanning.
const SKIP_DIRS: &[&str] = &["target", ".git", ".github", ".vscode", ".idea", "node_modules"];

/// Discover all supported source files in the given project directory.
pub(crate) fn find_source_files(project_dir: &Path, exclude_path: Option<&Path>, config: &HeatherConfig) -> Vec<PathBuf> {
    let exclude_canonical = exclude_path.and_then(|p| std::fs::canonicalize(p).ok());

    let exclude_list: Vec<PathBuf> = config
        .exclude
        .iter()
        .filter_map(|rel| {
            let full = project_dir.join(rel);
            std::fs::canonicalize(&full)
                .inspect_err(|err| {
                    println!("  Warning: exclude entry '{rel}' could not be resolved and will be ignored: {err}");
                })
                .ok()
        })
        .collect();

    let files: Vec<PathBuf> = WalkDir::new(project_dir)
        .into_iter()
        .filter_entry(|entry| !should_skip_dir(entry))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && CommentStyle::from_path(entry.path()).is_some())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| {
            if let Some(ref excl) = exclude_canonical
                && std::fs::canonicalize(path).ok().as_ref() == Some(excl)
            {
                return false;
            }

            if !config.dot_toml
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with('.')
                && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            {
                return false;
            }

            if !exclude_list.is_empty()
                && let Ok(canonical) = std::fs::canonicalize(path)
                && exclude_list.iter().any(|excl| canonical == *excl || canonical.starts_with(excl))
            {
                return false;
            }

            true
        })
        .collect();

    sort_files(files)
}

fn sort_files(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    files.sort();
    files
}

fn should_skip_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }

    let name = entry.file_name().to_string_lossy();

    if entry.depth() > 0 && name.starts_with('.') {
        return true;
    }

    SKIP_DIRS.iter().any(|skip| name == *skip)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn config(exclude: Vec<String>) -> HeatherConfig {
        HeatherConfig {
            header_text: "header".to_owned(),
            scripts: true,
            dot_toml: false,
            exclude,
        }
    }

    #[test]
    fn sort_files_orders_paths() {
        assert_eq!(
            sort_files(vec!["z.rs".into(), "a.rs".into(), "m.rs".into()]),
            vec![PathBuf::from("a.rs"), PathBuf::from("m.rs"), PathBuf::from("z.rs")]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    fn non_directory_entries_are_not_skipped() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("target");
        std::fs::write(&path, "file").unwrap();
        let entry = WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| entry.path() == path)
            .unwrap();

        assert!(!should_skip_dir(&entry));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem; miri isolation forbids it")]
    fn any_matching_exclusion_removes_a_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("included")).unwrap();
        std::fs::create_dir_all(tmp.path().join("excluded")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other-exclusion")).unwrap();
        std::fs::write(tmp.path().join("included/a.rs"), "").unwrap();
        std::fs::write(tmp.path().join("excluded/b.rs"), "").unwrap();
        let cfg = config(vec!["other-exclusion".to_owned(), "excluded".to_owned()]);

        let files = find_source_files(tmp.path(), None, &cfg);

        assert_eq!(files, vec![tmp.path().join("included/a.rs")]);
    }
}
