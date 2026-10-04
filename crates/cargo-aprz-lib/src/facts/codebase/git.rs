// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use ohno::{IntoAppError, bail};
use tokio::process::Command;
use url::Url;

use super::provider::LOG_TARGET;
use crate::Result;

pub(super) const GIT_TIMEOUT: Duration = Duration::from_mins(5);
const SECONDS_PER_DAY: i64 = 86_400;

/// Convert a path to a UTF-8 string, returning an error if the path contains invalid UTF-8.
fn path_str(path: &Path) -> Result<&str> {
    path.to_str().into_app_err("invalid UTF-8 in repository path")
}

/// Result of a repository sync operation.
#[derive(Debug)]
pub enum RepoStatus {
    /// Repository was successfully cloned or updated.
    Ok,
    /// Repository does not exist on the remote.
    NotFound,
}

/// Clone or update a git repository
pub async fn get_repo(repo_path: &Path, repo_url: &Url, timeout: Duration) -> Result<RepoStatus> {
    let start_time = std::time::Instant::now();

    let status = get_repo_core(repo_path, repo_url, timeout).await?;

    match &status {
        RepoStatus::Ok => {
            log::debug!(target: LOG_TARGET, "Successfully prepared cached repository from '{repo_url}' in {:.3}s", start_time.elapsed().as_secs_f64());
        }
        RepoStatus::NotFound => {}
    }

    Ok(status)
}

async fn get_repo_core(repo_path: &Path, repo_url: &Url, timeout: Duration) -> Result<RepoStatus> {
    get_repo_core_with_executable(repo_path, repo_url, timeout, Path::new("git")).await
}

async fn get_repo_core_with_executable(repo_path: &Path, repo_url: &Url, timeout: Duration, git_executable: &Path) -> Result<RepoStatus> {
    let path_str = path_str(repo_path)?;

    if !repo_path.exists() {
        if let Some(parent) = repo_path.parent() {
            fs::create_dir_all(parent).into_app_err_with(|| format!("creating directory '{}'", parent.display()))?;
        }

        return clone_repo_with_executable(path_str, repo_url, timeout, git_executable).await;
    }

    // Verify it's a valid git repository before attempting update
    if !repo_path.join(".git").exists() {
        log::warn!(target: LOG_TARGET, "Cached repository path '{path_str}' exists but .git directory missing, re-cloning");
        // #[gamma::skip(try.propagate_to_unwrap, reason = "filesystem adapter errors must be propagated rather than converted into process panics")]
        remove_cached_repo(repo_path, &format!("removing potentially corrupt cached repository '{path_str}'"))?;
        return clone_repo_with_executable(path_str, repo_url, timeout, git_executable).await;
    }

    log::info!(target: LOG_TARGET, "Syncing repository '{repo_url}'");

    // First, try to fetch new commits
    // --filter=blob:none downloads only commit/tree objects, not file contents
    // --prune removes refs that no longer exist on remote
    // --force allows updating refs even if they're not fast-forward
    let output = run_git_executable_with_timeout(
        git_executable,
        &["-C", path_str, "fetch", "origin", "--filter=blob:none", "--prune", "--force"],
        timeout,
    )
    .await?;

    if !output.status.success() {
        // Fetch failed - repository might be corrupted, try re-clone
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::warn!(target: LOG_TARGET, "Git fetch failed ({}), removing and re-cloning", stderr.trim());
        // #[gamma::skip(try.propagate_to_unwrap, reason = "filesystem adapter errors must be propagated rather than converted into process panics")]
        remove_cached_repo(Path::new(path_str), &format!("removing stale cached repository '{path_str}'"))?;
        return clone_repo_with_executable(path_str, repo_url, timeout, git_executable).await;
    }

    // Reset to match remote HEAD (discard any local changes)
    // #[gamma::skip(try.propagate_to_unwrap, reason = "external git process failures must be propagated rather than converted into process panics")]
    let output = run_git_executable_with_timeout(git_executable, &["-C", path_str, "reset", "--hard", "origin/HEAD"], timeout).await?;
    check_git_output(&output, "git reset")?;
    Ok(RepoStatus::Ok)
}

fn remove_cached_repo(path: &Path, context: &str) -> Result<()> {
    fs::remove_dir_all(path).into_app_err(context)
}

/// Check whether git stderr indicates the repository was not found on the remote.
fn is_repo_not_found(stderr: &str) -> bool {
    let stderr_lower = stderr.to_lowercase();
    stderr_lower.contains("not found") || stderr_lower.contains("does not exist")
}

async fn clone_repo_with_executable(repo_path: &str, repo_url: &Url, timeout: Duration, git_executable: &Path) -> Result<RepoStatus> {
    log::info!(target: LOG_TARGET, "Syncing repository '{repo_url}'");
    // --filter=blob:none creates a partial clone with full history but no blob contents
    let output = run_git_executable_with_timeout(
        git_executable,
        &[
            "clone",
            "--filter=blob:none",
            "--single-branch",
            "--no-tags",
            repo_url.as_str(),
            repo_path,
        ],
        timeout,
    )
    .await?;

    if output.status.success() {
        return Ok(RepoStatus::Ok);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Clean up any partial clone directory left behind
    let path = Path::new(repo_path);
    if path.exists() {
        let _ = fs::remove_dir_all(path);
    }

    if is_repo_not_found(&stderr) {
        log::debug!(target: LOG_TARGET, "Repository '{repo_url}' not found on remote");
        return Ok(RepoStatus::NotFound);
    }

    bail!("git clone failed: {stderr}");
}

fn check_git_output(output: &std::process::Output, operation: &str) -> Result<()> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{operation} failed: {stderr}");
    }
    Ok(())
}

/// Count unique contributors in the repository
pub async fn count_contributors(repo_path: &Path, timeout: Duration) -> Result<u64> {
    let path_str = path_str(repo_path)?;
    // -s = summary (count only), -n = sort by count, -e = show emails
    // --all ensures we count contributors from all fetched refs, not just HEAD
    // #[gamma::skip(try.propagate_to_unwrap, reason = "external git process failures must be propagated rather than converted into process panics")]
    let output = run_git_with_timeout(&["-C", path_str, "shortlog", "-sne", "--all"], timeout).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git shortlog failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().count() as u64)
}

/// Commit statistics gathered from a single git log invocation.
pub struct CommitStats {
    /// Total number of commits.
    pub commit_count: u64,
    /// Timestamp of the first (oldest) commit.
    pub first_commit_at: DateTime<Utc>,
    /// Timestamp of the most recent commit.
    pub last_commit_at: DateTime<Utc>,
    /// Number of commits within each requested time window, in the same order as the input.
    pub commits_per_window: Vec<u64>,
}

/// Gather commit statistics from a single `git log` invocation.
///
/// Returns total count, first/last commit timestamps, and per-window commit counts
/// for each entry in `day_windows`. Uses Unix timestamps for efficient comparison.
pub async fn get_commit_stats(repo_path: &Path, day_windows: &[i64], timeout: Duration) -> Result<CommitStats> {
    let path_str = path_str(repo_path)?;

    // %at = author date as Unix timestamp
    // #[gamma::skip(try.propagate_to_unwrap, reason = "external git process failures must be propagated rather than converted into process panics")]
    let output = run_git_with_timeout(&["-C", path_str, "log", "--format=%at"], timeout).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git log failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(summarize_commit_timestamps(&stdout, day_windows, Utc::now().timestamp()))
}

/// Turn the `git log --format=%at` output into [`CommitStats`].
///
/// `now` is the reference point the day windows are measured back from.
fn summarize_commit_timestamps(stdout: &str, day_windows: &[i64], now: i64) -> CommitStats {
    let mut commit_count: u64 = 0;
    let mut first_timestamp: Option<i64> = None;
    let mut last_timestamp: Option<i64> = None;
    let mut window_counts = vec![0u64; day_windows.len()];
    let window_thresholds: Vec<i64> = day_windows.iter().map(|days| now - days * SECONDS_PER_DAY).collect();

    for line in stdout.lines() {
        let Ok(ts) = line.trim().parse::<i64>() else {
            continue;
        };

        commit_count += 1;

        // git log outputs newest first, so first parsed is last_timestamp, last parsed is first_timestamp
        if last_timestamp.is_none() {
            last_timestamp = Some(ts);
        }
        first_timestamp = Some(ts);

        for (i, threshold) in window_thresholds.iter().enumerate() {
            if ts >= *threshold {
                window_counts[i] += 1;
            }
        }
    }

    let first_commit_at = first_timestamp
        .and_then(|ts| DateTime::from_timestamp(ts, 0))
        .unwrap_or(DateTime::UNIX_EPOCH);

    let last_commit_at = last_timestamp
        .and_then(|ts| DateTime::from_timestamp(ts, 0))
        .unwrap_or(DateTime::UNIX_EPOCH);

    CommitStats {
        commit_count,
        first_commit_at,
        last_commit_at,
        commits_per_window: window_counts,
    }
}

async fn run_git_with_timeout(args: &[&str], timeout: Duration) -> Result<std::process::Output> {
    run_git_executable_with_timeout(Path::new("git"), args, timeout).await
}

async fn run_git_executable_with_timeout(executable: &Path, args: &[&str], timeout: Duration) -> Result<std::process::Output> {
    // #[gamma::skip(try.propagate_to_unwrap, reason = "spawning an external git process is an adapter boundary whose errors must remain recoverable")]
    let child = Command::new(executable)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Timeout cancellation drops the child handle; kill-on-drop prevents an orphaned Git
        // process from continuing after the operation has been reported as timed out.
        // #[gamma::skip(call.replace_with_default, call_result.default, reason = "subprocess cleanup policy is only observable when an external git process outlives a timed-out invocation")]
        .kill_on_drop(true)
        .spawn()
        .into_app_err("spawning git command")?;

    classify_git_run(tokio::time::timeout(timeout, child.wait_with_output()).await, args, timeout)
}

/// Turn the outcome of a timed git invocation into a [`Result`].
fn classify_git_run(
    result: Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed>,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output> {
    match result {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(e).into_app_err_with(|| format!("running 'git {}'", args.join(" "))),
        Err(_) => {
            bail!("'git {}' timed out after {} seconds", args.join(" "), timeout.as_secs());
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(windows)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt;
    #[cfg(windows)]
    use std::path::PathBuf;
    use std::process::{Command, ExitStatus, Output};

    use super::*;

    #[test]
    fn test_check_git_output_success() {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        };

        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        };

        let output = Output {
            status,
            stdout: vec![],
            stderr: vec![],
        };

        check_git_output(&output, "test operation").unwrap();
    }

    #[test]
    fn test_check_git_output_failure() {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(256) // Exit code 1
        };

        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(1)
        };

        let output = Output {
            status,
            stdout: vec![],
            stderr: b"error: failed to do something".to_vec(),
        };

        let result = check_git_output(&output, "test operation");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test operation failed"));
    }

    #[test]
    fn test_is_repo_not_found_positive() {
        assert!(is_repo_not_found("Repository not found"));
        assert!(is_repo_not_found("ERROR: Repository not found."));
        assert!(is_repo_not_found("remote: Repository does not exist"));
        assert!(is_repo_not_found("fatal: repository 'https://...' does not exist"));
    }

    #[test]
    fn test_is_repo_not_found_negative() {
        assert!(!is_repo_not_found("fatal: unable to access"));
        assert!(!is_repo_not_found("Permission denied"));
        assert!(!is_repo_not_found(""));
    }

    #[test]
    fn test_is_repo_not_found_case_insensitive() {
        assert!(is_repo_not_found("NOT FOUND"));
        assert!(is_repo_not_found("DOES NOT EXIST"));
        assert!(is_repo_not_found("Not Found"));
    }

    #[test]
    fn test_path_str_valid_utf8() {
        let path = Path::new("/tmp/test");
        assert_eq!(path_str(path).unwrap(), "/tmp/test");
    }

    #[test]
    fn commit_window_uses_exact_day_lengths() {
        assert_eq!(SECONDS_PER_DAY, 86_400);
    }

    #[test]
    fn test_check_git_output_with_stderr() {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(256)
        };

        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(1)
        };

        let stderr_msg = b"fatal: not a git repository";
        let output = Output {
            status,
            stdout: vec![],
            stderr: stderr_msg.to_vec(),
        };

        let result = check_git_output(&output, "git status");
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("git status failed"));
        assert!(error_msg.contains("not a git repository"));
    }

    /// Create a temp git repository with a few commits for testing.
    /// Returns the tempdir (must be kept alive) and the repo path.
    fn create_test_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let repo_path = tmp.path().join("test-repo");
        fs::create_dir_all(&repo_path).expect("create repo dir");

        // Initialize a repo and make commits
        let init = |args: &[&str]| {
            let _ = Command::new("git")
                .args(args)
                .current_dir(&repo_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .expect("run git command");
        };

        init(&["init"]);
        init(&["config", "user.email", "test@test.com"]);
        init(&["config", "user.name", "Test User"]);

        // Create two commits so we have meaningful stats
        fs::write(repo_path.join("file1.txt"), "hello").expect("write file1");
        init(&["add", "."]);
        init(&["commit", "-m", "first commit"]);

        fs::write(repo_path.join("file2.txt"), "world").expect("write file2");
        init(&["add", "."]);
        init(&["commit", "-m", "second commit"]);

        (tmp, repo_path)
    }

    /// Helper to set up a bare repo with one commit and return the temp dir + bare path.
    fn create_bare_repo_with_commit() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let bare_path = tmp.path().join("bare.git");

        let run = |args: &[&str], dir: &Path| {
            let _ = Command::new("git")
                .args(args)
                .current_dir(dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .expect("run git command");
        };

        fs::create_dir_all(&bare_path).expect("create bare dir");
        run(&["init", "--bare"], &bare_path);

        let work_path = tmp.path().join("work");
        let _ = Command::new("git")
            .args(["clone", bare_path.to_str().unwrap(), work_path.to_str().unwrap()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        run(&["config", "user.email", "test@test.com"], &work_path);
        run(&["config", "user.name", "Test User"], &work_path);
        fs::write(work_path.join("file.txt"), "content").expect("write file");
        run(&["add", "."], &work_path);
        run(&["commit", "-m", "initial"], &work_path);
        run(&["push"], &work_path);

        (tmp, bare_path)
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_run_git_with_timeout_success() {
        let output = run_git_with_timeout(&["--version"], GIT_TIMEOUT).await.unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("git version"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_run_git_with_timeout_failure() {
        // Run git log in a directory that is not a repo
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_str().unwrap();
        let output = run_git_with_timeout(&["-C", path, "log"], GIT_TIMEOUT).await.unwrap();
        assert!(!output.status.success());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_count_contributors() {
        let (_tmp, repo_path) = create_test_repo();
        let count = count_contributors(&repo_path, GIT_TIMEOUT).await.unwrap();
        assert_eq!(count, 1); // Single test user
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_count_contributors_failure() {
        let tmp = tempfile::tempdir().unwrap();
        // Not a git repo - shortlog should fail
        let result = count_contributors(tmp.path(), GIT_TIMEOUT).await;
        let _ = result.unwrap_err();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_commit_stats_basic() {
        let (_tmp, repo_path) = create_test_repo();
        let stats = get_commit_stats(&repo_path, &[30, 365], GIT_TIMEOUT).await.unwrap();
        assert_eq!(stats.commit_count, 2);
        assert!(stats.first_commit_at <= stats.last_commit_at);
        assert_eq!(stats.commits_per_window.len(), 2);
        // Both commits were just made, so they should be within both windows
        assert_eq!(stats.commits_per_window[0], 2); // last 30 days
        assert_eq!(stats.commits_per_window[1], 2); // last 365 days
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_commit_stats_empty_windows() {
        let (_tmp, repo_path) = create_test_repo();
        let stats = get_commit_stats(&repo_path, &[], GIT_TIMEOUT).await.unwrap();
        assert_eq!(stats.commit_count, 2);
        assert!(stats.commits_per_window.is_empty());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_commit_stats_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let result = get_commit_stats(tmp.path(), &[30], GIT_TIMEOUT).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_clone_from_bare() {
        let (tmp, bare_path) = create_bare_repo_with_commit();

        // Clone into a new path via get_repo
        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        let status = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();
        assert!(matches!(status, RepoStatus::Ok));
        assert!(clone_path.join(".git").exists());

        // Call get_repo again to exercise the fetch+reset path
        let status = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();
        assert!(matches!(status, RepoStatus::Ok));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_updates_existing_clone_without_recloning() {
        let (tmp, bare_path) = create_bare_repo_with_commit();
        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        let status = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();
        assert!(matches!(status, RepoStatus::Ok));

        let untracked_file = clone_path.join("untracked.txt");
        fs::write(&untracked_file, "local state").expect("write untracked file into clone");

        let status = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();

        assert!(matches!(status, RepoStatus::Ok));
        assert!(
            untracked_file.exists(),
            "updating an existing valid clone must fetch and reset, not remove and re-clone it"
        );
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_core_clones_when_target_does_not_exist() {
        let (tmp, bare_path) = create_bare_repo_with_commit();
        let clone_path = tmp.path().join("never-created").join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();

        let status = get_repo_core(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();

        assert!(matches!(status, RepoStatus::Ok));
        assert!(clone_path.join(".git").exists());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_reclones_when_git_dir_missing() {
        let (tmp, bare_path) = create_bare_repo_with_commit();

        // Clone via get_repo
        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        let status = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();
        assert!(matches!(status, RepoStatus::Ok));

        // Remove .git to simulate corruption
        fs::remove_dir_all(clone_path.join(".git")).unwrap();

        // get_repo should detect missing .git and re-clone
        let status = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap();
        assert!(matches!(status, RepoStatus::Ok));
        assert!(clone_path.join(".git").exists());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_nonexistent_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let clone_path = tmp.path().join("clone");
        let bad_url = Url::from_file_path(tmp.path().join("nonexistent.git")).unwrap();
        // Cloning a non-existent local path either returns NotFound or an error,
        // depending on the exact git error message. Either way, it should not succeed.
        if let Ok(status) = get_repo(&clone_path, &bad_url, GIT_TIMEOUT).await {
            assert!(matches!(status, RepoStatus::NotFound));
        }
        // Also acceptable — git error message didn't match not-found patterns
    }

    /// Create a repository whose commits come from two authors and are dated
    /// 400, 200, 30 and 1 days in the past.
    fn create_dated_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let repo_path = tmp.path().join("dated-repo");
        fs::create_dir_all(&repo_path).expect("create repo dir");

        let run = |args: &[&str], date: Option<&str>| {
            let mut cmd = Command::new("git");
            let _ = cmd
                .args(args)
                .current_dir(&repo_path)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", &repo_path)
                .env("XDG_CONFIG_HOME", &repo_path);
            if let Some(date) = date {
                let _ = cmd.env("GIT_AUTHOR_DATE", date).env("GIT_COMMITTER_DATE", date);
            }
            let output = cmd.output().expect("the tests require the `git` executable on PATH");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };

        run(&["-c", "init.defaultBranch=main", "init", "--quiet"], None);

        let commit = |name: &str, email: &str, days: i64, message: &str| {
            let date = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
            run(
                &[
                    "-c",
                    &format!("user.name={name}"),
                    "-c",
                    &format!("user.email={email}"),
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    message,
                ],
                Some(&date),
            );
        };

        commit("Alice", "alice@example.invalid", 400, "one");
        commit("Bob", "bob@example.invalid", 200, "two");
        commit("Alice", "alice@example.invalid", 30, "three");
        commit("Bob", "bob@example.invalid", 1, "four");

        (tmp, repo_path)
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_count_contributors_counts_distinct_authors() {
        let (_tmp, repo_path) = create_dated_repo();
        assert_eq!(count_contributors(&repo_path, GIT_TIMEOUT).await.unwrap(), 2);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_commit_stats_windows() {
        let (_tmp, repo_path) = create_dated_repo();
        let stats = get_commit_stats(&repo_path, &[90, 180, 365], GIT_TIMEOUT).await.unwrap();

        assert_eq!(stats.commit_count, 4);
        assert_eq!(stats.commits_per_window, vec![2, 2, 3]);

        let now = Utc::now();
        assert!((now - stats.first_commit_at).num_days() >= 399);
        assert!((now - stats.last_commit_at).num_days() <= 2);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_reclones_when_fetch_fails() {
        let (tmp, bare_path) = create_bare_repo_with_commit();

        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        assert!(matches!(
            get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap(),
            RepoStatus::Ok
        ));

        // Point the clone's origin at a remote that cannot be fetched. The next
        // sync must notice the failed fetch and re-clone from `bare_url`.
        let missing = tmp.path().join("gone.git");
        let output = Command::new("git")
            .args(["-C", clone_path.to_str().unwrap(), "remote", "set-url", "origin"])
            .arg(&missing)
            .output()
            .expect("the tests require the `git` executable on PATH");
        assert!(output.status.success());

        assert!(matches!(
            get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap(),
            RepoStatus::Ok
        ));
        assert!(clone_path.join(".git").exists());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn fetch_spawn_errors_are_propagated_instead_of_panicking() {
        let (tmp, bare_path) = create_bare_repo_with_commit();
        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        assert!(matches!(
            get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap(),
            RepoStatus::Ok
        ));

        let missing_git = tmp.path().join("missing-git");
        let error = get_repo_core_with_executable(&clone_path, &bare_url, GIT_TIMEOUT, &missing_git)
            .await
            .expect_err("a nonexistent executable must fail at process creation");

        assert!(format!("{error:#}").contains("spawning git command"), "{error:#}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn reset_failures_are_propagated_with_the_operation_name() {
        let (tmp, bare_path) = create_bare_repo_with_commit();
        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        assert!(matches!(
            get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap(),
            RepoStatus::Ok
        ));
        let output = Command::new("git")
            .args([
                "-C",
                clone_path.to_str().unwrap(),
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/missing",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());

        let error = get_repo_core(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap_err();

        assert!(format!("{error:#}").contains("git reset"), "{error:#}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_fails_when_fetch_and_reclone_both_fail() {
        let (tmp, bare_path) = create_bare_repo_with_commit();

        let clone_path = tmp.path().join("clone");
        let bare_url = Url::from_file_path(&bare_path).unwrap();
        assert!(matches!(
            get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap(),
            RepoStatus::Ok
        ));

        // Removing the origin repository makes both the fetch and the re-clone fail.
        fs::remove_dir_all(&bare_path).unwrap();

        let _ = get_repo(&clone_path, &bare_url, GIT_TIMEOUT).await.unwrap_err();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_clone_removes_the_directory_left_behind_by_a_failed_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("partial-clone");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("leftover.txt"), "debris").unwrap();

        // Cloning into a non-empty directory always fails, which leaves the directory
        // behind for `clone_repo` to clean up.
        let missing = Url::from_file_path(tmp.path().join("no-such-repo.git")).unwrap();
        let result = clone_repo_with_executable(target.to_str().unwrap(), &missing, GIT_TIMEOUT, Path::new("git")).await;

        assert!(!target.exists(), "the partial clone directory must be removed");
        // Git words the failure differently across versions, so accept either verdict.
        assert!(result.is_err() || matches!(result, Ok(RepoStatus::NotFound)));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn clone_spawn_errors_are_propagated_instead_of_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("clone");
        let remote = Url::from_file_path(tmp.path().join("remote.git")).unwrap();

        let missing_git = tmp.path().join("missing-git");
        let result = clone_repo_with_executable(target.to_str().unwrap(), &remote, GIT_TIMEOUT, &missing_git).await;

        let error = result.expect_err("a nonexistent executable must fail at process creation");
        assert!(format!("{error:#}").contains("spawning git command"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn test_path_str_invalid_utf8() {
        let path = Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe"));
        let error = path_str(path).unwrap_err();
        assert!(error.to_string().contains("invalid UTF-8 in repository path"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn test_path_str_invalid_utf8() {
        let path = PathBuf::from(OsString::from_wide(&[0xD800]));
        let error = path_str(&path).unwrap_err();
        assert!(error.to_string().contains("invalid UTF-8 in repository path"), "{error}");
    }

    #[cfg(windows)]
    #[tokio::test]
    #[cfg_attr(miri, ignore = "Tokio's Windows runtime requires unsupported I/O completion ports")]
    async fn public_git_queries_propagate_non_utf8_paths() {
        let path = PathBuf::from(OsString::from_wide(&[0xD800]));
        let _ = count_contributors(&path, GIT_TIMEOUT).await.unwrap_err();
        let Err(_) = get_commit_stats(&path, &[], GIT_TIMEOUT).await else {
            panic!("a non-UTF-8 path must be rejected");
        };
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn public_git_queries_propagate_non_utf8_paths() {
        let path = Path::new(OsStr::from_bytes(b"/invalid/\xff"));
        let Err(_) = count_contributors(path, GIT_TIMEOUT).await else {
            panic!("a non-UTF-8 path must be rejected");
        };
        let Err(_) = get_commit_stats(path, &[], GIT_TIMEOUT).await else {
            panic!("a non-UTF-8 path must be rejected");
        };
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "uses filesystem; Miri isolation forbids it")]
    async fn repository_parent_creation_errors_are_propagated() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_file = tmp.path().join("not-a-directory");
        fs::write(&parent_file, "occupied").unwrap();
        let destination = parent_file.join("clone");
        let remote = Url::from_file_path(tmp.path().join("remote.git")).unwrap();

        let result = get_repo_core(&destination, &remote, GIT_TIMEOUT).await;

        assert!(result.is_err());
        assert!(format!("{:#}", result.unwrap_err()).contains("creating directory"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses filesystem; Miri isolation forbids it")]
    fn cached_repository_removal_errors_are_propagated() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-directory");
        fs::write(&file, "occupied").unwrap();

        let error = remove_cached_repo(&file, "removing fixture repository").unwrap_err();

        assert!(error.to_string().contains("removing fixture repository"), "{error}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_run_git_with_timeout_reports_expiry() {
        // A future that never completes always trips the timeout, whereas a real git
        // process may finish on the first poll and win the race against a zero budget.
        let elapsed = tokio::time::timeout(Duration::ZERO, core::future::pending::<()>())
            .await
            .expect_err("a pending future can never complete, so the budget must expire");

        let error = classify_git_run(Err(elapsed), &["--version"], GIT_TIMEOUT).expect_err("an expired budget must surface as an error");
        assert!(format!("{error:#}").contains("timed out"), "{error:#}");
    }

    #[test]
    fn test_classify_git_run_reports_io_errors() {
        let error = classify_git_run(
            Ok(Err(std::io::Error::other("pipe exploded"))),
            &["-C", "somewhere", "log"],
            GIT_TIMEOUT,
        )
        .expect_err("an I/O failure must surface as an error");

        let message = format!("{error:#}");
        assert!(message.contains("running 'git -C somewhere log'"), "{message}");
        assert!(message.contains("pipe exploded"), "{message}");
    }

    #[test]
    fn test_summarize_commit_timestamps_skips_unparsable_lines() {
        const DAY: i64 = 86400;
        let now = 1_000 * DAY;

        // `git log` only ever emits timestamps, but a garbled line must be skipped
        // rather than derail the whole summary.
        let stdout = format!("{}\nnot-a-timestamp\n\n{}\n", now - DAY, now - 400 * DAY);
        let stats = summarize_commit_timestamps(&stdout, &[90, 365], now);

        assert_eq!(stats.commit_count, 2);
        assert_eq!(stats.commits_per_window, vec![1, 1]);
        assert_eq!(stats.first_commit_at.timestamp(), now - 400 * DAY);
        assert_eq!(stats.last_commit_at.timestamp(), now - DAY);
    }

    #[test]
    fn commit_exactly_on_a_window_boundary_is_included() {
        let now = 500 * SECONDS_PER_DAY;
        let threshold = now - 90 * SECONDS_PER_DAY;
        let stats = summarize_commit_timestamps(&format!("{threshold}\n"), &[90], now);

        assert_eq!(stats.commits_per_window, vec![1]);
    }

    #[test]
    fn day_windows_use_exactly_86400_seconds() {
        let now = 500 * SECONDS_PER_DAY;
        let just_outside = now - 90 * SECONDS_PER_DAY - 1;
        let stats = summarize_commit_timestamps(&format!("{just_outside}\n"), &[90], now);

        assert_eq!(stats.commits_per_window, vec![0]);
    }

    #[test]
    fn test_summarize_commit_timestamps_without_any_commits() {
        let stats = summarize_commit_timestamps("", &[90], 1_000_000);
        assert_eq!(stats.commit_count, 0);
        assert_eq!(stats.first_commit_at, DateTime::UNIX_EPOCH);
        assert_eq!(stats.last_commit_at, DateTime::UNIX_EPOCH);
    }

    #[test]
    fn first_log_entry_is_the_latest_commit() {
        let stats = summarize_commit_timestamps("200\n100\n", &[], 300);
        assert_eq!(stats.last_commit_at.timestamp(), 200);
        assert_eq!(stats.first_commit_at.timestamp(), 100);
        assert_eq!(stats.last_commit_at.timestamp_subsec_nanos(), 0);
        assert_eq!(stats.first_commit_at.timestamp_subsec_nanos(), 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_get_repo_with_a_path_that_has_no_parent() {
        // An empty path is the one case where `Path::parent` reports `None`, so no
        // directory is created and the clone fails on its empty destination instead.
        let (_tmp, bare_path) = create_bare_repo_with_commit();
        let bare_url = Url::from_file_path(&bare_path).unwrap();

        let result = get_repo(Path::new(""), &bare_url, GIT_TIMEOUT).await;

        // Git words the rejection differently across versions, so accept either verdict.
        match result {
            Ok(status) => assert!(matches!(status, RepoStatus::NotFound), "unexpected status: {status:?}"),
            Err(e) => assert!(format!("{e:#}").contains("git clone failed"), "{e:#}"),
        }
    }
}
