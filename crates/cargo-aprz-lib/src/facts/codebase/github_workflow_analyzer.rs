// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Detector for CI tool usage in GitHub Actions CI workflows.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use ohno::IntoAppError;

use super::provider::LOG_TARGET;
use crate::Result;

const WALKING_WORKFLOWS_CONTEXT: &str = "walking workflows directory";

#[derive(Debug, Default, Clone)]
pub struct GitHubWorkflowInfo {
    pub workflows_detected: bool,
    pub clippy_detected: bool,
    pub miri_detected: bool,
}

/// Detect if Miri and Clippy are mentioned in GitHub Actions CI
pub fn sniff_github_workflows(repo_path: impl AsRef<Path>) -> Result<GitHubWorkflowInfo> {
    let workflows_dir = repo_path.as_ref().join(".github").join("workflows");
    if !workflows_dir.exists() {
        return Ok(GitHubWorkflowInfo::default());
    }

    // #[gamma::skip(literal.bool_flip, reason = "symlink traversal policy belongs to the external filesystem walker adapter")]
    let entries = walkdir::WalkDir::new(&workflows_dir).follow_links(false);
    scan_workflow_entries(
        &workflows_dir,
        entries
            .into_iter()
            .map(|entry| entry.map(|entry| (entry.path().to_owned(), entry.file_type().is_dir()))),
        |path| fs::File::open(path).map(BufReader::new),
    )
}

fn scan_workflow_entries<R: BufRead>(
    workflows_dir: &Path,
    entries: impl IntoIterator<Item = std::result::Result<(PathBuf, bool), impl std::error::Error + Send + Sync + 'static>>,
    mut open: impl FnMut(&Path) -> std::io::Result<R>,
) -> Result<GitHubWorkflowInfo> {
    const MAX_WORKFLOW_FILES: usize = 100;
    let mut usage = GitHubWorkflowInfo {
        workflows_detected: true,
        ..GitHubWorkflowInfo::default()
    };
    let mut file_count = 0;
    for entry_result in entries {
        // #[gamma::skip(try.propagate_to_unwrap, reason = "filesystem walk failures must be propagated rather than converted into process panics")]
        let (path, is_directory) = entry_result.into_app_err(WALKING_WORKFLOWS_CONTEXT)?;

        // Skip directories
        if is_directory {
            continue;
        }

        // Check for YAML extension
        let is_yaml = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext == "yml" || ext == "yaml");

        if !is_yaml {
            continue;
        }

        file_count += 1;
        if file_count > MAX_WORKFLOW_FILES {
            log::warn!(target: LOG_TARGET, "Workflow file count limit ({MAX_WORKFLOW_FILES}) exceeded in directory '{}', stopping scan", workflows_dir.display());
            break;
        }

        // #[gamma::skip(try.propagate_to_unwrap, reason = "opening a workflow file is a filesystem adapter boundary whose errors must remain recoverable")]
        let mut reader = open(&path).into_app_err_with(|| format!("opening workflow file '{}'", path.display()))?;

        // The line buffer is reused and matched case-insensitively in place, so scanning a
        // workflow file allocates nothing per line.
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                // #[gamma::skip(loop.break_to_continue, tag = "timeout", reason = "continuing at EOF repeatedly reads zero bytes without advancing")]
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }

            if contains_ignore_ascii_case(&line, "miri") {
                usage.miri_detected = true;
            }

            if contains_ignore_ascii_case(&line, "clippy") {
                usage.clippy_detected = true;
            }

            if usage.miri_detected && usage.clippy_detected {
                return Ok(usage);
            }
        }
    }

    Ok(usage)
}

/// Case-insensitive substring search for an ASCII needle, without allocating.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn matches_needles_regardless_of_case() {
        assert!(contains_ignore_ascii_case("run: cargo MIRI test", "miri"));
        assert!(contains_ignore_ascii_case("Clippy", "clippy"));
        assert!(contains_ignore_ascii_case("miri", "miri"));
        assert!(!contains_ignore_ascii_case("mir", "miri"));
        assert!(!contains_ignore_ascii_case("", "miri"));
        assert!(!contains_ignore_ascii_case("miri", ""));
    }

    #[test]
    fn workflow_walking_context_is_exact() {
        assert_eq!(WALKING_WORKFLOWS_CONTEXT, "walking workflows directory");
    }

    #[test]
    fn line_buffer_is_cleared_between_reads() {
        let result = scan_workflow_entries(
            Path::new("workflows"),
            [Ok::<_, std::io::Error>((PathBuf::from("ci.yml"), false))],
            |_| Ok(std::io::Cursor::new("run: cargo cli\nppy\n")),
        )
        .unwrap();

        assert!(!result.clippy_detected, "text split across separate lines is not a tool name");
    }

    #[test]
    fn tools_are_detected_in_either_line_order_without_scanning_yaml_named_directories_as_files() {
        for contents in [
            "run: cargo miri test\nrun: cargo clippy\n",
            "run: cargo clippy\nrun: cargo miri test\n",
        ] {
            let result = scan_workflow_entries(
                Path::new("workflows"),
                [
                    Ok::<_, std::io::Error>((PathBuf::from("nested.yaml"), true)),
                    Ok((PathBuf::from("ci.yml"), false)),
                ],
                |path| {
                    assert_eq!(path, Path::new("ci.yml"));
                    Ok(std::io::Cursor::new(contents))
                },
            )
            .unwrap();
            assert_eq!(
                (result.workflows_detected, result.miri_detected, result.clippy_detected),
                (true, true, true)
            );
        }
    }

    #[test]
    fn workflow_scan_skips_non_yaml_and_stops_before_later_open_errors() {
        for (contents, miri, clippy) in [
            ("run: cargo miri test\n", true, false),
            ("run: cargo clippy\n", false, true),
            ("run: cargo miri test\nrun: cargo clippy\n", true, true),
        ] {
            let entries = [(PathBuf::from("notes.txt"), false), (PathBuf::from("ci.yml"), false)];
            let open = |path: &Path| {
                assert_eq!(path, Path::new("ci.yml"));
                Ok(std::io::Cursor::new(contents))
            };
            let result = scan_workflow_entries(
                Path::new("workflows"),
                entries.clone().into_iter().map(Ok::<_, std::io::Error>),
                open,
            )
            .unwrap();
            assert_eq!((result.miri_detected, result.clippy_detected), (miri, clippy));
            if miri && clippy {
                let result = scan_workflow_entries(
                    Path::new("workflows"),
                    entries
                        .into_iter()
                        .chain([(PathBuf::from("later.yml"), false)])
                        .map(Ok::<_, std::io::Error>),
                    open,
                )
                .unwrap();
                assert!(result.miri_detected && result.clippy_detected);
            }
        }
        let error = scan_workflow_entries(
            Path::new("workflows"),
            [Ok::<_, std::io::Error>((PathBuf::from("missing.yml"), false))],
            |path| {
                assert_eq!(path, Path::new("missing.yml"));
                Err::<std::io::Cursor<&str>, _>(std::io::Error::from(std::io::ErrorKind::NotFound))
            },
        )
        .unwrap_err();
        assert_eq!(
            error.source().unwrap().downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn reaching_the_workflow_limit_stops_before_later_walk_errors() {
        let entries = (0..101)
            .map(|number| Ok((PathBuf::from(format!("workflow-{number}.yml")), false)))
            .chain(std::iter::once(Err(std::io::Error::other("unvisited walk failure"))));
        let mut opened = 0;

        let result = scan_workflow_entries(Path::new("workflows"), entries, |_| {
            opened += 1;
            Ok(std::io::Cursor::new("run: cargo test\n"))
        })
        .expect("the walk after the file limit must not be evaluated");

        assert_eq!(opened, 100);
        assert_eq!(
            (result.workflows_detected, result.miri_detected, result.clippy_detected),
            (true, false, false)
        );
    }

    #[test]
    fn scanning_reuses_a_line_buffer_without_retaining_processed_lines() {
        struct ObservingReader<'a> {
            inner: std::io::Cursor<&'a [u8]>,
            lengths: &'a std::cell::RefCell<Vec<usize>>,
        }

        impl std::io::Read for ObservingReader<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.inner.read(buf)
            }
        }

        impl BufRead for ObservingReader<'_> {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                self.inner.fill_buf()
            }

            fn consume(&mut self, amount: usize) {
                self.inner.consume(amount);
            }

            fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
                self.lengths.borrow_mut().push(buf.len());
                self.inner.read_line(buf)
            }
        }

        let lengths = std::cell::RefCell::new(Vec::new());
        let entries = [Ok::<_, std::io::Error>((PathBuf::from("ci.yml"), false))];
        let result = scan_workflow_entries(Path::new("workflows"), entries, |_| {
            Ok(ObservingReader {
                inner: std::io::Cursor::new(b"run: cargo test\nrun: cargo build\n".as_slice()),
                lengths: &lengths,
            })
        })
        .expect("the workflow is readable");

        assert_eq!(*lengths.borrow(), [0, 0, 0]);
        assert!(!result.miri_detected && !result.clippy_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_no_workflows_directory() {
        let temp_dir = tempfile::tempdir().unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(!result.workflows_detected);
        assert!(!result.miri_detected);
        assert!(!result.clippy_detected);
    }

    #[test]
    fn yaml_outside_the_workflows_directory_is_ignored() {
        let temp_dir = tempfile::tempdir().unwrap();
        let github_dir = temp_dir.path().join(".github");
        fs::create_dir_all(&github_dir).unwrap();
        fs::write(github_dir.join("not-a-workflow.yml"), "run: cargo clippy\n").unwrap();
        let result = sniff_github_workflows(temp_dir.path()).unwrap();
        assert!(!result.workflows_detected);
        assert!(!result.clippy_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_empty_workflows_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(!result.miri_detected);
        assert!(!result.clippy_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_workflows_with_clippy() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        let workflow_file = workflows_dir.join("ci.yml");
        fs::write(
            &workflow_file,
            "
name: CI
on: [push]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - name: Run clippy
        run: cargo clippy -- -D warnings
",
        )
        .unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(result.clippy_detected);
        assert!(!result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_workflows_with_miri() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        let workflow_file = workflows_dir.join("miri.yaml");
        fs::write(
            &workflow_file,
            "
name: Miri
on: [push]
jobs:
  miri:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - name: Run Miri
        run: cargo +nightly miri test
",
        )
        .unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(!result.clippy_detected);
        assert!(result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_workflows_with_both() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        let workflow_file = workflows_dir.join("ci.yml");
        fs::write(
            &workflow_file,
            "
name: CI
on: [push]
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - name: Run Clippy
        run: cargo clippy
  miri:
    runs-on: ubuntu-latest
    steps:
      - name: Run Miri
        run: cargo +nightly miri test
",
        )
        .unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(result.clippy_detected);
        assert!(result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_case_insensitive_detection() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        let workflow_file = workflows_dir.join("ci.yml");
        fs::write(
            &workflow_file,
            "
name: CI
steps:
  - name: Run CLIPPY in uppercase
    run: cargo CLIPPY
  - name: Run MiRi in mixed case
    run: cargo MiRi test
",
        )
        .unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.clippy_detected);
        assert!(result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_multiple_workflow_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        // First file with clippy
        fs::write(workflows_dir.join("clippy.yml"), "run: cargo clippy").unwrap();

        // Second file with miri
        fs::write(workflows_dir.join("miri.yaml"), "run: cargo miri test").unwrap();

        // Third file with neither
        fs::write(workflows_dir.join("test.yml"), "run: cargo test").unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(result.clippy_detected);
        assert!(result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_non_yaml_files_ignored() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        // Create a non-YAML file with clippy/miri mentions
        fs::write(workflows_dir.join("README.md"), "This mentions clippy and miri").unwrap();

        // Create a YAML file without mentions
        fs::write(workflows_dir.join("ci.yml"), "run: cargo test").unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        // README.md should be ignored
        assert!(!result.clippy_detected);
        assert!(!result.miri_detected);
    }

    #[test]
    fn test_github_workflow_info_default() {
        let info = GitHubWorkflowInfo::default();
        assert!(!info.workflows_detected);
        assert!(!info.clippy_detected);
        assert!(!info.miri_detected);
    }

    #[test]
    fn test_github_workflow_info_clone() {
        let info1 = GitHubWorkflowInfo {
            workflows_detected: true,
            clippy_detected: true,
            miri_detected: false,
        };

        let info2 = info1.clone();

        assert_eq!(info1.workflows_detected, info2.workflows_detected);
        assert_eq!(info1.clippy_detected, info2.clippy_detected);
        assert_eq!(info1.miri_detected, info2.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_malformed_yaml_is_still_scanned() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        // The scan is line based, so a file that no YAML parser would accept is
        // still searched for tool mentions.
        fs::write(workflows_dir.join("broken.yml"), "jobs: [ unbalanced\n\trun: cargo clippy\n:::\n").unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(result.clippy_detected);
        assert!(!result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_nested_workflow_directories_are_scanned() {
        let temp_dir = tempfile::tempdir().unwrap();
        let nested = temp_dir.path().join(".github").join("workflows").join("shared");
        fs::create_dir_all(&nested).unwrap();

        fs::write(nested.join("lint.yml"), "run: cargo clippy").unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(result.clippy_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_workflow_file_limit_stops_the_scan() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        // The scan stops after 100 files, so the extra ones are never opened.
        for i in 0..150 {
            fs::write(workflows_dir.join(format!("wf-{i:03}.yml")), "run: cargo build").unwrap();
        }

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(!result.clippy_detected);
        assert!(!result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_hundredth_workflow_file_is_still_scanned() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        for i in 0..100 {
            fs::write(workflows_dir.join(format!("wf-{i:03}.yml")), "run: cargo test").unwrap();
        }
        let yaml_paths = workflow_yaml_paths_in_scan_order(&workflows_dir);
        fs::write(&yaml_paths[99], "run: cargo miri test").unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(result.miri_detected);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn test_workflow_file_after_limit_is_not_scanned() {
        let temp_dir = tempfile::tempdir().unwrap();
        let workflows_dir = temp_dir.path().join(".github").join("workflows");
        fs::create_dir_all(&workflows_dir).unwrap();

        for i in 0..101 {
            fs::write(workflows_dir.join(format!("wf-{i:03}.yml")), "run: cargo test").unwrap();
        }
        let yaml_paths = workflow_yaml_paths_in_scan_order(&workflows_dir);
        fs::write(&yaml_paths[100], "run: cargo clippy").unwrap();

        let result = sniff_github_workflows(temp_dir.path()).unwrap();

        assert!(result.workflows_detected);
        assert!(!result.clippy_detected);
    }

    fn workflow_yaml_paths_in_scan_order(workflows_dir: &Path) -> Vec<std::path::PathBuf> {
        walkdir::WalkDir::new(workflows_dir)
            .follow_links(false)
            .into_iter()
            .map(|entry| entry.unwrap())
            .filter(|entry| !entry.file_type().is_dir())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_some_and(|ext| ext == "yml" || ext == "yaml")
            })
            .map(|entry| entry.path().to_path_buf())
            .collect()
    }
}
