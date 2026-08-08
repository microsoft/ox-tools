// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cargo_metadata::{Metadata, MetadataCommand, PackageId, TargetKind};
use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use ohno::{EnrichableExt, IntoAppError, app_err};
use tokio::task::{JoinHandle, spawn_blocking};

use super::{CodebaseData, git, source_file_analyzer};
use crate::facts::ProviderResult;
use crate::facts::cache::{Cache, CacheResult};
use crate::facts::codebase::github_workflow_analyzer::{GitHubWorkflowInfo, sniff_github_workflows};
use crate::facts::crate_spec::{self, CrateSpec};
use crate::facts::path_utils::sanitize_path_component;
use crate::facts::repo_spec::RepoSpec;
use crate::facts::request_tracker::{RequestTracker, TrackedTopic};
use crate::facts::throttler::Throttler;
use crate::{HashMap, Result};

pub(super) const LOG_TARGET: &str = "  codebase";

const MAX_CONCURRENT_REQUESTS: usize = 5;

#[derive(Debug, Clone)]
pub struct Provider {
    cache: Cache,
    throttler: Arc<Throttler>,
    timeouts: Timeouts,
}

const METADATA_TIMEOUT: Duration = Duration::from_mins(5);
const GIT_REPO_TIMEOUT: Duration = Duration::from_mins(5);

/// Time budgets for the external operations the provider drives.
///
/// Production always uses [`Timeouts::default`]; tests substitute
/// `Duration::ZERO` to reach the expiry paths without waiting.
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    /// Budget for cloning or updating the repository.
    git_repo: Duration,
    /// Budget for a single `cargo metadata` invocation.
    metadata: Duration,
    /// Budget for each individual git command.
    git_command: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            git_repo: GIT_REPO_TIMEOUT,
            metadata: METADATA_TIMEOUT,
            git_command: git::GIT_TIMEOUT,
        }
    }
}

/// Repository-level data that's shared across all crates in a repository
#[derive(Debug, Clone)]
struct RepoData {
    metadata: Arc<Metadata>,
    workflows: GitHubWorkflowInfo,
    contributor_count: u64,
    commits_last_90_days: u64,
    commits_last_180_days: u64,
    commits_last_365_days: u64,
    commit_count: u64,
    first_commit_at: DateTime<Utc>,
    last_commit_at: DateTime<Utc>,
}

impl Provider {
    #[must_use]
    pub fn new(cache: Cache) -> Self {
        Self {
            cache,
            throttler: Throttler::new(MAX_CONCURRENT_REQUESTS),
            timeouts: Timeouts::default(),
        }
    }

    /// Build a provider whose external operations use `timeouts` instead of the defaults.
    #[cfg(test)]
    #[cfg(not(miri))]
    fn with_timeouts(cache: Cache, timeouts: Timeouts) -> Self {
        Self {
            cache,
            throttler: Throttler::new(MAX_CONCURRENT_REQUESTS),
            timeouts,
        }
    }

    pub async fn get_codebase_data(
        &self,
        crates: Arc<[CrateSpec]>,
        tracker: &RequestTracker,
    ) -> impl Iterator<Item = (CrateSpec, ProviderResult<CodebaseData>)> {
        let repo_crates = crate_spec::by_repo(crates.iter().cloned());

        tracker.add_requests(TrackedTopic::Codebase, repo_crates.len() as u64);

        // Check cache for all crates from each repo
        // If any crate from a repo is expired/missing, we reanalyze all crates from that repo for consistency
        let mut cached_results = Vec::new();
        let mut needs_repo_fetch: HashMap<RepoSpec, Vec<CrateSpec>> = HashMap::default();

        for (repo_spec, crates) in repo_crates {
            let mut all_cached_data = Vec::new();
            let mut needs_fresh_repo = false;

            // Check if all crates from this repo have valid cached state
            for crate_spec in &crates {
                let crate_name = crate_spec.name();
                let filename = Self::get_data_filename(crate_name, &repo_spec);

                match self.cache.load::<CodebaseData>(&filename) {
                    CacheResult::Data(cached_data) => {
                        all_cached_data.push((crate_spec.clone(), ProviderResult::Found(cached_data)));
                    }
                    CacheResult::NoData(reason) => {
                        all_cached_data.push((crate_spec.clone(), ProviderResult::Unavailable(reason.into())));
                    }
                    CacheResult::Miss => {
                        needs_fresh_repo = true;
                        break; // No need to check more - we'll reanalyze all
                    }
                }
            }

            if needs_fresh_repo {
                // At least one crate is expired/missing, reanalyze all crates from this repo
                let _ = needs_repo_fetch.insert(repo_spec, crates);
            } else {
                // All crates have valid cached state, we're done for this repo
                cached_results.extend(all_cached_data);
                tracker.complete_request(TrackedTopic::Codebase);
            }
        }

        // Process each repo end-to-end: fetch repo data, then analyze and cache its crates's codebase data.
        let repo_results = join_all(needs_repo_fetch.into_iter().map(|(repo_spec, crates)| {
            let provider = self.clone();
            let tracker = tracker.clone();

            tokio::spawn(async move { provider.fetch_and_analyze_repo(repo_spec, crates, tracker).await })
        }))
        .await
        .into_iter()
        .flat_map(|result| result.expect("task must not panic"));

        // Combine cached and newly analyzed results
        cached_results.into_iter().chain(repo_results).inspect(|(crate_spec, result)| {
            if let ProviderResult::Error(e) = result {
                log::error!(target: LOG_TARGET, "Could not analyze codebase for {crate_spec}: {e:#}");
            } else if let ProviderResult::Unavailable(reason) = result {
                log::warn!(target: LOG_TARGET, "Codebase data unavailable for {crate_spec}: {reason}");
            }
        })
    }

    /// Fetch repository data and analyze all its crates, writing cache files per-crate.
    async fn fetch_and_analyze_repo(
        self,
        repo_spec: RepoSpec,
        crates: Vec<CrateSpec>,
        tracker: RequestTracker,
    ) -> Vec<(CrateSpec, ProviderResult<CodebaseData>)> {
        let _permit = self.throttler.acquire().await;
        // Sync the git repo first — failures here are transient (network) and should not be cached
        let repo_path = self.get_repo_cache_path(&repo_spec);
        match Self::sync_repo(&repo_path, &repo_spec, self.timeouts.git_repo, self.timeouts.git_command).await {
            Err(e) => {
                tracker.complete_request(TrackedTopic::Codebase);
                let error = Arc::new(e);
                return crates
                    .into_iter()
                    .map(|crate_spec| (crate_spec, ProviderResult::Error(Arc::clone(&error))))
                    .collect();
            }
            Ok(git::RepoStatus::NotFound) => {
                let reason = format!("repository '{repo_spec}' not found");
                log::debug!(target: LOG_TARGET, "{reason}");
                let results: Vec<_> = crates
                    .into_iter()
                    .map(|crate_spec| {
                        let filename = Self::get_data_filename(crate_spec.name(), &repo_spec);
                        if let Err(e) = self.cache.save_no_data(&filename, &reason) {
                            log::debug!(target: LOG_TARGET, "Could not save cache for {crate_spec}: {e:#}");
                            return (crate_spec, ProviderResult::Error(Arc::new(e)));
                        }
                        (crate_spec, ProviderResult::Unavailable(reason.clone().into()))
                    })
                    .collect();
                tracker.complete_request(TrackedTopic::Codebase);
                return results;
            }
            Ok(git::RepoStatus::Ok) => {}
        }

        let fetch_result = self.fetch_repo_data_core(&repo_spec, &repo_path).await;

        let results = match fetch_result {
            Ok(repo_data) => {
                let repo_data = Arc::new(repo_data);
                join_all(crates.into_iter().map(|crate_spec| {
                    let provider = self.clone();
                    let repo_spec = repo_spec.clone();
                    let repo_data = Arc::clone(&repo_data);

                    tokio::spawn(provider.analyze_crate(crate_spec, repo_spec, repo_data))
                }))
                .await
                .into_iter()
                .map(|result| result.expect("task must not panic"))
                .collect()
            }
            Err(e) => {
                let reason = format!("{e:#}");
                log::warn!(target: LOG_TARGET, "Could not analyze repository '{repo_spec}': {reason}");
                crates
                    .into_iter()
                    .map(|crate_spec| {
                        let filename = Self::get_data_filename(crate_spec.name(), &repo_spec);

                        // only write NoData if there's no existing valid cache entry
                        if !matches!(self.cache.load::<CodebaseData>(&filename), CacheResult::Data(_))
                            && let Err(e) = self.cache.save_no_data(&filename, &reason)
                        {
                            log::debug!(target: LOG_TARGET, "Could not save cache for {crate_spec}: {e:#}");
                        }

                        (crate_spec, ProviderResult::Unavailable(reason.clone().into()))
                    })
                    .collect()
            }
        };

        tracker.complete_request(TrackedTopic::Codebase);
        results
    }

    /// Sync (clone or pull) the git repository. Failures here are transient.
    async fn sync_repo(
        repo_path: &Path,
        repo_spec: &RepoSpec,
        repo_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<git::RepoStatus> {
        let git_result = tokio::time::timeout(repo_timeout, git::get_repo(repo_path, repo_spec.url(), command_timeout)).await;

        match git_result {
            Err(_) => Err(app_err!(
                "git operation timed out after {} seconds for repository '{repo_spec}'",
                repo_timeout.as_secs(),
            )),
            Ok(Err(e)) => Err(e.enrich_with(|| format!("syncing repository '{repo_spec}'"))),
            Ok(Ok(status)) => Ok(status),
        }
    }

    async fn fetch_repo_data_core(&self, repo_spec: &RepoSpec, repo_path: &Path) -> Result<RepoData> {
        let root_manifest = repo_path.join("Cargo.toml");
        if !root_manifest.exists() {
            return Err(app_err!("could not find Cargo.toml in root of repository '{repo_spec}'"));
        }

        log::debug!(target: LOG_TARGET, "Running cargo metadata for repository '{repo_spec}'");
        let timeout_result = tokio::time::timeout(
            self.timeouts.metadata,
            spawn_blocking(move || MetadataCommand::new().manifest_path(&root_manifest).exec()),
        )
        .await;

        let metadata = match timeout_result {
            Err(_) => {
                let timeout_secs = self.timeouts.metadata.as_secs();
                return Err(app_err!(
                    "cargo metadata timed out after {timeout_secs} seconds for repository '{repo_spec}' - workspace may be too large or Cargo.toml is corrupted"
                ));
            }
            Ok(join_result) => classify_metadata_result(join_result, repo_spec)?,
        };

        log::debug!(target: LOG_TARGET, "Gathering commit statistics for repository '{repo_spec}'");

        let (contributor_count, commit_stats) = tokio::join!(
            git::count_contributors(repo_path, self.timeouts.git_command),
            git::get_commit_stats(repo_path, &[90, 180, 365], self.timeouts.git_command),
        );

        let contributor_count = match contributor_count {
            Ok(count) => count,
            Err(e) => {
                log::warn!(target: LOG_TARGET, "Could not count contributors for '{repo_spec}': {e:#}");
                0
            }
        };

        let commit_stats = match commit_stats {
            Ok(stats) => stats,
            Err(e) => {
                log::warn!(target: LOG_TARGET, "Could not get commit statistics for '{repo_spec}': {e:#}");
                git::CommitStats {
                    commit_count: 0,
                    first_commit_at: DateTime::UNIX_EPOCH,
                    last_commit_at: DateTime::UNIX_EPOCH,
                    commits_per_window: vec![0, 0, 0],
                }
            }
        };

        log::debug!(target: LOG_TARGET, "Detecting workflows in repository '{repo_spec}'");

        let repo_path_owned = repo_path.to_path_buf();
        let workflows = spawn_blocking(move || sniff_github_workflows(&repo_path_owned))
            .await
            .expect("task must not panic")
            .into_app_err_with(|| format!("analyzing GitHub workflows in repository '{repo_spec}'"))?;

        log::debug!(target: LOG_TARGET, "Analyzed repository '{repo_spec}', found {} packages", metadata.packages.len());

        Ok(RepoData {
            metadata: Arc::new(metadata),
            workflows,
            contributor_count,
            commits_last_90_days: commit_stats.commits_per_window[0],
            commits_last_180_days: commit_stats.commits_per_window[1],
            commits_last_365_days: commit_stats.commits_per_window[2],
            commit_count: commit_stats.commit_count,
            first_commit_at: commit_stats.first_commit_at,
            last_commit_at: commit_stats.last_commit_at,
        })
    }

    /// Analyze a single crate
    async fn analyze_crate(
        self,
        crate_spec: CrateSpec,
        repo_spec: RepoSpec,
        repo_data: Arc<RepoData>,
    ) -> (CrateSpec, ProviderResult<CodebaseData>) {
        let crate_name = crate_spec.name().to_string();
        let filename = Self::get_data_filename(&crate_name, &repo_spec);

        log::info!(target: LOG_TARGET, "Analyzing source code for {crate_spec} from repository '{repo_spec}'");

        // Find the package we're interested in
        let Some(package) = repo_data.metadata.packages.iter().find(|p| p.name == crate_name) else {
            let reason = format!("crate '{crate_name}' not found in repository '{repo_spec}'");
            log::debug!(target: LOG_TARGET, "{reason}");
            if let Err(e) = self.cache.save_no_data(&filename, &reason) {
                log::debug!(target: LOG_TARGET, "Could not save cache for {crate_spec}: {e:#}");
            }
            return (crate_spec, ProviderResult::Unavailable(reason.into()));
        };

        let Some(crate_path) = package.manifest_path.parent() else {
            let reason = format!("package manifest has no parent directory for {crate_spec}");
            if let Err(e) = self.cache.save_no_data(&filename, &reason) {
                log::debug!(target: LOG_TARGET, "Could not save cache for {crate_spec}: {e:#}");
            }
            return (crate_spec, ProviderResult::Unavailable(reason.into()));
        };

        log::debug!(target: LOG_TARGET, "Found crate at {crate_path}");

        let example_count = package.targets.iter().filter(|t| t.kind.contains(&TargetKind::Example)).count();
        let transitive_dependencies = Self::count_transitive_dependencies(&package.id, &repo_data.metadata);

        // Create CodebaseData with non-source fields initialized
        let mut codebase_data = CodebaseData {
            source_files_analyzed: 0,
            production_lines: 0,
            test_lines: 0,
            comment_lines: 0,
            unsafe_count: 0,
            source_files_with_errors: 0,
            example_count: example_count as u64,
            transitive_dependencies: transitive_dependencies as u64,
            workflows_detected: repo_data.workflows.workflows_detected,
            miri_detected: repo_data.workflows.miri_detected,
            clippy_detected: repo_data.workflows.clippy_detected,
            contributors: repo_data.contributor_count,
            commits_last_90_days: repo_data.commits_last_90_days,
            commits_last_180_days: repo_data.commits_last_180_days,
            commits_last_365_days: repo_data.commits_last_365_days,
            commit_count: repo_data.commit_count,
            first_commit_at: repo_data.first_commit_at,
            last_commit_at: repo_data.last_commit_at,
        };

        Self::analyze_source_files(crate_path.as_std_path(), &mut codebase_data).await;

        let result = match self.cache.save(&filename, &codebase_data) {
            Ok(()) => ProviderResult::Found(codebase_data),
            Err(e) => ProviderResult::Error(Arc::new(e)),
        };

        log::debug!(target: LOG_TARGET, "Completed analysis of {crate_spec}");

        (crate_spec, result)
    }

    /// Analyze source files in a crate directory
    ///
    /// Walks the `src/` directory and analyzes each Rust file using the source analyzer,
    /// directly updating the provided `CodebaseData` with accumulated statistics.
    /// Uses parallel processing with tokio tasks and a semaphore to limit concurrency.
    ///
    /// Individual files that cannot be read or parsed are skipped, so the walk as a whole
    /// never fails.
    async fn analyze_source_files(crate_path: &Path, codebase_data: &mut CodebaseData) {
        const MAX_FILES: usize = 10_000;
        const MAX_FILE_SIZE: u64 = 5_000_000; // 5MB
        const MAX_DEPTH: usize = 50;

        let src_dir = crate_path.join("src");
        if !src_dir.exists() {
            return;
        }

        // Collect file paths first (blocking directory walk)
        let file_paths: Vec<_> = spawn_blocking(move || {
            walkdir::WalkDir::new(&src_dir)
                .follow_links(false) // Don't follow symlinks to prevent loops
                .max_depth(MAX_DEPTH)
                .into_iter()
                .filter_map(filter_walk_entry)
                .filter(|e| !e.file_type().is_dir())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("rs"))
                .take(MAX_FILES)
                .filter_map(|entry| filter_source_entry(&entry, MAX_FILE_SIZE))
                .collect()
        })
        .await
        .expect("task must not panic");

        if file_paths.is_empty() {
            return;
        }

        if file_paths.len() == MAX_FILES {
            log::debug!(
                target: LOG_TARGET,
                "File count limit ({MAX_FILES}) reached in {}, some files may not be analyzed",
                crate_path.join("src").display()
            );
        }

        log::debug!(target: LOG_TARGET, "Analyzing {} source files", file_paths.len());

        // Analyze files in parallel, one blocking task per worker rather than one per file.
        // Only `num_workers` files can be analyzed at a time regardless, so spawning a task per
        // file just adds scheduling overhead for tasks that immediately queue.
        let num_workers = std::thread::available_parallelism().map_or(4, core::num::NonZero::get);
        let chunk_size = file_paths.len().div_ceil(num_workers).max(1);
        let mut analysis_tasks: Vec<JoinHandle<Vec<Result<_, ohno::AppError>>>> = Vec::with_capacity(num_workers);
        for chunk in file_paths.chunks(chunk_size) {
            let chunk = chunk.to_vec();

            let task = spawn_blocking(move || {
                chunk
                    .into_iter()
                    .map(|path| {
                        let content =
                            fs::read_to_string(&path).into_app_err_with(|| format!("reading source file '{}'", path.display()))?;
                        Ok(source_file_analyzer::analyze_source_file(&content))
                    })
                    .collect()
            });

            analysis_tasks.push(task);
        }

        let results = join_all(analysis_tasks).await;

        for file_stats in results
            .into_iter()
            .flat_map(|task_result| task_result.expect("tasks must not panic"))
            .filter_map(filter_source_result)
        {
            codebase_data.source_files_analyzed += 1;
            codebase_data.production_lines += file_stats.production_lines;
            codebase_data.test_lines += file_stats.test_lines;
            codebase_data.comment_lines += file_stats.comment_lines;
            codebase_data.unsafe_count += file_stats.unsafe_count;

            if file_stats.has_errors {
                codebase_data.source_files_with_errors += 1;
            }
        }
    }

    /// Get the sanitized host/owner/repo path components for a repository.
    fn safe_repo_components(repo_spec: &RepoSpec) -> (String, String, String) {
        (
            sanitize_path_component(repo_spec.host()),
            sanitize_path_component(repo_spec.owner()),
            sanitize_path_component(repo_spec.repo()),
        )
    }

    /// Get the cache path for a specific repository
    fn get_repo_cache_path(&self, repo_spec: &RepoSpec) -> PathBuf {
        let (safe_host, safe_owner, safe_repo) = Self::safe_repo_components(repo_spec);
        self.cache.dir().join("repos").join(safe_host).join(safe_owner).join(safe_repo)
    }

    /// Get the codebase data filename for a specific crate in a repository
    ///
    /// Returns a relative path suitable for `Cache::load`/`Cache::save`.
    fn get_data_filename(crate_name: &str, repo_spec: &RepoSpec) -> String {
        let (safe_host, safe_owner, safe_repo) = Self::safe_repo_components(repo_spec);
        let safe_crate = sanitize_path_component(crate_name);

        format!("analysis/{safe_host}/{safe_owner}/{safe_repo}/{safe_crate}.bin")
    }

    /// Count transitive dependencies by walking the dependency graph
    fn count_transitive_dependencies(package_id: &PackageId, metadata: &Metadata) -> usize {
        use std::collections::VecDeque;

        use crate::HashSet;

        let Some(resolve) = &metadata.resolve else {
            log::debug!(target: LOG_TARGET, "No resolve graph in metadata, cannot count transitive dependencies");
            return 0;
        };

        let node_map: HashMap<&PackageId, &cargo_metadata::Node> = resolve.nodes.iter().map(|n| (&n.id, n)).collect();

        // Find the node for this package in the resolve graph
        let Some(node) = node_map.get(package_id) else {
            log::debug!(target: LOG_TARGET, "Could not find package '{package_id}' in resolve graph, cannot count transitive dependencies");
            return 0;
        };

        // Breadth-first traversal of the dependency graph using references
        let mut visited: HashSet<&PackageId> = HashSet::default();
        let mut to_visit: VecDeque<&PackageId> = VecDeque::new();

        // Start with direct dependencies (push references)
        for dep_id in &node.dependencies {
            to_visit.push_back(dep_id);
        }

        // Visit all transitive dependencies
        while let Some(dep_id) = to_visit.pop_front() {
            if visited.insert(dep_id)
                && let Some(dep_node) = node_map.get(dep_id)
            {
                for transitive_dep_id in &dep_node.dependencies {
                    to_visit.push_back(transitive_dep_id);
                }
            }
        }

        visited.len()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn filter_walk_entry(entry: walkdir::Result<walkdir::DirEntry>) -> Option<walkdir::DirEntry> {
    entry
        .inspect_err(|error| log::debug!(target: LOG_TARGET, "Could not walk directory: {error:#}"))
        .ok()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn filter_source_entry(entry: &walkdir::DirEntry, max_file_size: u64) -> Option<PathBuf> {
    let metadata = entry
        .metadata()
        .inspect_err(|error| log::debug!(target: LOG_TARGET, "Could not read metadata for {}: {error:#}", entry.path().display()))
        .ok()?;

    if metadata.len() > max_file_size {
        log::debug!(target: LOG_TARGET, "Skipping large file '{}' ({} bytes)", entry.path().display(), metadata.len());
        return None;
    }

    Some(entry.path().to_path_buf())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn filter_source_result(
    result: core::result::Result<source_file_analyzer::SourceFileInfo, ohno::AppError>,
) -> Option<source_file_analyzer::SourceFileInfo> {
    result
        .inspect_err(|error| log::debug!(target: LOG_TARGET, "Could not read source file, skipping: {error:#}"))
        .ok()
}

/// Turn the outcome of the `cargo metadata` blocking task into a [`Result`].
fn classify_metadata_result(
    join_result: Result<Result<Metadata, cargo_metadata::Error>, tokio::task::JoinError>,
    repo_spec: &RepoSpec,
) -> Result<Metadata> {
    match join_result {
        Ok(Ok(metadata)) => Ok(metadata),
        Ok(Err(e)) => Err(e).into_app_err_with(|| format!("running cargo metadata for repository '{repo_spec}'")),
        Err(e) => Err(e).into_app_err_with(|| format!("joining cargo metadata task for repository '{repo_spec}'")),
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::process::Command;
    use std::sync::Mutex;

    use semver::Version;
    use tempfile::TempDir;
    use url::Url;

    use super::*;
    use crate::facts::Progress;

    #[test]
    fn test_safe_repo_components() {
        let url = url::Url::parse("https://github.com/tokio-rs/tokio").unwrap();
        let repo_spec = RepoSpec::parse(&url).unwrap();
        let (host, owner, repo) = Provider::safe_repo_components(&repo_spec);
        assert_eq!(host, "github.com");
        assert_eq!(owner, "tokio-rs");
        assert_eq!(repo, "tokio");
    }

    #[test]
    fn test_safe_repo_components_sanitized() {
        let url = url::Url::parse("https://evil.com/../../etc/passwd").unwrap();
        let repo_spec = RepoSpec::parse(&url).unwrap();
        let (host, owner, repo) = Provider::safe_repo_components(&repo_spec);
        assert!(!host.contains(".."));
        assert!(!owner.contains(".."));
        assert!(!repo.contains(".."));
    }

    #[test]
    fn test_get_data_filename() {
        let url = url::Url::parse("https://github.com/tokio-rs/tokio").unwrap();
        let repo_spec = RepoSpec::parse(&url).unwrap();
        let filename = Provider::get_data_filename("tokio", &repo_spec);
        assert!(filename.starts_with("analysis/"));
        assert!(filename.contains("github.com"));
        assert!(filename.contains("tokio-rs"));
        assert!(filename.contains("tokio"));
        assert!(Path::new(&filename).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("bin")));
    }

    #[test]
    fn test_get_repo_cache_path() {
        let cache = Cache::new("/tmp/cache", Duration::from_hours(1), false);
        let provider = Provider::new(cache);
        let url = url::Url::parse("https://github.com/tokio-rs/tokio").unwrap();
        let repo_spec = RepoSpec::parse(&url).unwrap();
        let path = provider.get_repo_cache_path(&repo_spec);
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("repos"));
        assert!(path_str.contains("github.com"));
        assert!(path_str.contains("tokio-rs"));
        assert!(path_str.contains("tokio"));
    }

    #[test]
    fn test_get_data_filename_sanitized() {
        let url = url::Url::parse("https://evil.com/../../etc/passwd").unwrap();
        let repo_spec = RepoSpec::parse(&url).unwrap();
        let filename = Provider::get_data_filename("../malicious", &repo_spec);
        assert!(!filename.contains("../"));
    }

    // ---------------------------------------------------------------------
    // Local git repository fixtures
    //
    // The provider clones the repository it analyzes with the `git` command line
    // tool. Git happily clones from a `file://` URL, so the tests below build a
    // real repository in a temp directory and point the provider at it, which
    // exercises the genuine clone/fetch/reset, `cargo metadata` and analysis
    // paths without any network access.
    // ---------------------------------------------------------------------

    #[derive(Debug)]
    struct NoOpProgress;

    impl Progress for NoOpProgress {
        fn set_phase(&self, _phase: &str) {}
        fn set_determinate(&self, _callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {}
        fn set_indeterminate(&self, _callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {}
        fn println(&self, _msg: &str) {}
        fn done(&self) {}
    }

    fn test_tracker() -> RequestTracker {
        RequestTracker::new(&(Arc::new(NoOpProgress) as Arc<dyn Progress>))
    }

    /// Root manifest of the fixture repository.
    ///
    /// The fixture is a small workspace whose root package depends on `aprz-helper`,
    /// which in turn depends on `aprz-leaf`. Both are path dependencies, so
    /// `cargo metadata` resolves them without touching the network while still
    /// producing a dependency graph to walk.
    const FIXTURE_MANIFEST: &str = r#"[workspace]
members = ["helper", "leaf"]

[package]
name = "aprz-fixture"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
aprz-helper = { path = "helper" }
aprz-leaf = { path = "leaf" }
"#;

    const FIXTURE_HELPER_MANIFEST: &str = r#"[package]
name = "aprz-helper"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
aprz-leaf = { path = "../leaf" }
"#;

    const FIXTURE_LEAF_MANIFEST: &str = r#"[package]
name = "aprz-leaf"
version = "0.1.0"
edition = "2021"
publish = false
"#;

    /// A manifest for a crate that stands on its own, with no workspace members.
    const FIXTURE_STANDALONE_MANIFEST: &str = r#"[workspace]

[package]
name = "aprz-fixture"
version = "0.1.0"
edition = "2021"
publish = false
"#;

    const FIXTURE_LIB_RS: &str = r"//! Fixture crate for the codebase provider tests.

/// Adds two numbers.
pub fn add(a: u32, b: u32) -> u32 {
    // A plain line comment.
    a + b
}

/// Reads through a raw pointer.
///
/// # Safety
///
/// The pointer must be valid.
pub unsafe fn read_raw(p: *const u32) -> u32 {
    unsafe { *p }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }
}
";

    const FIXTURE_UTIL_RS: &str = r"/// Doubles a number.
pub fn double(v: u32) -> u32 {
    v * 2
}
";

    const FIXTURE_EXAMPLE_RS: &str = r#"fn main() {
    println!("{}", aprz_fixture::add(1, 2));
}
"#;

    const FIXTURE_WORKFLOW_FULL: &str = r"name: CI
on: [push]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: cargo clippy -- -D warnings
      - run: cargo miri test
";

    const FIXTURE_WORKFLOW_PLAIN: &str = r"name: CI
on: [push]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test
";

    /// Run a git command in `dir`, isolated from the ambient git configuration.
    ///
    /// `HOME`/`XDG_CONFIG_HOME` are redirected into the fixture and the system
    /// configuration is disabled, so the fixture never reads or writes the
    /// developer's global git config.
    fn git(dir: &Path, date: Option<&str>, args: &[&str]) {
        let mut cmd = Command::new("git");
        let _ = cmd
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", dir)
            .env("XDG_CONFIG_HOME", dir)
            .env("GIT_TERMINAL_PROMPT", "0");

        if let Some(date) = date {
            let _ = cmd.env("GIT_AUTHOR_DATE", date).env("GIT_COMMITTER_DATE", date);
        }

        let output = cmd.output().expect("the tests require the `git` executable on PATH");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A git timestamp `days` days in the past, used to spread the fixture's
    /// commit history across the provider's 90/180/365 day windows.
    fn days_ago(days: i64) -> String {
        (Utc::now() - chrono::Duration::days(days)).to_rfc3339()
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("creating fixture directory");
        }
        fs::write(path, contents).expect("writing fixture file");
    }

    /// How a fixture repository should be populated.
    #[derive(Debug, Clone, Copy)]
    struct FixtureOptions {
        /// Contents of the root `Cargo.toml`, or `None` to omit it entirely.
        manifest: Option<&'static str>,
        /// Contents of `.github/workflows/ci.yml`, or `None` for no workflows.
        workflow: Option<&'static str>,
    }

    impl Default for FixtureOptions {
        fn default() -> Self {
            Self {
                manifest: Some(FIXTURE_MANIFEST),
                workflow: Some(FIXTURE_WORKFLOW_FULL),
            }
        }
    }

    /// A git repository fixture holding a small, self-contained crate.
    #[derive(Debug)]
    struct RepoFixture {
        _tmp: TempDir,
        path: PathBuf,
    }

    const ALICE: [&str; 4] = ["-c", "user.name=Alice Fixture", "-c", "user.email=alice@example.invalid"];
    const BOB: [&str; 4] = ["-c", "user.name=Bob Fixture", "-c", "user.email=bob@example.invalid"];

    impl RepoFixture {
        /// Build a repository with five commits from two distinct authors,
        /// dated 500, 300, 200, 30 and 1 days ago.
        fn new(options: FixtureOptions) -> Self {
            let tmp = tempfile::tempdir().expect("creating temp dir");
            let path = tmp.path().join("fixture-repo");
            fs::create_dir_all(&path).expect("creating fixture repo dir");

            // `-c init.defaultBranch` keeps the fixture independent of the
            // ambient git configuration and of the git version's default.
            git(&path, None, &["-c", "init.defaultBranch=main", "init", "--quiet"]);

            let commit = |who: &[&str; 4], days: i64, message: &str| {
                git(&path, None, &["add", "-A"]);
                let mut args = who.to_vec();
                args.extend_from_slice(&["commit", "--quiet", "-m", message]);
                git(&path, Some(&days_ago(days)), &args);
            };

            if let Some(manifest) = options.manifest {
                write_file(&path.join("Cargo.toml"), manifest);
            }
            write_file(&path.join("src").join("lib.rs"), FIXTURE_LIB_RS);
            write_file(&path.join("helper").join("Cargo.toml"), FIXTURE_HELPER_MANIFEST);
            write_file(&path.join("helper").join("src").join("lib.rs"), FIXTURE_UTIL_RS);
            write_file(&path.join("leaf").join("Cargo.toml"), FIXTURE_LEAF_MANIFEST);
            write_file(&path.join("leaf").join("src").join("lib.rs"), FIXTURE_UTIL_RS);
            commit(&ALICE, 500, "initial crate");

            write_file(&path.join("src").join("util.rs"), FIXTURE_UTIL_RS);
            commit(&BOB, 300, "add util module");

            if let Some(workflow) = options.workflow {
                write_file(&path.join(".github").join("workflows").join("ci.yml"), workflow);
            }
            write_file(&path.join("examples").join("demo.rs"), FIXTURE_EXAMPLE_RS);
            commit(&ALICE, 200, "add CI and example");

            write_file(&path.join("README.md"), "# fixture\n");
            commit(&ALICE, 30, "add readme");

            write_file(&path.join("README.md"), "# fixture crate\n");
            commit(&BOB, 1, "tweak readme");

            Self { _tmp: tmp, path }
        }

        fn url(&self) -> Url {
            Url::from_file_path(&self.path).expect("fixture path is absolute")
        }

        fn repo_spec(&self) -> RepoSpec {
            repo_spec_for_url(&self.url())
        }
    }

    /// Build a [`RepoSpec`] that keeps `url` verbatim.
    ///
    /// `RepoSpec::parse` rebuilds the URL as `scheme://host/owner/repo`, which
    /// throws away the tail of a filesystem path and the port of a local test
    /// server, so it cannot describe either kind of fixture remote.
    /// Round-tripping through the serialized form keeps the URL intact while
    /// still producing the host/owner/repo components the provider uses to
    /// build cache paths.
    fn repo_spec_for_url(url: &Url) -> RepoSpec {
        serde_json::from_value(serde_json::json!({
            "url": url.as_str(),
            "host": "local",
            "owner": "fixtures",
            "repo": "aprz-fixture",
        }))
        .expect("RepoSpec deserializes from the shape produced by its own Serialize impl")
    }

    fn crate_spec(name: &str, repo_spec: &RepoSpec) -> CrateSpec {
        CrateSpec::from_arcs_with_repo(Arc::from(name), Arc::new(Version::new(0, 1, 0)), repo_spec.clone())
    }

    /// Run the provider over a single crate and return its result.
    async fn analyze_one(provider: &Provider, name: &str, repo_spec: &RepoSpec) -> ProviderResult<CodebaseData> {
        let crates: Arc<[CrateSpec]> = Arc::from(vec![crate_spec(name, repo_spec)]);
        let mut results: Vec<_> = provider.get_codebase_data(crates, &test_tracker()).await.collect();
        assert_eq!(results.len(), 1, "expected exactly one result");
        results.remove(0).1
    }

    fn test_cache(dir: &Path) -> Cache {
        Cache::new(dir, Duration::from_hours(1), false)
    }

    #[derive(Debug)]
    struct CapturingLogger;

    static CAPTURING_LOGGER: CapturingLogger = CapturingLogger;
    static CAPTURED_LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    impl log::Log for CapturingLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            CAPTURED_LOGS
                .lock()
                .expect("captured log mutex should not be poisoned")
                .push(record.args().to_string());
        }

        fn flush(&self) {}
    }

    fn install_capturing_logger() {
        CAPTURED_LOGS.lock().expect("captured log mutex should not be poisoned").clear();
        let _ = log::set_logger(&CAPTURING_LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
    }

    fn captured_logs() -> String {
        CAPTURED_LOGS.lock().expect("captured log mutex should not be poisoned").join("\n")
    }

    fn run_ignored_helper(helper_name: &str) -> String {
        let module = module_path!().split_once("::").map_or(module_path!(), |(_, rest)| rest);
        let output = Command::new(std::env::current_exe().expect("test binary path should be available"))
            .env("CARGO_APRZ_CAPTURE_LOGS", "1")
            .args(["--exact", &format!("{module}::{helper_name}"), "--ignored", "--nocapture"])
            .output()
            .expect("capturing helper test should run");

        assert!(
            output.status.success(),
            "capturing helper failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout).expect("capturing helper output should be UTF-8")
    }

    // The three helpers below only panic when a test has already failed, so their
    // failure arms are unreachable in a green run.

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn unavailable_reason(result: &ProviderResult<CodebaseData>) -> String {
        match result {
            ProviderResult::Unavailable(reason) => reason.to_string(),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn found_data(result: ProviderResult<CodebaseData>) -> CodebaseData {
        match result {
            ProviderResult::Found(data) => data,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn error_message(result: &ProviderResult<CodebaseData>) -> String {
        match result {
            ProviderResult::Error(e) => format!("{e:#}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// Make every cache *write* for `crate_name` fail, by parking a directory where the
    /// cache file itself belongs: the provider creates the parent directories happily and
    /// then cannot create the file.
    fn block_cache_writes(cache_dir: &Path, crate_name: &str, repo_spec: &RepoSpec) {
        let blocked = cache_dir.join(Provider::get_data_filename(crate_name, repo_spec));
        fs::create_dir_all(&blocked).expect("creating the blocking directory");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_local_repository_end_to_end() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let repo_spec = fixture.repo_spec();

        let data = found_data(analyze_one(&provider, "aprz-fixture", &repo_spec).await);

        // Two distinct commit authors, five commits dated 500/300/200/30/1 days ago.
        assert_eq!(data.contributors, 2);
        assert_eq!(data.commit_count, 5);
        assert_eq!(data.commits_last_90_days, 2);
        assert_eq!(data.commits_last_180_days, 2);
        assert_eq!(data.commits_last_365_days, 4);
        assert!(data.first_commit_at < data.last_commit_at);

        // `src/lib.rs` and `src/util.rs`.
        assert_eq!(data.source_files_analyzed, 2);
        assert_eq!(data.source_files_with_errors, 0);
        assert!(data.production_lines > 0, "expected production lines");
        assert!(data.test_lines > 0, "expected test lines");
        assert!(data.comment_lines > 0, "expected comment lines");
        assert_eq!(data.unsafe_count, 2); // `unsafe fn` plus the `unsafe` block inside it

        assert_eq!(data.example_count, 1);
        assert_eq!(data.transitive_dependencies, 2); // aprz-helper plus aprz-leaf
        assert!(data.workflows_detected);
        assert!(data.clippy_detected);
        assert!(data.miri_detected);

        // The repository was really cloned into the cache.
        let repo_path = provider.get_repo_cache_path(&repo_spec);
        assert!(repo_path.join(".git").exists());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_local_repository_without_workflows() {
        let fixture = RepoFixture::new(FixtureOptions {
            workflow: None,
            ..FixtureOptions::default()
        });
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));

        let data = found_data(analyze_one(&provider, "aprz-fixture", &fixture.repo_spec()).await);

        assert!(!data.workflows_detected);
        assert!(!data.clippy_detected);
        assert!(!data.miri_detected);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_workflow_without_clippy_or_miri_is_detected_but_empty() {
        let fixture = RepoFixture::new(FixtureOptions {
            workflow: Some(FIXTURE_WORKFLOW_PLAIN),
            ..FixtureOptions::default()
        });
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));

        let data = found_data(analyze_one(&provider, "aprz-fixture", &fixture.repo_spec()).await);

        assert!(data.workflows_detected);
        assert!(!data.clippy_detected);
        assert!(!data.miri_detected);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_second_call_is_served_from_cache() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let repo_spec = fixture.repo_spec();

        let first = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(first.is_found());

        // Delete the clone. A second call that still succeeds proves the result
        // came from the cache rather than from another clone.
        let repo_path = provider.get_repo_cache_path(&repo_spec);
        fs::remove_dir_all(&repo_path).expect("removing the cached clone");

        let second = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(second.is_found());
        assert!(!repo_path.exists(), "the cache hit must not re-clone the repository");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_second_call_updates_existing_clone() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        // `ignore_cache` forces a second analysis, which finds the clone already
        // present and takes the fetch/reset path instead of cloning again.
        let provider = Provider::new(Cache::new(cache_dir.path(), Duration::from_hours(1), true));
        let repo_spec = fixture.repo_spec();

        assert!(analyze_one(&provider, "aprz-fixture", &repo_spec).await.is_found());

        let repo_path = provider.get_repo_cache_path(&repo_spec);
        assert!(repo_path.join(".git").exists());

        assert!(analyze_one(&provider, "aprz-fixture", &repo_spec).await.is_found());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_reclones_when_git_directory_is_missing() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(Cache::new(cache_dir.path(), Duration::from_hours(1), true));
        let repo_spec = fixture.repo_spec();

        assert!(analyze_one(&provider, "aprz-fixture", &repo_spec).await.is_found());

        let repo_path = provider.get_repo_cache_path(&repo_spec);
        fs::remove_dir_all(repo_path.join(".git")).expect("removing the .git directory");

        assert!(analyze_one(&provider, "aprz-fixture", &repo_spec).await.is_found());
        assert!(repo_path.join(".git").exists());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_missing_root_manifest_is_unavailable() {
        let fixture = RepoFixture::new(FixtureOptions {
            manifest: None,
            ..FixtureOptions::default()
        });
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let repo_spec = fixture.repo_spec();

        let result = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(unavailable_reason(&result).contains("could not find Cargo.toml"));

        // The failure is cached, so a second call answers without touching git.
        let second = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(unavailable_reason(&second).contains("could not find Cargo.toml"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_repository_analysis_failure_is_written_as_no_data() {
        let fixture = RepoFixture::new(FixtureOptions {
            manifest: None,
            ..FixtureOptions::default()
        });
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let repo_spec = fixture.repo_spec();

        let result = analyze_one(&provider, "aprz-fixture", &repo_spec).await;

        assert!(unavailable_reason(&result).contains("could not find Cargo.toml"));
        let filename = Provider::get_data_filename("aprz-fixture", &repo_spec);
        assert!(matches!(provider.cache.load::<CodebaseData>(&filename), CacheResult::NoData(_)));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_broken_manifest_makes_cargo_metadata_fail() {
        let fixture = RepoFixture::new(FixtureOptions {
            manifest: Some("this is not a valid manifest\n"),
            ..FixtureOptions::default()
        });
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));

        let result = analyze_one(&provider, "aprz-fixture", &fixture.repo_spec()).await;
        let reason = unavailable_reason(&result);
        assert!(reason.contains("cargo metadata"), "unexpected reason: {reason}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_crate_not_present_in_repository() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));

        let result = analyze_one(&provider, "not-in-this-repo", &fixture.repo_spec()).await;
        let reason = unavailable_reason(&result);
        assert!(reason.contains("not found in repository"), "unexpected reason: {reason}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_unreachable_repository_reports_error() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let missing = Url::from_file_path(tmp.path().join("no-such-repo")).expect("path is absolute");

        let result = analyze_one(&provider, "aprz-fixture", &repo_spec_for_url(&missing)).await;
        assert!(error_message(&result).contains("syncing repository"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_repository_not_found_is_unavailable() {
        // Git reports a 404 from an HTTP remote as "repository ... not found",
        // which is the message `is_repo_not_found` recognizes.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        // `RepoSpec::parse` rebuilds the URL from host/owner/repo and so drops the
        // ephemeral port of the mock server; build the spec directly instead.
        let url = Url::parse(&format!("{}/fixtures/missing", server.uri())).expect("valid URL");
        let repo_spec = repo_spec_for_url(&url);

        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));

        let result = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(unavailable_reason(&result).contains("not found"));

        // The "not found" verdict is cached.
        let second = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(unavailable_reason(&second).contains("not found"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_crates_without_repository_are_skipped() {
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let crates: Arc<[CrateSpec]> = Arc::from(vec![CrateSpec::from_arcs(Arc::from("no-repo"), Arc::new(Version::new(1, 0, 0)))]);

        assert!(provider.get_codebase_data(crates, &test_tracker()).await.next().is_none());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_repo_data_tolerates_a_non_git_directory() {
        // A directory holding a valid manifest but no git history: `cargo metadata`
        // succeeds while the git queries fail, which must degrade to zeroed stats.
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("Cargo.toml"), FIXTURE_STANDALONE_MANIFEST);
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);

        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let repo_spec = repo_spec_for_url(&Url::from_file_path(tmp.path()).expect("path is absolute"));

        let repo_data = provider
            .fetch_repo_data_core(&repo_spec, tmp.path())
            .await
            .expect("cargo metadata succeeds even without git history");

        assert_eq!(repo_data.contributor_count, 0);
        assert_eq!(repo_data.commit_count, 0);
        assert_eq!(repo_data.commits_last_90_days, 0);
        assert_eq!(repo_data.commits_last_365_days, 0);
        assert_eq!(repo_data.first_commit_at, DateTime::UNIX_EPOCH);
        assert!(!repo_data.workflows.workflows_detected);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_sync_repo_clones_then_updates() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let repo_path = cache_dir.path().join("repos").join("clone");
        let repo_spec = fixture.repo_spec();

        let status = Provider::sync_repo(&repo_path, &repo_spec, GIT_REPO_TIMEOUT, git::GIT_TIMEOUT)
            .await
            .expect("cloning the fixture");
        assert!(matches!(status, git::RepoStatus::Ok));

        let status = Provider::sync_repo(&repo_path, &repo_spec, GIT_REPO_TIMEOUT, git::GIT_TIMEOUT)
            .await
            .expect("updating the clone");
        assert!(matches!(status, git::RepoStatus::Ok));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_without_src_directory() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        let mut data = empty_codebase_data();

        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_ignores_non_rust_files() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("notes.txt"), "not rust\n");
        let mut data = empty_codebase_data();

        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_counts_unparsable_files() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);
        write_file(&tmp.path().join("src").join("broken.rs"), "fn nope( {\n");
        let mut data = empty_codebase_data();

        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 2);
        assert_eq!(data.source_files_with_errors, 1);
        assert_eq!(data.unsafe_count, 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_skips_unreadable_files() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);

        // A dangling symlink survives the directory walk (which does not follow
        // links) but cannot be read, exercising the skip-on-read-error path.
        std::os::unix::fs::symlink(tmp.path().join("does-not-exist.rs"), tmp.path().join("src").join("dangling.rs"))
            .expect("creating a dangling symlink");

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_skips_oversized_files() {
        const OVER_LIMIT: usize = 5_000_001;

        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);
        fs::write(tmp.path().join("src").join("huge.rs"), vec![b'\n'; OVER_LIMIT]).expect("writing the oversized file");

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_includes_file_at_size_limit() {
        const EXACT_LIMIT: usize = 5_000_000;

        let tmp = tempfile::tempdir().expect("creating temp dir");
        fs::create_dir_all(tmp.path().join("src")).expect("creating src dir");
        fs::write(tmp.path().join("src").join("limit.rs"), vec![b' '; EXACT_LIMIT]).expect("writing the limit-sized file");

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_counts_only_rust_extension() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), "pub fn ok() {}\n");
        write_file(&tmp.path().join("src").join("notes.txt"), "fn not_rust() {}\n");

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, 1);
        assert_eq!(data.source_files_with_errors, 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn test_analyze_source_files_does_not_log_limit_before_limit() {
        let logs = run_ignored_helper("helper_capture_source_file_limit_log_before_limit");

        assert!(logs.contains("source file limit log capture control"), "{logs}");
        assert!(!logs.contains("File count limit"), "{logs}");
    }

    #[tokio::test]
    #[ignore = "spawned by test_analyze_source_files_does_not_log_limit_before_limit"]
    async fn helper_capture_source_file_limit_log_before_limit() {
        if std::env::var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
            return;
        }

        install_capturing_logger();
        log::debug!("source file limit log capture control");

        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), "pub fn ok() {}\n");
        let mut data = empty_codebase_data();

        Provider::analyze_source_files(tmp.path(), &mut data).await;

        println!("{}", captured_logs());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_count_transitive_dependencies_for_unknown_package() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("Cargo.toml"), FIXTURE_STANDALONE_MANIFEST);
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);

        let manifest = tmp.path().join("Cargo.toml");
        let metadata = MetadataCommand::new().manifest_path(&manifest).exec().expect("cargo metadata");
        let unknown = PackageId {
            repr: "not-a-real-package".to_owned(),
        };
        assert_eq!(Provider::count_transitive_dependencies(&unknown, &metadata), 0);

        let package = metadata.packages.first().expect("the fixture has one package");
        assert_eq!(Provider::count_transitive_dependencies(&package.id, &metadata), 0);

        let no_deps = MetadataCommand::new()
            .manifest_path(&manifest)
            .no_deps()
            .exec()
            .expect("cargo metadata --no-deps");
        assert!(no_deps.resolve.is_none());
        assert_eq!(Provider::count_transitive_dependencies(&package.id, &no_deps), 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_repository_not_found_reports_error_when_the_cache_cannot_be_written() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/fixtures/missing", server.uri())).expect("valid URL");
        let repo_spec = repo_spec_for_url(&url);

        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        block_cache_writes(cache_dir.path(), "aprz-fixture", &repo_spec);
        let provider = Provider::new(test_cache(cache_dir.path()));

        let result = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(error_message(&result).contains("creating cache file"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_repository_analysis_failure_survives_a_failed_cache_write() {
        let fixture = RepoFixture::new(FixtureOptions {
            manifest: None,
            ..FixtureOptions::default()
        });
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let repo_spec = fixture.repo_spec();
        block_cache_writes(cache_dir.path(), "aprz-fixture", &repo_spec);
        let provider = Provider::new(test_cache(cache_dir.path()));

        // The repository verdict is still reported even though it cannot be memoized.
        let result = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(unavailable_reason(&result).contains("could not find Cargo.toml"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_missing_crate_survives_a_failed_cache_write() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let repo_spec = fixture.repo_spec();
        block_cache_writes(cache_dir.path(), "not-in-this-repo", &repo_spec);
        let provider = Provider::new(test_cache(cache_dir.path()));

        let result = analyze_one(&provider, "not-in-this-repo", &repo_spec).await;
        assert!(unavailable_reason(&result).contains("not found in repository"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_failure_to_cache_a_successful_analysis_is_an_error() {
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let repo_spec = fixture.repo_spec();
        block_cache_writes(cache_dir.path(), "aprz-fixture", &repo_spec);
        let provider = Provider::new(test_cache(cache_dir.path()));

        let result = analyze_one(&provider, "aprz-fixture", &repo_spec).await;
        assert!(error_message(&result).contains("creating cache file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_unreadable_workflow_file_fails_repository_analysis() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("Cargo.toml"), FIXTURE_STANDALONE_MANIFEST);
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);
        let workflow = tmp.path().join(".github").join("workflows").join("ci.yml");
        write_file(&workflow, FIXTURE_WORKFLOW_FULL);
        fs::set_permissions(&workflow, fs::Permissions::from_mode(0o000)).expect("removing all permissions");

        if fs::File::open(&workflow).is_ok() {
            return; // running as root, where file permissions do not apply
        }

        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::new(test_cache(cache_dir.path()));
        let repo_spec = repo_spec_for_url(&Url::from_file_path(tmp.path()).expect("path is absolute"));

        let error = provider
            .fetch_repo_data_core(&repo_spec, tmp.path())
            .await
            .expect_err("an unreadable workflow file must fail the analysis");
        assert!(format!("{error:#}").contains("analyzing GitHub workflows"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_tolerates_an_unwalkable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);
        let locked = tmp.path().join("src").join("locked");
        write_file(&locked.join("hidden.rs"), FIXTURE_UTIL_RS);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("removing all permissions");

        if fs::read_dir(&locked).is_ok() {
            return; // running as root, where directory permissions do not apply
        }

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        // Restore the permissions so the temporary directory can be cleaned up.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restoring permissions");

        assert_eq!(data.source_files_analyzed, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_skips_files_it_cannot_stat() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);
        let listable = tmp.path().join("src").join("listable");
        write_file(&listable.join("hidden.rs"), FIXTURE_UTIL_RS);

        // Readable but not searchable: the directory can be listed, so the walk finds the
        // file, but the file itself cannot be stat'ed.
        fs::set_permissions(&listable, fs::Permissions::from_mode(0o444)).expect("dropping the execute bit");

        if fs::metadata(listable.join("hidden.rs")).is_ok() {
            return; // running as root, where directory permissions do not apply
        }

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        // Restore the permissions so the temporary directory can be cleaned up.
        fs::set_permissions(&listable, fs::Permissions::from_mode(0o700)).expect("restoring permissions");

        assert_eq!(data.source_files_analyzed, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_analyze_source_files_stops_at_the_file_limit() {
        const MAX_FILES: usize = 10_000;

        // The "limit reached" notice is logged at debug level; evaluate its arguments too.
        crate::facts::test_logging::enable_log_argument_evaluation();

        let tmp = tempfile::tempdir().expect("creating temp dir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("creating the source directory");
        for i in 0..=MAX_FILES {
            fs::write(src.join(format!("f{i}.rs")), "").expect("writing a fixture file");
        }

        let mut data = empty_codebase_data();
        Provider::analyze_source_files(tmp.path(), &mut data).await;

        assert_eq!(data.source_files_analyzed, MAX_FILES as u64);
    }

    #[test]
    fn test_noop_progress_is_inert() {
        let progress = NoOpProgress;
        progress.set_phase("phase");
        progress.set_determinate(Box::new(|| (0, 0, String::new())));
        progress.set_indeterminate(Box::new(String::new));
        progress.println("message");
        progress.done();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_sync_repo_reports_a_timeout() {
        // A zero-length budget expires before the clone can make progress, so the
        // expiry arm is reached without any sleeping and without any flakiness.
        let fixture = RepoFixture::new(FixtureOptions::default());
        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let repo_path = cache_dir.path().join("repos").join("clone");

        let error = Provider::sync_repo(&repo_path, &fixture.repo_spec(), Duration::ZERO, git::GIT_TIMEOUT)
            .await
            .expect_err("a zero timeout must expire");

        assert!(format!("{error:#}").contains("git operation timed out"), "{error:#}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_cargo_metadata_timeout_is_reported() {
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("Cargo.toml"), FIXTURE_STANDALONE_MANIFEST);
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);

        let cache_dir = tempfile::tempdir().expect("creating temp dir");
        let provider = Provider::with_timeouts(
            test_cache(cache_dir.path()),
            Timeouts {
                metadata: Duration::ZERO,
                ..Timeouts::default()
            },
        );
        let repo_spec = repo_spec_for_url(&Url::from_file_path(tmp.path()).expect("path is absolute"));

        let error = provider
            .fetch_repo_data_core(&repo_spec, tmp.path())
            .await
            .expect_err("a zero timeout must expire");

        assert!(format!("{error:#}").contains("cargo metadata timed out"), "{error:#}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_classify_metadata_result_reports_a_join_error() {
        // The blocking task that runs `cargo metadata` cannot panic in practice, so the
        // join failure is reproduced here with a task that deliberately does. The panic
        // message it prints to stderr during the run is expected.
        let join_error = spawn_blocking(
            #[cfg_attr(coverage_nightly, coverage(off))]
            || panic!("boom"),
        )
        .await
        .expect_err("the task panics, so joining it must fail");

        let url = Url::parse("https://github.com/tokio-rs/tokio").expect("valid URL");
        let repo_spec = RepoSpec::parse(&url).expect("a GitHub URL is a valid repo spec");

        let error = classify_metadata_result(Err(join_error), &repo_spec).expect_err("a join failure is an error");
        assert!(format!("{error:#}").contains("joining cargo metadata task"), "{error:#}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run external commands")]
    async fn test_crate_with_a_parentless_manifest_path_is_unavailable() {
        // `cargo metadata` always reports an absolute manifest path, so the only way to
        // reach the "no parent directory" fallback is to build metadata that claims
        // otherwise: an empty path is the one case where `Path::parent` reports `None`.
        let tmp = tempfile::tempdir().expect("creating temp dir");
        write_file(&tmp.path().join("Cargo.toml"), FIXTURE_STANDALONE_MANIFEST);
        write_file(&tmp.path().join("src").join("lib.rs"), FIXTURE_LIB_RS);

        let metadata = MetadataCommand::new()
            .manifest_path(tmp.path().join("Cargo.toml"))
            .exec()
            .expect("cargo metadata");

        let mut json = serde_json::to_value(&metadata).expect("metadata serializes");
        for package in json["packages"].as_array_mut().expect("metadata lists its packages") {
            package["manifest_path"] = serde_json::Value::String(String::new());
        }
        let metadata: Metadata = serde_json::from_value(json).expect("metadata round-trips through its own JSON form");

        let repo_spec = repo_spec_for_url(&Url::from_file_path(tmp.path()).expect("path is absolute"));
        let repo_data = Arc::new(RepoData {
            metadata: Arc::new(metadata),
            workflows: GitHubWorkflowInfo::default(),
            contributor_count: 0,
            commits_last_90_days: 0,
            commits_last_180_days: 0,
            commits_last_365_days: 0,
            commit_count: 0,
            first_commit_at: DateTime::UNIX_EPOCH,
            last_commit_at: DateTime::UNIX_EPOCH,
        });

        // Run it twice: once where the verdict can be memoized, and once where the
        // cache write fails and the verdict is still reported.
        for block_writes in [false, true] {
            let cache_dir = tempfile::tempdir().expect("creating temp dir");
            if block_writes {
                block_cache_writes(cache_dir.path(), "aprz-fixture", &repo_spec);
            }
            let provider = Provider::new(test_cache(cache_dir.path()));

            let (_, result) = provider
                .analyze_crate(crate_spec("aprz-fixture", &repo_spec), repo_spec.clone(), Arc::clone(&repo_data))
                .await;

            assert!(unavailable_reason(&result).contains("manifest has no parent directory"));
        }
    }

    fn empty_codebase_data() -> CodebaseData {
        CodebaseData {
            source_files_analyzed: 0,
            production_lines: 0,
            test_lines: 0,
            comment_lines: 0,
            unsafe_count: 0,
            source_files_with_errors: 0,
            example_count: 0,
            transitive_dependencies: 0,
            workflows_detected: false,
            miri_detected: false,
            clippy_detected: false,
            contributors: 0,
            commits_last_90_days: 0,
            commits_last_180_days: 0,
            commits_last_365_days: 0,
            commit_count: 0,
            first_commit_at: DateTime::UNIX_EPOCH,
            last_commit_at: DateTime::UNIX_EPOCH,
        }
    }
}
