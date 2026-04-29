// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File scanner for discovering source files in a project.
//!
//! Walks the project directory tree, collecting all supported source files
//! (`.rs` and `.toml`) while skipping build artifacts and hidden directories.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::comment::CommentStyle;
use crate::config::HeatherConfig;

/// Directories to always skip when scanning.
const SKIP_DIRS: &[&str] = &["target", ".git", ".github", ".vscode", ".idea", "node_modules"];

/// Discover all supported source files in the given project directory.
///
/// Finds files with extensions supported by [`CommentStyle`] (`.rs`, `.toml`).
/// Skips hidden directories, build artifacts (`target/`), and other
/// non-source directories. Returns a sorted list of absolute paths.
///
/// Uses the config to:
/// - Filter out TOML files starting with `.` (unless `config.dot_toml` is `true`)
/// - Apply the `config.exclude` list (relative paths from `project_dir`)
/// - Always exclude `exclude_path` (typically the config file itself)
#[must_use]
pub fn find_source_files(project_dir: &Path, exclude_path: Option<&Path>, config: &HeatherConfig) -> Vec<PathBuf> {
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
            // Skip the config file itself
            if let Some(ref excl) = exclude_canonical
                && std::fs::canonicalize(path).ok().as_ref() == Some(excl)
            {
                return false;
            }

            // Skip dot-TOML files unless config says otherwise
            if !config.dot_toml
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with('.')
                && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("toml"))
            {
                return false;
            }

            // Skip files in the exclude list
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

/// Returns `true` if a directory entry should be skipped entirely.
fn should_skip_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }

    let name = entry.file_name().to_string_lossy();

    // Skip hidden directories (except the root which might be ".")
    if entry.depth() > 0 && name.starts_with('.') {
        return true;
    }

    // Skip known non-source directories
    SKIP_DIRS.iter().any(|skip| name == *skip)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn default_config() -> HeatherConfig {
        HeatherConfig::with_defaults(String::new())
    }

    fn create_file(dir: &Path, relative: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, "// placeholder\n").unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn finds_rs_files_in_src() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "src/lib.rs");
        create_file(dir.path(), "src/utils/mod.rs");

        let files = find_source_files(dir.path(), None, &default_config());
        let rs_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")).count();
        assert_eq!(rs_count, 3);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn finds_toml_files() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Cargo.toml");
        create_file(dir.path(), "deny.toml");

        let files = find_source_files(dir.path(), None, &default_config());
        let toml_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "toml")).count();
        assert_eq!(toml_count, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn finds_both_rs_and_toml() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "Cargo.toml");

        let files = find_source_files(dir.path(), None, &default_config());
        assert_eq!(files.len(), 2);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn excludes_specified_file() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "Cargo.toml");
        create_file(dir.path(), ".cargo-heather.toml");

        let exclude = dir.path().join(".cargo-heather.toml");
        let files = find_source_files(dir.path(), Some(&exclude), &default_config());
        assert!(files.iter().all(|f| !f.ends_with(".cargo-heather.toml")));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn finds_rs_files_in_tests_and_examples() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "tests/test_one.rs");
        create_file(dir.path(), "examples/demo.rs");
        create_file(dir.path(), "benches/bench.rs");

        let files = find_source_files(dir.path(), None, &default_config());
        let rs_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")).count();
        assert_eq!(rs_count, 4);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn skips_target_directory() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "target/debug/build/generated.rs");

        let files = find_source_files(dir.path(), None, &default_config());
        let rs_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")).count();
        assert_eq!(rs_count, 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn skips_hidden_directories() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), ".hidden/something.rs");

        let files = find_source_files(dir.path(), None, &default_config());
        let rs_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")).count();
        assert_eq!(rs_count, 1);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn ignores_unsupported_files() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "src/readme.md");
        create_file(dir.path(), "data.json");

        let files = find_source_files(dir.path(), None, &default_config());
        assert!(files.iter().all(|f| {
            let ext = f.extension().and_then(|e| e.to_str());
            ext == Some("rs") || ext == Some("toml")
        }));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();
        let files = find_source_files(dir.path(), None, &default_config());
        assert!(files.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn returns_sorted_paths() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/z.rs");
        create_file(dir.path(), "src/a.rs");
        create_file(dir.path(), "src/m.rs");

        let files = find_source_files(dir.path(), None, &default_config());
        let names: Vec<&str> = files.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert_eq!(names, vec!["a.rs", "m.rs", "z.rs"]);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn finds_build_rs_at_root() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "build.rs");
        create_file(dir.path(), "src/main.rs");

        let files = find_source_files(dir.path(), None, &default_config());
        let rs_count = files.iter().filter(|f| f.extension().is_some_and(|e| e == "rs")).count();
        assert_eq!(rs_count, 2);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn should_skip_dir_skips_target() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();

        let walker = WalkDir::new(dir.path());
        for entry in walker {
            let entry = entry.unwrap();
            if entry.file_name() == "target" {
                assert!(should_skip_dir(&entry));
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn should_skip_dir_does_not_skip_src() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        let walker = WalkDir::new(dir.path());
        for entry in walker {
            let entry = entry.unwrap();
            if entry.file_name() == "src" {
                assert!(!should_skip_dir(&entry));
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn skips_dot_toml_by_default() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Cargo.toml");
        create_file(dir.path(), ".rustfmt.toml");
        create_file(dir.path(), ".cargo-heather.toml");

        let config = default_config();
        let files = find_source_files(dir.path(), None, &config);
        let names: Vec<&str> = files.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert!(names.contains(&"Cargo.toml"));
        assert!(!names.contains(&".rustfmt.toml"));
        assert!(!names.contains(&".cargo-heather.toml"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn includes_dot_toml_when_configured() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "Cargo.toml");
        create_file(dir.path(), ".rustfmt.toml");

        let mut config = default_config();
        config.dot_toml = true;
        let files = find_source_files(dir.path(), None, &config);
        let names: Vec<&str> = files.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert!(names.contains(&"Cargo.toml"));
        assert!(names.contains(&".rustfmt.toml"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn exclude_list_filters_files() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "src/generated.rs");
        create_file(dir.path(), "Cargo.toml");

        let mut config = default_config();
        config.exclude = vec!["src/generated.rs".to_owned()];
        let files = find_source_files(dir.path(), None, &config);
        let names: Vec<&str> = files.iter().filter_map(|p| p.file_name()?.to_str()).collect();
        assert!(names.contains(&"main.rs"));
        assert!(!names.contains(&"generated.rs"));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // filesystem access is not supported under Miri isolation
    fn exclude_list_filters_directories() {
        let dir = TempDir::new().unwrap();
        create_file(dir.path(), "src/main.rs");
        create_file(dir.path(), "vendor/lib.rs");
        create_file(dir.path(), "vendor/util.rs");

        let mut config = default_config();
        config.exclude = vec!["vendor".to_owned()];
        let files = find_source_files(dir.path(), None, &config);
        assert!(files.iter().all(|f| !f.to_string_lossy().contains("vendor")));
        assert_eq!(files.len(), 1);
    }
}
