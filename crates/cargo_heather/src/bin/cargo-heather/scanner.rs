// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File scanner for discovering source files in a project.
//!
//! Walks the project directory tree, collecting all supported source files
//! (`.rs` and `.toml`) while skipping build artifacts and hidden directories.

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
        .map(|rel| project_dir.join(rel))
        .filter_map(|p| std::fs::canonicalize(&p).ok())
        .collect();

    let mut files: Vec<PathBuf> = WalkDir::new(project_dir)
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
