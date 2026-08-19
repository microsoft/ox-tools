// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use compact_str::CompactString;
use futures_util::future::join_all;
use ohno::EnrichableExt;
use reqwest::header::LINK;

use super::cached_repo::{CachedIssue, CachedRepo};
use super::client::{Client, HostingApiResult, Issue, RateLimitInfo, Repository};
use super::{AgeStats, BugLabelMatcher, HostingData, TimeWindowStats};
use crate::facts::cache::{Cache, CacheResult};
use crate::facts::crate_spec::{self, CrateSpec};
use crate::facts::path_utils::sanitize_path_component;
use crate::facts::request_tracker::{RequestTracker, TopicStatus, TrackedTopic};
use crate::facts::throttler::Throttler;
use crate::facts::{Endpoints, ProviderResult, RepoSpec};
use crate::{HashMap, Result};

const LOG_TARGET: &str = "   hosting";
const SECONDS_PER_DAY: f64 = 86400.0;
const ISSUE_LOOKBACK_DAYS: i64 = 365 * 10;
const ISSUE_PAGE_SIZE: u8 = 100;
const MAX_ISSUE_PAGES: u32 = 10;
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 3600;
const MAX_CONCURRENT_REQUESTS: usize = 5;

/// Which caller-supplied API token belongs to a host.
#[derive(Debug, Clone, Copy)]
enum TokenKind {
    GitHub,
    Codeberg,
}

/// Configuration for a specific hosting provider
#[derive(Debug, Clone, Copy)]
#[expect(clippy::struct_field_names, reason = "host_domain is a clear and reasonable field name")]
struct Host {
    /// Host domain (e.g., `github.com`, `Codeberg.org`)
    host_domain: &'static str,
    /// Base API URL
    base_url: &'static str,
    /// Display name for error messages
    display_name: &'static str,
    /// Whether to use `watchers_count` field instead of `subscribers_count`
    use_watchers_for_subscribers: bool,
    /// Which caller-supplied token authenticates against this host
    token_kind: TokenKind,
}

/// Supported hosting providers
static SUPPORTED_HOSTS: &[Host] = &[
    Host {
        host_domain: "github.com",
        base_url: "https://api.github.com",
        display_name: "GitHub",
        use_watchers_for_subscribers: false,
        token_kind: TokenKind::GitHub,
    },
    Host {
        host_domain: "codeberg.org",
        base_url: "https://codeberg.org/api/v1",
        display_name: "Codeberg",
        use_watchers_for_subscribers: true,
        token_kind: TokenKind::Codeberg,
    },
];

/// Macro to unwrap `HostingApiResult` or propagate rate limit/error
macro_rules! unwrap_or_return {
    ($expr:expr) => {
        match $expr {
            HostingApiResult::Success(data, rate_limit) => (data, rate_limit),
            HostingApiResult::RateLimited(rate_limit) => return HostingApiResult::RateLimited(rate_limit),
            HostingApiResult::NotFound(rate_limit) => return HostingApiResult::NotFound(rate_limit),
            HostingApiResult::Failed(e, rate_limit) => return HostingApiResult::Failed(e, rate_limit),
        }
    };
}

/// Macro to unwrap `HostingApiResult` for repo data operations or return early with `RepoData` error
/// Takes operation name strings and constructs error messages
/// Warning is optional - if provided, logs on failure
macro_rules! unwrap_repo_result {
    ($expr:expr, $repo_spec:expr, $operation:expr, $cache:expr, $cache_filename:expr $(, $warn_operation:expr)?) => {
        match $expr {
            HostingApiResult::Success(data, rate_limit) => (data, rate_limit),
            HostingApiResult::RateLimited(rate_limit) => {
                return RepoData {
                    repo_spec: $repo_spec.clone(),
                    result: ProviderResult::Error(Arc::new(ohno::app_err!("rate limited"))),
                    rate_limit: Some(rate_limit),
                    is_rate_limited: true,
                };
            }
            HostingApiResult::NotFound(rate_limit) => {
                let reason = format!("repository '{}' not found", $repo_spec);
                if let Err(e) = $cache.save_no_data($cache_filename, &reason) {
                    log::debug!(target: LOG_TARGET, "Could not save cache for '{}': {e:#}", $repo_spec);
                }
                return RepoData {
                    repo_spec: $repo_spec.clone(),
                    result: ProviderResult::Unavailable(reason.into()),
                    rate_limit,
                    is_rate_limited: false,
                };
            }
            HostingApiResult::Failed(e, rate_limit) => {
                $(
                    log::warn!(target: LOG_TARGET, "Could not fetch {} for '{}': {:#}", $warn_operation, $repo_spec, e);
                )?
                let error = Arc::new(e.enrich_with(|| format!("fetching {} for repository '{}'", $operation, $repo_spec)));
                return RepoData {
                    repo_spec: $repo_spec.clone(),
                    result: ProviderResult::Error(error),
                    rate_limit,
                    is_rate_limited: false,
                };
            }
        }
    };
}

/// Result of fetching hosting data for a repository
#[derive(Debug, Clone)]
struct RepoData {
    repo_spec: RepoSpec,
    result: ProviderResult<HostingData>,
    rate_limit: Option<RateLimitInfo>,
    is_rate_limited: bool,
}

impl RepoData {
    /// Create `RepoData` from cached data
    const fn from_cache(repo_spec: RepoSpec, result: ProviderResult<HostingData>) -> Self {
        Self {
            repo_spec,
            result,
            rate_limit: None,
            is_rate_limited: false,
        }
    }

    /// Create `RepoData` from successful fetch
    const fn success(repo_spec: RepoSpec, result: ProviderResult<HostingData>, rate_limit: Option<RateLimitInfo>) -> Self {
        Self {
            repo_spec,
            result,
            rate_limit,
            is_rate_limited: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Provider {
    hosts: Vec<(Host, Client)>,
    cache: Cache,
    throttler: Arc<Throttler>,
    bug_labels: Arc<BugLabelMatcher>,
}

impl Provider {
    pub fn new(
        github_token: Option<&str>,
        codeberg_token: Option<&str>,
        cache: Cache,
        bug_labels: Arc<BugLabelMatcher>,
        endpoints: &Endpoints,
    ) -> Result<Self> {
        let mut hosts = Vec::with_capacity(SUPPORTED_HOSTS.len());

        for host in SUPPORTED_HOSTS {
            // Map host to its caller-supplied token
            let token = match host.token_kind {
                TokenKind::GitHub => github_token,
                TokenKind::Codeberg => codeberg_token,
            };

            let base_url = endpoints.host_url(host.host_domain).unwrap_or(host.base_url);
            let client = Client::new(token, base_url)?;
            hosts.push((*host, client));
        }

        Ok(Self {
            hosts,
            cache,
            throttler: Throttler::new(MAX_CONCURRENT_REQUESTS),
            bug_labels,
        })
    }

    pub async fn get_hosting_data(
        &self,
        crates: Arc<[CrateSpec]>,
        tracker: &RequestTracker,
    ) -> impl Iterator<Item = (CrateSpec, ProviderResult<HostingData>)> {
        let repo_to_crates = crate_spec::by_repo(crates.iter().cloned());

        // Group repos by host domain
        let mut repos_by_host: HashMap<&'static str, Vec<RepoSpec>> = crate::hash_map_with_capacity(SUPPORTED_HOSTS.len());
        let mut crates_by_host: HashMap<&'static str, HashMap<RepoSpec, Vec<CrateSpec>>> =
            crate::hash_map_with_capacity(SUPPORTED_HOSTS.len());
        let mut unknown_host_crates: Vec<(CrateSpec, CompactString)> = Vec::new();

        for (repo_spec, crate_specs) in repo_to_crates {
            let host_domain = repo_spec.host();

            // Check if this host is supported
            if let Some(host) = SUPPORTED_HOSTS.iter().find(|h| h.host_domain == host_domain) {
                repos_by_host.entry(host.host_domain).or_default().push(repo_spec.clone());
                let _ = crates_by_host.entry(host.host_domain).or_default().insert(repo_spec, crate_specs);
            } else {
                let filename = Self::get_cache_filename(host_domain, repo_spec.owner(), repo_spec.repo());
                let reason: CompactString = format!("unsupported hosting provider: {host_domain}").into();

                match self.cache.load::<CachedRepo>(&filename) {
                    CacheResult::Miss => {
                        log::debug!(target: LOG_TARGET, "Unsupported host '{host_domain}', cannot fetch hosting data for {repo_spec}");
                        let _ = self.cache.save_no_data(&filename, reason.as_str());
                    }
                    _ => {
                        log::debug!(target: LOG_TARGET, "Using cached unsupported-host result for '{repo_spec}'");
                    }
                }

                for crate_spec in crate_specs {
                    unknown_host_crates.push((crate_spec, reason.clone()));
                }
            }
        }

        // Track requests for each supported host
        for repos in repos_by_host.values() {
            tracker.add_requests(TrackedTopic::Repos, repos.len() as u64);
        }

        // Process each supported host in parallel
        // Dispatch all repos across all hosts through the throttler
        let mut fetch_futures = Vec::new();
        for (host, client) in &self.hosts {
            if let Some(repos) = repos_by_host.remove(host.host_domain) {
                for repo_spec in repos {
                    fetch_futures.push(self.fetch_with_retry(client, host, repo_spec, tracker));
                }
            }
        }

        let all_results = join_all(fetch_futures).await;

        // Merge all repo-to-crates maps for efficient lookup
        let mut repo_to_crates_all = HashMap::default();
        for crates_map in crates_by_host.into_values() {
            repo_to_crates_all.extend(crates_map);
        }

        // Flatten results and map back to crates
        let known_host_results = all_results.into_iter().flat_map(move |repo_data| {
            let crate_specs = repo_to_crates_all.remove(&repo_data.repo_spec).expect("repo_spec must exist");
            crate_specs
                .into_iter()
                .map(move |crate_spec| (crate_spec, repo_data.result.clone()))
        });

        // Create error results for crates from unknown hosts
        let unknown_host_results = unknown_host_crates
            .into_iter()
            .map(|(crate_spec, reason)| (crate_spec, ProviderResult::Unavailable(reason)));

        // Chain all results together
        known_host_results.chain(unknown_host_results).inspect(|(crate_spec, result)| {
            if let ProviderResult::Error(e) = result {
                log::error!(target: LOG_TARGET, "Could not fetch hosting data for {crate_spec}: {e:#}");
            } else if let ProviderResult::Unavailable(reason) = result {
                log::warn!(target: LOG_TARGET, "Hosting data unavailable for {crate_spec}: {reason}");
            }
        })
    }

    /// Fetch hosting data for a repo, retrying on rate limits.
    ///
    /// Acquires a throttler permit before each attempt. On rate limit, pauses
    /// the throttler for all concurrent tasks and retries after the pause.
    async fn fetch_with_retry(&self, client: &Client, host: &Host, repo_spec: RepoSpec, tracker: &RequestTracker) -> RepoData {
        loop {
            let _permit = self.throttler.acquire().await;
            let result = self.fetch_hosting_data_for_repo(client, host, &repo_spec).await;

            if result.is_rate_limited {
                if let Some(rl) = &result.rate_limit {
                    log::debug!(
                        target: LOG_TARGET,
                        "{} API rate limit for '{repo_spec}': {} remaining, resets at {}",
                        host.display_name,
                        rl.remaining,
                        rl.reset_at.with_timezone(&chrono::Local).format("%T")
                    );
                }
                if let Some(rate_limit) = result.rate_limit {
                    let now = Utc::now();
                    let reset_time = rate_limit.reset_at;
                    let wait_until = rate_limit_wait_until(now, reset_time);

                    if should_start_rate_limit_pause(now, wait_until) {
                        self.begin_rate_limit_pause(host, &repo_spec, tracker, now, wait_until);
                    }
                }
                continue;
            }

            tracker.complete_request(TrackedTopic::Repos);
            return result;
        }
    }

    /// Pause the throttler until `wait_until` and start reporting progress while it stays paused.
    ///
    /// Excluded from coverage: `Throttler::pause_for` returns `false` when a concurrent task
    /// already installed a pause that lasts at least as long, and losing that race cannot be
    /// forced deterministically from a test, so the `false` path is unreachable in practice
    /// under measurement.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn begin_rate_limit_pause(
        &self,
        host: &Host,
        repo_spec: &RepoSpec,
        tracker: &RequestTracker,
        now: DateTime<Utc>,
        wait_until: DateTime<Utc>,
    ) {
        let wait_duration = (wait_until - now).to_std().unwrap_or(Duration::ZERO);
        if self.throttler.pause_for(wait_duration) {
            tracker.set_topic_status(TrackedTopic::Repos, TopicStatus::Blocked);
            let formatted_time = wait_until.with_timezone(&chrono::Local).format("%T").to_string();
            log::warn!(target: LOG_TARGET, "Hit {} rate limit for repository '{repo_spec}'", host.display_name);
            if should_print_to_tracker(log::log_enabled!(log::Level::Warn)) {
                tracker.println(&format!(
                    "{} rate limit exceeded: Waiting until {formatted_time}...",
                    host.display_name
                ));
            }

            drop(tokio::spawn(Self::report_rate_limit_progress(
                Arc::clone(&self.throttler),
                tracker.clone(),
                host.display_name,
                wait_until,
                formatted_time,
                Duration::from_mins(1),
            )));
        }
    }

    /// Report progress once a minute while the throttler stays paused by a rate limit.
    ///
    /// Runs as a detached task for as long as the pause lasts, which is up to
    /// `MAX_RATE_LIMIT_WAIT_SECS`. Excluded from coverage: driving it would mean waiting
    /// out real minute-long sleeps, and its two `log_enabled` branches depend on
    /// process-wide logger state that tests cannot own.
    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn report_rate_limit_progress(
        throttler: Arc<Throttler>,
        tracker: RequestTracker,
        display_name: &'static str,
        wait_until: DateTime<Utc>,
        formatted_time: String,
        progress_interval: Duration,
    ) {
        loop {
            tokio::time::sleep(progress_interval).await;
            if !throttler.is_paused() {
                tracker.set_topic_status(TrackedTopic::Repos, TopicStatus::Active);
                log::info!(target: LOG_TARGET, "{display_name} rate limit lifted, resuming requests");
                if should_print_to_tracker(log::log_enabled!(log::Level::Info)) {
                    tracker.println(&format!("{display_name} rate limit lifted, resuming requests"));
                }
                break;
            }
            let remaining = wait_until - Utc::now();
            let remaining_mins = remaining.num_minutes();
            if should_report_remaining_minutes(remaining_mins) {
                log::info!(
                    target: LOG_TARGET,
                    "{display_name} rate limit: ~{remaining_mins} minute(s) remaining until {formatted_time}"
                );
                if should_print_to_tracker(log::log_enabled!(log::Level::Info)) {
                    tracker.println(&format!(
                        "{display_name} rate limit: ~{remaining_mins} minute(s) remaining until {formatted_time}"
                    ));
                }
            }
        }
    }

    /// Fetch repository data for a single repository
    async fn fetch_hosting_data_for_repo(&self, client: &Client, host: &Host, repo_spec: &RepoSpec) -> RepoData {
        let owner = repo_spec.owner();
        let repo = repo_spec.repo();

        let filename = Self::get_cache_filename(host.host_domain, owner, repo);
        match self.cache.load::<CachedRepo>(&filename) {
            CacheResult::Data(data) => {
                let hosting_data = compute_hosting_data(&data, &self.bug_labels);
                return RepoData::from_cache(repo_spec.clone(), ProviderResult::Found(hosting_data));
            }
            CacheResult::NoData(reason) => return RepoData::from_cache(repo_spec.clone(), ProviderResult::Unavailable(reason.into())),
            CacheResult::Miss => {}
        }

        // If the throttler is paused due to a rate limit detected by another task,
        // skip HTTP calls and signal rate-limited so the caller retries after the pause.
        if let Some(result) = self.rate_limited_result(repo_spec) {
            return result;
        }

        log::info!(target: LOG_TARGET, "Querying {} for information on repository '{repo_spec}'", host.display_name);

        // Run requests sequentially so each throttler permit produces at most one
        // concurrent HTTP request, keeping actual in-flight calls within
        // MAX_CONCURRENT_REQUESTS.
        let repo_res = self.get_repo_info(client, owner, repo).await;

        // Check for rate limiting or permanent failures in each result
        let (repo_data, repo_rate_limit) = unwrap_repo_result!(repo_res, repo_spec, "core info", self.cache, &filename);

        // Bail if another task paused the throttler while we were fetching repo info.
        // Use rate_limit: None so fetch_with_retry doesn't extend the pause with
        // primary rate limit info from the successful repo request.
        if let Some(result) = self.rate_limited_result(repo_spec) {
            return result;
        }

        let issues_res = self.get_issues_and_pulls(client, owner, repo).await;
        let (raw_issues, issues_rate_limit) = unwrap_repo_result!(
            issues_res,
            repo_spec,
            "issues and pull request info",
            self.cache,
            &filename,
            "issues/PRs"
        );

        // Use the most conservative rate limit info (the one with the least remaining quota)
        let rate_limit = [issues_rate_limit, repo_rate_limit]
            .into_iter()
            .flatten()
            .min_by_key(|rl| rl.remaining);

        // GitHub uses subscribers_count, Codeberg uses watchers_count
        let subscribers = if host.use_watchers_for_subscribers {
            repo_data.watchers_count
        } else {
            repo_data.subscribers_count
        }
        .filter(|&count| count >= 0)
        .map_or(0, i64::cast_unsigned);

        let cached_repo = CachedRepo {
            stars: u64::from(repo_data.stargazers_count.unwrap_or(0)),
            forks: u64::from(repo_data.forks_count.unwrap_or(0)),
            subscribers,
            issues: raw_issues.issues,
        };

        let total_requests = total_hosting_requests(raw_issues.request_count);
        log::debug!(target: LOG_TARGET, "Completed {total_requests} {} API request(s) for repository '{repo_spec}'", host.display_name);

        let result = match self.cache.save(&filename, &cached_repo) {
            Ok(()) => ProviderResult::Found(compute_hosting_data(&cached_repo, &self.bug_labels)),
            Err(e) => ProviderResult::Error(Arc::new(e)),
        };

        RepoData::success(repo_spec.clone(), result, rate_limit)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn rate_limited_result(&self, repo_spec: &RepoSpec) -> Option<RepoData> {
        self.throttler.is_paused().then(|| RepoData {
            repo_spec: repo_spec.clone(),
            result: ProviderResult::Error(Arc::new(ohno::app_err!("rate limited"))),
            rate_limit: None,
            is_rate_limited: true,
        })
    }

    /// Get the cache filename for a specific repository
    fn get_cache_filename(host_domain: &str, owner: &str, repo: &str) -> String {
        let safe_host = sanitize_path_component(host_domain);
        let safe_owner = sanitize_path_component(owner);
        let safe_repo = sanitize_path_component(repo);
        format!("{safe_host}/{safe_owner}/{safe_repo}.bin")
    }

    /// Construct API URL for a repository with optional path suffix
    fn repo_url(client: &Client, owner: &str, repo: &str, suffix: &str) -> String {
        format!("{}/repos/{owner}/{repo}{suffix}", client.base_url())
    }

    async fn get_repo_info(&self, client: &Client, owner: &str, repo: &str) -> HostingApiResult<Repository> {
        let url = Self::repo_url(client, owner, repo, "");

        let (resp, rate_limit) = unwrap_or_return!(client.api_call(&url).await);
        match resp.json().await {
            Ok(repo_info) => HostingApiResult::Success(repo_info, rate_limit),
            Err(e) => HostingApiResult::Failed(e.into(), rate_limit),
        }
    }

    async fn get_issues_and_pulls(&self, client: &Client, owner: &str, repo: &str) -> HostingApiResult<RawIssues> {
        let since = Utc::now() - chrono::Duration::days(ISSUE_LOOKBACK_DAYS);
        let since_str = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let mut all_issues = Vec::with_capacity(ISSUE_PAGE_SIZE as usize);
        let mut latest_rate_limit: Option<RateLimitInfo> = None;
        let mut page_num = 1u32;
        let mut request_count = 0u32;

        loop {
            request_count += 1;
            let url = format!(
                "{}/repos/{owner}/{repo}/issues?state=all&since={since_str}&per_page={ISSUE_PAGE_SIZE}&page={page_num}",
                client.base_url()
            );

            let (resp, rate_limit) = unwrap_or_return!(client.api_call(&url).await);

            // Update rate limit info - keep the most conservative (lowest remaining)
            latest_rate_limit = [latest_rate_limit, rate_limit].into_iter().flatten().min_by_key(|rl| rl.remaining);

            // Parse next page link if present
            let has_next_page = resp
                .headers()
                .get(LINK)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|link_str| link_str.contains(r#"rel="next""#));

            let issues: Vec<Issue> = match resp.json().await {
                Ok(i) => i,
                Err(e) => return HostingApiResult::Failed(e.into(), latest_rate_limit),
            };

            if issues.is_empty() {
                break;
            }

            all_issues.extend(issues.into_iter().map(CachedIssue::from));

            if !has_next_page {
                break;
            }

            // Stop paginating if another task detected a rate limit.
            // Use a minimal RateLimitInfo with reset_at=now so fetch_with_retry
            // doesn't extend the existing (short) pause with primary rate limit info
            // from successful pagination pages.
            if self.throttler.is_paused() {
                return HostingApiResult::RateLimited(RateLimitInfo {
                    remaining: 0,
                    reset_at: Utc::now(),
                });
            }

            page_num += 1;

            if page_num > MAX_ISSUE_PAGES {
                log::debug!(target: LOG_TARGET, "Reached maximum issue page limit ({MAX_ISSUE_PAGES}) for '{owner}/{repo}', stopping pagination after {} issues", all_issues.len());
                break;
            }
        }

        HostingApiResult::Success(
            RawIssues {
                issues: all_issues,
                request_count,
            },
            latest_rate_limit,
        )
    }
}

/// Raw issue records fetched from the hosting API, before any aggregation.
struct RawIssues {
    issues: Vec<CachedIssue>,
    request_count: u32,
}

fn rate_limit_wait_until(now: DateTime<Utc>, reset_time: DateTime<Utc>) -> DateTime<Utc> {
    reset_time.min(now + chrono::Duration::seconds(MAX_RATE_LIMIT_WAIT_SECS.cast_signed()))
}

fn should_start_rate_limit_pause(now: DateTime<Utc>, wait_until: DateTime<Utc>) -> bool {
    wait_until > now
}

const fn should_print_to_tracker(log_enabled: bool) -> bool {
    !log_enabled
}

const fn should_report_remaining_minutes(remaining_mins: i64) -> bool {
    remaining_mins > 0
}

const fn total_hosting_requests(issue_request_count: u32) -> u32 {
    1 + issue_request_count
}

/// Compute age statistics from an iterator of durations in seconds.
fn compute_age_stats(seconds_iter: impl Iterator<Item = f64>) -> AgeStats {
    let mut seconds: Vec<f64> = seconds_iter.filter(|&s| s.is_finite() && s >= 0.0).collect();

    compute_age_stats_from_vec(&mut seconds)
}

/// Compute age statistics from a pre-collected vector of durations in seconds.
/// Sorts the vector in place to compute percentiles.
#[expect(clippy::cast_precision_loss, reason = "acceptable for statistics")]
#[expect(clippy::cast_possible_truncation, reason = "acceptable for day conversion")]
#[expect(clippy::cast_sign_loss, reason = "values are filtered to be non-negative")]
fn compute_age_stats_from_vec(seconds: &mut [f64]) -> AgeStats {
    if seconds.is_empty() {
        return AgeStats::default();
    }

    seconds.sort_by(|a, b| a.partial_cmp(b).expect("no NaN values should be present"));

    AgeStats {
        avg: (seconds.iter().sum::<f64>() / seconds.len() as f64 / SECONDS_PER_DAY) as u32,
        p50: (percentile(seconds, 50.0) / SECONDS_PER_DAY) as u32,
        p75: (percentile(seconds, 75.0) / SECONDS_PER_DAY) as u32,
        p90: (percentile(seconds, 90.0) / SECONDS_PER_DAY) as u32,
        p95: (percentile(seconds, 95.0) / SECONDS_PER_DAY) as u32,
    }
}

fn percentile(sorted_data: &[f64], percentile: f64) -> f64 {
    if sorted_data.is_empty() {
        return 0.0;
    }

    #[expect(clippy::cast_possible_truncation, reason = "index calculation")]
    #[expect(clippy::cast_sign_loss, reason = "value is clamped to non-negative range")]
    #[expect(clippy::cast_precision_loss, reason = "index fits in usize")]
    let idx = (percentile / 100.0 * (sorted_data.len() - 1) as f64)
        .round()
        .clamp(0.0, (sorted_data.len() - 1) as f64) as usize;
    sorted_data[idx]
}

/// Aggregated statistics for one family of issues (all issues, or the bug subset).
struct IssueFamilyStats {
    open: u64,
    opened: TimeWindowStats,
    closed: TimeWindowStats,
    open_age: AgeStats,
    closed_age: AgeStats,
    closed_age_last_90_days: AgeStats,
    closed_age_last_180_days: AgeStats,
    closed_age_last_365_days: AgeStats,
}

/// Aggregated pull request statistics.
struct PullRequestStats {
    open: u64,
    opened: TimeWindowStats,
    merged: TimeWindowStats,
    closed: TimeWindowStats,
    open_age: AgeStats,
    merged_age: AgeStats,
    merged_age_last_90_days: AgeStats,
    merged_age_last_180_days: AgeStats,
    merged_age_last_365_days: AgeStats,
}

/// Time window cutoffs used when bucketing timestamps.
#[derive(Clone, Copy)]
#[expect(
    clippy::struct_field_names,
    reason = "the shared prefix names the time unit, which each field needs"
)]
struct Cutoffs {
    days_90: DateTime<Utc>,
    days_180: DateTime<Utc>,
    days_365: DateTime<Utc>,
}

impl Cutoffs {
    fn new(now: DateTime<Utc>) -> Self {
        Self {
            days_90: now - chrono::Duration::days(90),
            days_180: now - chrono::Duration::days(180),
            days_365: now - chrono::Duration::days(365),
        }
    }
}

/// Increment time window counters for a given timestamp.
fn increment_window(stats: &mut TimeWindowStats, ts: DateTime<Utc>, cutoffs: Cutoffs) {
    stats.total += 1;
    if ts >= cutoffs.days_365 {
        stats.last_365_days += 1;
        if ts >= cutoffs.days_180 {
            stats.last_180_days += 1;
            if ts >= cutoffs.days_90 {
                stats.last_90_days += 1;
            }
        }
    }
}

/// Accumulates ages into nested time-window buckets.
///
/// Since `90-day ⊂ 180-day ⊂ 365-day ⊂ all`, each item is pushed to every applicable bucket.
#[derive(Default)]
struct AgeBuckets {
    all: Vec<f64>,
    days_365: Vec<f64>,
    days_180: Vec<f64>,
    days_90: Vec<f64>,
}

impl AgeBuckets {
    fn push(&mut self, age_seconds: f64, event_at: DateTime<Utc>, cutoffs: Cutoffs) {
        if !age_seconds.is_finite() || age_seconds < 0.0 {
            return;
        }

        self.all.push(age_seconds);
        if event_at >= cutoffs.days_365 {
            self.days_365.push(age_seconds);
            if event_at >= cutoffs.days_180 {
                self.days_180.push(age_seconds);
                if event_at >= cutoffs.days_90 {
                    self.days_90.push(age_seconds);
                }
            }
        }
    }

    /// Returns age statistics for the all-time, 365-day, 180-day, and 90-day buckets.
    fn finish(mut self) -> (AgeStats, AgeStats, AgeStats, AgeStats) {
        (
            compute_age_stats_from_vec(&mut self.all),
            compute_age_stats_from_vec(&mut self.days_365),
            compute_age_stats_from_vec(&mut self.days_180),
            compute_age_stats_from_vec(&mut self.days_90),
        )
    }
}

/// Compute statistics for a family of issues (excludes pull requests).
///
/// This is called once for all issues and again for the bug subset, so the two
/// metric families are guaranteed to be computed identically.
#[expect(clippy::cast_precision_loss, reason = "acceptable for duration")]
fn compute_issue_family_stats(issues: &[&CachedIssue], now: DateTime<Utc>) -> IssueFamilyStats {
    let cutoffs = Cutoffs::new(now);

    let mut opened = TimeWindowStats::default();
    let mut closed = TimeWindowStats::default();
    let mut open_ages = Vec::new();
    let mut closed_ages = AgeBuckets::default();

    for issue in issues {
        increment_window(&mut opened, issue.created_at, cutoffs);

        if let Some(closed_at) = issue.closed_at {
            increment_window(&mut closed, closed_at, cutoffs);

            // Age statistics partition issues by current state, so a reopened issue
            // (open, but carrying the `closed_at` of a previous closure) contributes
            // only to the open-age statistics.
            if !issue.is_open {
                closed_ages.push((closed_at - issue.created_at).num_seconds() as f64, closed_at, cutoffs);
            }
        }

        if issue.is_open {
            open_ages.push((now - issue.created_at).num_seconds() as f64);
        }
    }

    let open_age = compute_age_stats(open_ages.into_iter());
    let (closed_age, closed_age_last_365_days, closed_age_last_180_days, closed_age_last_90_days) = closed_ages.finish();

    IssueFamilyStats {
        open: issues.iter().filter(|issue| issue.is_open).count() as u64,
        opened,
        closed,
        open_age,
        closed_age,
        closed_age_last_90_days,
        closed_age_last_180_days,
        closed_age_last_365_days,
    }
}

/// Compute statistics for pull requests.
#[expect(clippy::cast_precision_loss, reason = "acceptable for duration")]
fn compute_pull_request_stats(pulls: &[&CachedIssue], now: DateTime<Utc>) -> PullRequestStats {
    let cutoffs = Cutoffs::new(now);

    let mut opened = TimeWindowStats::default();
    let mut merged = TimeWindowStats::default();
    let mut closed = TimeWindowStats::default();
    let mut open_ages = Vec::new();
    let mut merged_ages = AgeBuckets::default();

    for pull in pulls {
        increment_window(&mut opened, pull.created_at, cutoffs);

        if let Some(closed_at) = pull.closed_at {
            increment_window(&mut closed, closed_at, cutoffs);
        }

        if let Some(merged_at) = pull.merged_at {
            increment_window(&mut merged, merged_at, cutoffs);
            merged_ages.push((merged_at - pull.created_at).num_seconds() as f64, merged_at, cutoffs);
        }

        if pull.is_open {
            open_ages.push((now - pull.created_at).num_seconds() as f64);
        }
    }

    let open_age = compute_age_stats(open_ages.into_iter());
    let (merged_age, merged_age_last_365_days, merged_age_last_180_days, merged_age_last_90_days) = merged_ages.finish();

    PullRequestStats {
        open: pulls.iter().filter(|pull| pull.is_open).count() as u64,
        opened,
        merged,
        closed,
        open_age,
        merged_age,
        merged_age_last_90_days,
        merged_age_last_180_days,
        merged_age_last_365_days,
    }
}

/// Compute the share of issues carrying at least one label, as a percentage (0-100).
///
/// This lets expressions distinguish "this repository has no bugs" from "this repository
/// does not label its issues", which would otherwise both report zero bugs.
#[expect(clippy::cast_precision_loss, reason = "issue counts are far below f64 precision limits")]
#[expect(clippy::cast_possible_truncation, reason = "value is clamped to 0-100")]
#[expect(clippy::cast_sign_loss, reason = "value is non-negative")]
fn compute_labeled_issue_ratio(issues: &[&CachedIssue]) -> u32 {
    if issues.is_empty() {
        return 0;
    }

    let labeled = issues.iter().filter(|issue| !issue.labels.is_empty()).count();
    ((labeled as f64 / issues.len() as f64) * 100.0).round().clamp(0.0, 100.0) as u32
}

/// Compute the full set of hosting metrics from raw cached repository data.
///
/// Bug metrics are a strict subset of the corresponding issue metrics: an issue counted as a
/// bug is also counted in the general issue metrics. Issues with no labels are never counted
/// as bugs; use `labeled_issue_ratio` to detect repositories that do not label issues at all.
pub(super) fn compute_hosting_data(repo: &CachedRepo, bug_labels: &BugLabelMatcher) -> HostingData {
    let now = Utc::now();

    let issues: Vec<&CachedIssue> = repo.issues.iter().filter(|issue| !issue.is_pr).collect();
    let pulls: Vec<&CachedIssue> = repo.issues.iter().filter(|issue| issue.is_pr).collect();
    let bugs: Vec<&CachedIssue> = issues.iter().copied().filter(|issue| issue.is_bug(bug_labels)).collect();

    let issue_stats = compute_issue_family_stats(&issues, now);
    let bug_stats = compute_issue_family_stats(&bugs, now);
    let pr_stats = compute_pull_request_stats(&pulls, now);

    HostingData {
        stars: repo.stars,
        forks: repo.forks,
        subscribers: repo.subscribers,

        open_issues: issue_stats.open,
        open_issue_age: issue_stats.open_age,
        issues_opened: issue_stats.opened,
        issues_closed: issue_stats.closed,
        closed_issue_age: issue_stats.closed_age,
        closed_issue_age_last_90_days: issue_stats.closed_age_last_90_days,
        closed_issue_age_last_180_days: issue_stats.closed_age_last_180_days,
        closed_issue_age_last_365_days: issue_stats.closed_age_last_365_days,

        open_bugs: bug_stats.open,
        open_bug_age: bug_stats.open_age,
        bugs_opened: bug_stats.opened,
        bugs_closed: bug_stats.closed,
        closed_bug_age: bug_stats.closed_age,
        closed_bug_age_last_90_days: bug_stats.closed_age_last_90_days,
        closed_bug_age_last_180_days: bug_stats.closed_age_last_180_days,
        closed_bug_age_last_365_days: bug_stats.closed_age_last_365_days,
        labeled_issue_ratio: compute_labeled_issue_ratio(&issues),

        open_prs: pr_stats.open,
        open_pr_age: pr_stats.open_age,
        prs_opened: pr_stats.opened,
        prs_merged: pr_stats.merged,
        prs_closed: pr_stats.closed,
        merged_pr_age: pr_stats.merged_age,
        merged_pr_age_last_90_days: pr_stats.merged_age_last_90_days,
        merged_pr_age_last_180_days: pr_stats.merged_age_last_180_days,
        merged_pr_age_last_365_days: pr_stats.merged_age_last_365_days,
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use core::sync::atomic::{AtomicU64, Ordering};

    use semver::Version;

    use super::*;

    #[test]
    fn test_percentile_empty() {
        assert!(percentile(&[], 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_percentile_single_element() {
        assert!((percentile(&[42.0], 50.0) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_percentile_median() {
        let data = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&data, 50.0) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_percentile_75th() {
        let data = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!((percentile(&data, 75.0) - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_percentile_95th() {
        let data = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert!((percentile(&data, 95.0) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_age_stats_empty() {
        let stats = compute_age_stats(core::iter::empty());
        assert_eq!(stats.avg, 0);
        assert_eq!(stats.p50, 0);
        assert_eq!(stats.p75, 0);
        assert_eq!(stats.p90, 0);
        assert_eq!(stats.p95, 0);
    }

    #[test]
    fn test_compute_age_stats_open_issues() {
        let seconds_per_day = 86400.0_f64;
        let stats = compute_age_stats([10.0, 20.0, 5.0].iter().map(|&days| days * seconds_per_day));
        // Average of 5, 10, 20 = 11.67 days
        assert!(stats.avg >= 11 && stats.avg <= 12);
        assert!(stats.p50 >= 9 && stats.p50 <= 11);
    }

    #[test]
    fn test_compute_age_stats_closed_issues() {
        let seconds_per_day = 86400.0_f64;
        // First issue was open for 10 days, second for 5 days
        let stats = compute_age_stats([10.0, 5.0].iter().map(|&days| days * seconds_per_day));
        // Average around 7.5 days
        assert!(stats.avg >= 7 && stats.avg <= 8);
    }

    fn test_cache() -> Cache {
        Cache::new("test_cache", Duration::from_hours(1), false)
    }

    static NEXT_CACHE_DIR_ID: AtomicU64 = AtomicU64::new(0);

    fn test_cache_dir(name: &str) -> std::path::PathBuf {
        let id = NEXT_CACHE_DIR_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()
            .expect("tests run with a current directory")
            .join("target")
            .join("cargo-aprz-lib-hosting-tests")
            .join(format!("{name}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).expect("test cache directory is creatable under target");
        path
    }

    #[test]
    fn test_get_cache_filename() {
        let filename = Provider::get_cache_filename("github.com", "tokio-rs", "tokio");

        assert!(filename.contains("github.com"));
        assert!(filename.contains("tokio-rs"));
        assert!(filename.contains("tokio.bin"));
    }

    #[test]
    fn test_get_cache_filename_sanitized() {
        let filename = Provider::get_cache_filename("evil.com", "../../../etc", "passwd");

        // Path traversal should be sanitized
        assert!(!filename.contains("../"));
        assert!(filename.contains("passwd.bin"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime")]
    fn test_repo_url() {
        let client = Client::new(None, "https://api.github.com").unwrap();

        let url = Provider::repo_url(&client, "tokio-rs", "tokio", "");
        assert_eq!(url, "https://api.github.com/repos/tokio-rs/tokio");

        let url_with_suffix = Provider::repo_url(&client, "tokio-rs", "tokio", "/commits");
        assert_eq!(url_with_suffix, "https://api.github.com/repos/tokio-rs/tokio/commits");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime")]
    fn test_repo_data_from_cache() {
        let repo_spec = RepoSpec::parse(&url::Url::parse("https://github.com/tokio-rs/tokio").unwrap()).unwrap();
        let hosting_data = HostingData {
            stars: 1000,
            forks: 200,
            subscribers: 50,
            open_issues: 10,
            open_prs: 5,
            ..Default::default()
        };

        let repo_data = RepoData::from_cache(repo_spec.clone(), ProviderResult::Found(hosting_data));

        assert_eq!(repo_data.repo_spec, repo_spec);
        assert!(matches!(repo_data.result, ProviderResult::Found(_)));
        assert!(!repo_data.is_rate_limited);
        assert!(repo_data.rate_limit.is_none());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime")]
    fn test_repo_data_success() {
        let repo_spec = RepoSpec::parse(&url::Url::parse("https://github.com/tokio-rs/tokio").unwrap()).unwrap();
        let hosting_data = HostingData {
            stars: 1000,
            forks: 200,
            subscribers: 50,
            open_issues: 10,
            open_prs: 5,
            ..Default::default()
        };

        let rate_limit = Some(RateLimitInfo {
            remaining: 5000,
            reset_at: DateTime::from_timestamp(1_234_567_890, 0).unwrap(),
        });

        let repo_data = RepoData::success(repo_spec.clone(), ProviderResult::Found(hosting_data), rate_limit);

        assert_eq!(repo_data.repo_spec, repo_spec);
        assert!(!repo_data.is_rate_limited);
        assert!(repo_data.rate_limit.is_some());
        assert_eq!(repo_data.rate_limit.unwrap().remaining, 5000);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime")]
    fn test_provider_new() {
        let provider = Provider::new(
            None,
            None,
            test_cache(),
            Arc::new(BugLabelMatcher::default()),
            &Endpoints::default(),
        )
        .unwrap();
        assert_eq!(provider.hosts.len(), 2); // GitHub and Codeberg
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetSystemTimePreciseAsFileTime")]
    fn test_provider_new_with_tokens() {
        let provider = Provider::new(
            Some("github_token"),
            Some("codeberg_token"),
            test_cache(),
            Arc::new(BugLabelMatcher::default()),
            &Endpoints::default(),
        )
        .unwrap();
        assert_eq!(provider.hosts.len(), 2);
    }

    #[test]
    fn test_compute_age_stats_filters_nan_and_negative() {
        let stats = compute_age_stats([f64::NAN, f64::INFINITY, -100.0, 86400.0].into_iter());
        // Only 86400.0 (1 day) should be counted
        assert_eq!(stats.avg, 1);
        assert_eq!(stats.p50, 1);
    }

    #[test]
    fn test_compute_age_stats_divides_seconds_into_days() {
        let seconds_per_day = 86400.0_f64;
        let stats = compute_age_stats([2.0, 4.0, 6.0].into_iter().map(|days| days * seconds_per_day));

        assert_eq!(stats.avg, 4);
        assert_eq!(stats.p50, 4);
        assert_eq!(stats.p75, 6);
        assert_eq!(stats.p90, 6);
        assert_eq!(stats.p95, 6);
    }

    #[test]
    fn test_rate_limit_wait_helpers_cap_and_use_strict_future() {
        let now = DateTime::from_timestamp(1_704_067_200, 0).expect("fixed timestamp is valid");

        assert_eq!(
            rate_limit_wait_until(now, now + chrono::Duration::seconds(7_200)),
            now + chrono::Duration::seconds(MAX_RATE_LIMIT_WAIT_SECS.cast_signed())
        );
        assert!(should_start_rate_limit_pause(now, now + chrono::Duration::seconds(1)));
        assert!(!should_start_rate_limit_pause(now, now));
        assert!(!should_start_rate_limit_pause(now, now - chrono::Duration::seconds(1)));
    }

    #[test]
    fn test_rate_limit_progress_helpers() {
        assert!(should_print_to_tracker(false));
        assert!(!should_print_to_tracker(true));
        assert!(should_report_remaining_minutes(1));
        assert!(!should_report_remaining_minutes(0));
        assert!(!should_report_remaining_minutes(-1));
    }

    #[test]
    fn test_total_hosting_requests_counts_repo_request_plus_issue_pages() {
        assert_eq!(total_hosting_requests(3), 4);
    }

    fn bug_patterns() -> BugLabelMatcher {
        BugLabelMatcher::new(&[
            "bug".to_string(),
            "crash".to_string(),
            "defect".to_string(),
            "regression".to_string(),
        ])
        .unwrap()
    }

    fn issue(created_at: DateTime<Utc>, closed_at: Option<DateTime<Utc>>, labels: &[&str]) -> CachedIssue {
        CachedIssue {
            created_at,
            closed_at,
            is_open: closed_at.is_none(),
            is_pr: false,
            merged_at: None,
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
        }
    }

    fn pull(created_at: DateTime<Utc>, closed_at: Option<DateTime<Utc>>, merged_at: Option<DateTime<Utc>>) -> CachedIssue {
        CachedIssue {
            created_at,
            closed_at,
            is_open: closed_at.is_none(),
            is_pr: true,
            merged_at,
            labels: Vec::new(),
        }
    }

    fn repo(issues: Vec<CachedIssue>) -> CachedRepo {
        CachedRepo {
            stars: 1,
            forks: 2,
            subscribers: 3,
            issues,
        }
    }

    #[test]
    fn test_increment_window_recent() {
        let now = Utc::now();
        let cutoffs = Cutoffs::new(now);

        let mut stats = TimeWindowStats::default();
        increment_window(&mut stats, now - chrono::Duration::days(10), cutoffs);
        assert_eq!(stats.total, 1);
        assert_eq!(stats.last_90_days, 1);
        assert_eq!(stats.last_180_days, 1);
        assert_eq!(stats.last_365_days, 1);
    }

    #[test]
    fn test_increment_window_old() {
        let now = Utc::now();
        let cutoffs = Cutoffs::new(now);

        let mut stats = TimeWindowStats::default();
        increment_window(&mut stats, now - chrono::Duration::days(200), cutoffs);
        assert_eq!(stats.total, 1);
        assert_eq!(stats.last_90_days, 0);
        assert_eq!(stats.last_180_days, 0);
        assert_eq!(stats.last_365_days, 1);
    }

    #[test]
    fn test_increment_window_very_old() {
        let now = Utc::now();
        let cutoffs = Cutoffs::new(now);

        let mut stats = TimeWindowStats::default();
        increment_window(&mut stats, now - chrono::Duration::days(400), cutoffs);
        assert_eq!(stats.total, 1);
        assert_eq!(stats.last_90_days, 0);
        assert_eq!(stats.last_180_days, 0);
        assert_eq!(stats.last_365_days, 0);
    }

    #[test]
    fn test_compute_hosting_data_empty() {
        let data = compute_hosting_data(&repo(Vec::new()), &bug_patterns());
        assert_eq!(data.open_issues, 0);
        assert_eq!(data.open_prs, 0);
        assert_eq!(data.open_bugs, 0);
        assert_eq!(data.issues_opened.total, 0);
        assert_eq!(data.bugs_opened.total, 0);
        assert_eq!(data.labeled_issue_ratio, 0);
    }

    #[test]
    fn test_compute_hosting_data_mixed_issues_and_prs() {
        let now = Utc::now();
        let day_ago = now - chrono::Duration::days(1);
        let week_ago = now - chrono::Duration::days(7);
        let two_days_ago = now - chrono::Duration::days(2);

        let data = compute_hosting_data(
            &repo(vec![
                issue(week_ago, None, &[]),
                issue(week_ago, Some(day_ago), &[]),
                pull(two_days_ago, None, None),
                pull(week_ago, Some(two_days_ago), Some(two_days_ago)),
            ]),
            &bug_patterns(),
        );

        assert_eq!(data.open_issues, 1);
        assert_eq!(data.open_prs, 1);
        assert_eq!(data.issues_opened.total, 2);
        assert_eq!(data.issues_closed.total, 1);
        assert_eq!(data.prs_opened.total, 2);
        assert_eq!(data.prs_merged.total, 1);
        assert_eq!(data.prs_closed.total, 1);
    }

    #[test]
    fn test_repo_core_fields_are_passed_through() {
        let data = compute_hosting_data(&repo(Vec::new()), &bug_patterns());
        assert_eq!(data.stars, 1);
        assert_eq!(data.forks, 2);
        assert_eq!(data.subscribers, 3);
    }

    #[test]
    fn test_unlabeled_repo_reports_issues_but_no_bugs() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let data = compute_hosting_data(
            &repo(vec![
                issue(week_ago, None, &[]),
                issue(week_ago, None, &[]),
                issue(week_ago, Some(now - chrono::Duration::days(1)), &[]),
            ]),
            &bug_patterns(),
        );

        assert_eq!(data.open_issues, 2);
        assert_eq!(data.issues_opened.total, 3);
        assert_eq!(data.open_bugs, 0, "unlabeled issues must not count as bugs");
        assert_eq!(data.bugs_opened.total, 0);
        assert_eq!(data.labeled_issue_ratio, 0, "ratio must flag the repo as unlabeled");
    }

    #[test]
    fn test_bugs_are_a_subset_of_issues() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);
        let day_ago = now - chrono::Duration::days(1);

        let data = compute_hosting_data(
            &repo(vec![
                issue(week_ago, None, &["C-bug"]),
                issue(week_ago, None, &["enhancement"]),
                issue(week_ago, None, &[]),
                issue(week_ago, Some(day_ago), &["bug"]),
                issue(week_ago, Some(day_ago), &["documentation"]),
                pull(week_ago, None, None),
            ]),
            &bug_patterns(),
        );

        assert_eq!(data.open_issues, 3);
        assert_eq!(data.open_bugs, 1);
        assert_eq!(data.issues_opened.total, 5, "PRs must not count as issues");
        assert_eq!(data.bugs_opened.total, 2);
        assert_eq!(data.issues_closed.total, 2);
        assert_eq!(data.bugs_closed.total, 1);

        assert!(data.open_bugs <= data.open_issues);
        assert!(data.bugs_opened.total <= data.issues_opened.total);
        assert!(data.bugs_closed.total <= data.issues_closed.total);
    }

    #[test]
    fn test_pr_labels_never_count_as_bugs() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let mut labeled_pr = pull(week_ago, None, None);
        labeled_pr.labels = vec!["bug".to_string()];

        let data = compute_hosting_data(&repo(vec![labeled_pr]), &bug_patterns());

        assert_eq!(data.open_bugs, 0);
        assert_eq!(data.bugs_opened.total, 0);
        assert_eq!(data.open_prs, 1);
    }

    #[test]
    fn test_empty_bug_labels_disables_bug_classification() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let data = compute_hosting_data(&repo(vec![issue(week_ago, None, &["bug"])]), &BugLabelMatcher::default());

        assert_eq!(data.open_issues, 1);
        assert_eq!(data.open_bugs, 0);
    }

    #[test]
    fn test_custom_bug_labels_are_honored() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let patterns = BugLabelMatcher::new(&["crash".to_string()]).unwrap();
        let data = compute_hosting_data(
            &repo(vec![issue(week_ago, None, &["crash"]), issue(week_ago, None, &["bug"])]),
            &patterns,
        );

        assert_eq!(data.open_issues, 2);
        assert_eq!(data.open_bugs, 1);
    }

    #[test]
    fn test_regex_bug_labels_are_honored() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let patterns = BugLabelMatcher::new(&["^(c|kind)[-/]bug$".to_string()]).unwrap();
        let data = compute_hosting_data(
            &repo(vec![issue(week_ago, None, &["C-bug"]), issue(week_ago, None, &["type: bug"])]),
            &patterns,
        );

        assert_eq!(data.open_issues, 2);
        assert_eq!(data.open_bugs, 1);
    }

    #[test]
    fn test_bug_ages_are_computed_over_the_bug_subset_only() {
        let now = Utc::now();

        // A bug closed after 2 days and a non-bug closed after 10 days.
        let data = compute_hosting_data(
            &repo(vec![
                issue(now - chrono::Duration::days(12), Some(now - chrono::Duration::days(10)), &["bug"]),
                issue(
                    now - chrono::Duration::days(20),
                    Some(now - chrono::Duration::days(10)),
                    &["enhancement"],
                ),
            ]),
            &bug_patterns(),
        );

        assert_eq!(data.closed_bug_age.p50, 2, "bug age must ignore the non-bug issue");
        assert_eq!(data.closed_issue_age.p50, 10, "issue age must span all issues");
        assert_eq!(data.closed_bug_age_last_90_days.p50, 2);
    }

    #[test]
    fn test_reopened_issue_counts_as_open_only_for_age_stats() {
        let now = Utc::now();

        // A reopened issue: currently open, but retains the closed_at of its prior closure.
        let reopened = CachedIssue {
            created_at: now - chrono::Duration::days(100),
            closed_at: Some(now - chrono::Duration::days(50)),
            is_open: true,
            is_pr: false,
            merged_at: None,
            labels: Vec::new(),
        };

        let data = compute_hosting_data(&repo(vec![reopened]), &bug_patterns());

        assert_eq!(data.open_issues, 1);
        assert_eq!(data.open_issue_age.p50, 100);

        // Closing events still count toward the closed time-window counters, but the
        // issue must not also feed the closed-age statistics while it is open.
        assert_eq!(data.issues_closed.total, 1);
        assert_eq!(data.closed_issue_age.p50, 0, "reopened issue must not feed closed-age stats");
        assert_eq!(data.closed_issue_age.avg, 0);
    }

    #[test]
    fn test_closed_issue_without_closed_at_is_excluded_from_ages() {
        let now = Utc::now();

        let closed_without_timestamp = CachedIssue {
            created_at: now - chrono::Duration::days(10),
            closed_at: None,
            is_open: false,
            is_pr: false,
            merged_at: None,
            labels: Vec::new(),
        };

        let data = compute_hosting_data(&repo(vec![closed_without_timestamp]), &bug_patterns());

        assert_eq!(data.open_issues, 0);
        assert_eq!(data.issues_closed.total, 0);
        assert_eq!(data.closed_issue_age.p50, 0);
    }

    #[test]
    fn test_labeled_issue_ratio() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let data = compute_hosting_data(
            &repo(vec![
                issue(week_ago, None, &["enhancement"]),
                issue(week_ago, None, &["bug"]),
                issue(week_ago, None, &[]),
                issue(week_ago, None, &[]),
            ]),
            &bug_patterns(),
        );

        assert_eq!(data.labeled_issue_ratio, 50);
    }

    #[test]
    fn test_labeled_issue_ratio_ignores_pull_requests() {
        let now = Utc::now();
        let week_ago = now - chrono::Duration::days(7);

        let data = compute_hosting_data(
            &repo(vec![issue(week_ago, None, &["bug"]), pull(week_ago, None, None)]),
            &bug_patterns(),
        );

        assert_eq!(data.labeled_issue_ratio, 100);
    }

    #[test]
    fn test_open_bug_age_uses_open_bugs_only() {
        let now = Utc::now();

        let data = compute_hosting_data(
            &repo(vec![
                issue(now - chrono::Duration::days(30), None, &["bug"]),
                issue(now - chrono::Duration::days(200), Some(now - chrono::Duration::days(1)), &["bug"]),
            ]),
            &bug_patterns(),
        );

        assert_eq!(data.open_bugs, 1);
        assert_eq!(data.open_bug_age.p50, 30);
    }

    #[test]
    fn test_percentile_boundary_values() {
        let data = vec![1.0, 2.0, 3.0];
        assert!((percentile(&data, 0.0) - 1.0).abs() < f64::EPSILON);
        assert!((percentile(&data, 100.0) - 3.0).abs() < f64::EPSILON);
        assert!((percentile(&data, 150.0) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_inconsistent_timestamps_are_excluded_from_age_stats() {
        let now = Utc::now();

        // A record whose closing predates its creation cannot yield a meaningful age,
        // so it must not skew the statistics.
        let backwards_issue = CachedIssue {
            created_at: now - chrono::Duration::days(10),
            closed_at: Some(now - chrono::Duration::days(40)),
            is_open: false,
            is_pr: false,
            merged_at: None,
            labels: Vec::new(),
        };

        let backwards_pull = CachedIssue {
            created_at: now - chrono::Duration::days(10),
            closed_at: Some(now - chrono::Duration::days(40)),
            is_open: false,
            is_pr: true,
            merged_at: Some(now - chrono::Duration::days(40)),
            labels: Vec::new(),
        };

        let data = compute_hosting_data(&repo(vec![backwards_issue, backwards_pull]), &bug_patterns());

        assert_eq!(data.issues_closed.total, 1);
        assert_eq!(data.closed_issue_age.avg, 0);
        assert_eq!(data.closed_issue_age_last_90_days.avg, 0);
        assert_eq!(data.prs_merged.total, 1);
        assert_eq!(data.merged_pr_age.avg, 0);
        assert_eq!(data.merged_pr_age_last_90_days.avg, 0);
    }

    // ---------------------------------------------------------------------
    // Paths that need a `Provider` wired to a local mock server. The
    // integration suite drives the provider end-to-end; the tests below reach
    // for the private surface instead, because the states they need (a
    // throttler that is already paused) cannot be produced deterministically
    // from the outside.
    // ---------------------------------------------------------------------

    use url::Url;

    /// A provider whose GitHub client points at `github_url`.
    fn test_provider(cache_dir: &std::path::Path, github_url: &str) -> Provider {
        let endpoints = Endpoints::default().with_github_url(github_url);
        Provider::new(
            None,
            None,
            Cache::new(cache_dir, Duration::from_hours(1), false),
            Arc::new(BugLabelMatcher::default()),
            &endpoints,
        )
        .expect("the default endpoints and an empty label set always build a client")
    }

    fn github_client(provider: &Provider) -> (&Host, &Client) {
        let (host, client) = provider
            .hosts
            .iter()
            .find(|(host, _)| host.host_domain == "github.com")
            .expect("github.com is always a supported host");
        (host, client)
    }

    fn github_repo_spec() -> RepoSpec {
        RepoSpec::parse(&Url::parse("https://github.com/owner/repo").expect("valid URL")).expect("a github.com URL parses")
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn unsupported_host_miss_is_saved_as_negative_cache_entry() {
        let cache_dir = test_cache_dir("unsupported-host");
        let cache = Cache::new(cache_dir.as_path(), Duration::from_hours(1), false);
        let provider = Provider::new(
            None,
            None,
            cache.clone(),
            Arc::new(BugLabelMatcher::default()),
            &Endpoints::default(),
        )
        .expect("the default endpoints and an empty label set always build a provider");
        let repo_spec = RepoSpec::parse(&Url::parse("https://example.com/owner/repo").expect("valid URL"))
            .expect("an example.com URL parses as a repo");
        let crate_spec = CrateSpec::from_arcs_with_repo(
            Arc::from("crate-with-unknown-host"),
            Arc::new(Version::parse("1.0.0").expect("valid version")),
            repo_spec,
        );
        let crates: Arc<[CrateSpec]> = vec![crate_spec].into();
        let tracker = RequestTracker::new(&(Arc::new(NoOpProgress) as Arc<dyn crate::facts::Progress>));

        let results: Vec<_> = provider.get_hosting_data(crates, &tracker).await.collect();

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, ProviderResult::Unavailable(_)));

        let filename = Provider::get_cache_filename("example.com", "owner", "repo");
        match cache.load::<CachedRepo>(&filename) {
            CacheResult::NoData(reason) => assert_eq!(reason, "unsupported hosting provider: example.com"),
            _ => panic!("unsupported host miss must be recorded as a negative cache entry"),
        }
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn issue_pagination_uses_ten_year_lookback_next_page_number_and_request_count() {
        let server = wiremock::MockServer::start().await;
        let issue = serde_json::json!([{
            "created_at": "2024-01-01T00:00:00Z",
            "closed_at": null,
            "state": "open",
            "pull_request": null,
            "labels": [],
        }]);

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("link", r#"<https://example.invalid/next>; rel="next""#)
                    .set_body_json(issue),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let cache_dir = test_cache_dir("issue-pagination");
        let provider = test_provider(&cache_dir, &server.uri());

        let (_, client) = github_client(&provider);
        let result = provider.get_issues_and_pulls(client, "owner", "repo").await;

        let HostingApiResult::Success(raw_issues, _) = result else {
            panic!("pagination should succeed against the mock server");
        };
        assert_eq!(raw_issues.issues.len(), 1);
        assert_eq!(raw_issues.request_count, 2);

        let requests = server.received_requests().await.expect("wiremock records requests");
        assert_eq!(requests.len(), 2);
        let first_query: std::collections::BTreeMap<_, _> = requests[0].url.query_pairs().into_owned().collect();
        let second_query: std::collections::BTreeMap<_, _> = requests[1].url.query_pairs().into_owned().collect();

        assert_eq!(first_query.get("page").map(String::as_str), Some("1"));
        assert_eq!(second_query.get("page").map(String::as_str), Some("2"));

        let since = DateTime::parse_from_rfc3339(first_query.get("since").expect("issues request includes the since query parameter"))
            .expect("since is RFC3339")
            .with_timezone(&Utc);
        let age_days = (Utc::now() - since).num_days();
        let expected_ten_year_lookback_days = 3_650;
        assert!(
            ((expected_ten_year_lookback_days - 1)..=(expected_ten_year_lookback_days + 1)).contains(&age_days),
            "since should be about {expected_ten_year_lookback_days} days ago, got {age_days}"
        );
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn missing_issues_endpoint_caches_the_repository_as_unavailable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/owner/repo"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "stargazers_count": 1,
                "forks_count": 2,
                "subscribers_count": 3
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/repos/owner/repo/issues"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let cache_dir = test_cache_dir("missing-issues-endpoint");
        let provider = test_provider(&cache_dir, &server.uri());
        let (host, client) = github_client(&provider);
        let repo_spec = github_repo_spec();

        let result = provider.fetch_hosting_data_for_repo(client, host, &repo_spec).await;

        assert!(matches!(result.result, ProviderResult::Unavailable(_)));
        let filename = Provider::get_cache_filename("github.com", "owner", "repo");
        assert!(matches!(provider.cache.load::<CachedRepo>(&filename), CacheResult::NoData(_)));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn begin_rate_limit_pause_pauses_the_throttler() {
        let cache_dir = test_cache_dir("begin-rate-limit-pause");
        let provider = test_provider(&cache_dir, "http://127.0.0.1:1");
        let tracker = RequestTracker::new(&(Arc::new(NoOpProgress) as Arc<dyn crate::facts::Progress>));
        let now = Utc::now();

        provider.begin_rate_limit_pause(
            &SUPPORTED_HOSTS[0],
            &github_repo_spec(),
            &tracker,
            now,
            now + chrono::Duration::milliseconds(200),
        );

        assert!(provider.throttler.is_paused());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn rate_limit_progress_waits_until_the_throttler_resumes() {
        let throttler = Throttler::new(1);
        let tracker = RequestTracker::new(&(Arc::new(NoOpProgress) as Arc<dyn crate::facts::Progress>));
        assert!(throttler.pause_for(Duration::from_millis(40)));

        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_millis(250),
            Provider::report_rate_limit_progress(
                Arc::clone(&throttler),
                tracker,
                "GitHub",
                Utc::now() + chrono::Duration::minutes(1),
                "soon".to_string(),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("rate-limit progress reporting must finish once the pause lifts");

        assert!(
            started.elapsed() >= Duration::from_millis(20),
            "progress reporting must wait for the active pause to lift"
        );
        assert!(!throttler.is_paused());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn a_paused_throttler_skips_the_http_calls_entirely() {
        let cache_dir = tempfile::tempdir().expect("temporary directories are creatable");
        // The unreachable address proves no request is made: any HTTP call would fail
        // with a connection error rather than the rate-limited verdict asserted below.
        let provider = test_provider(cache_dir.path(), "http://127.0.0.1:1");
        assert!(provider.throttler.pause_for(Duration::from_mins(5)));

        let (host, client) = github_client(&provider);
        let result = provider.fetch_hosting_data_for_repo(client, host, &github_repo_spec()).await;

        assert!(result.is_rate_limited);
        assert!(result.rate_limit.is_none());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn pagination_stops_when_another_task_pauses_the_throttler() {
        let server = wiremock::MockServer::start().await;
        let issue = serde_json::json!([{
            "created_at": "2024-01-01T00:00:00Z",
            "closed_at": null,
            "state": "open",
            "pull_request": null,
            "labels": [],
        }]);

        // A `next` link makes the provider want a second page, which it must not fetch.
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("link", r#"<https://example.invalid/next>; rel="next""#)
                    .set_body_json(issue),
            )
            .mount(&server)
            .await;

        let cache_dir = tempfile::tempdir().expect("temporary directories are creatable");
        let provider = test_provider(cache_dir.path(), &server.uri());
        assert!(provider.throttler.pause_for(Duration::from_mins(5)));

        let (_, client) = github_client(&provider);
        let result = provider.get_issues_and_pulls(client, "owner", "repo").await;

        assert!(matches!(result, HostingApiResult::RateLimited(_)));
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 1);
    }

    #[derive(Debug)]
    struct NoOpProgress;

    // An inert reporter: the tracker only calls a couple of these, and asserting on a
    // reporter that does nothing would prove nothing.
    #[cfg_attr(coverage_nightly, coverage(off))]
    impl crate::facts::Progress for NoOpProgress {
        fn set_phase(&self, _phase: &str) {}
        fn set_determinate(&self, _callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {}
        fn set_indeterminate(&self, _callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {}
        fn println(&self, _msg: &str) {}
        fn done(&self) {}
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn a_rate_limit_pauses_the_throttler_and_the_request_is_retried() {
        // The rate limit is also reported at debug level; evaluate those arguments too.
        crate::facts::test_logging::enable_log_argument_evaluation();

        let server = wiremock::MockServer::start().await;
        let cache_dir = tempfile::tempdir().expect("temporary directories are creatable");
        let provider = test_provider(cache_dir.path(), &server.uri());
        let tracker = RequestTracker::new(&(Arc::new(NoOpProgress) as Arc<dyn crate::facts::Progress>));
        tracker.add_requests(TrackedTopic::Repos, 1);
        assert!(
            !provider.throttler.is_paused(),
            "the retry test requires a provider that is not already paused"
        );

        // The first attempt is rate limited with a reset a few seconds out; the retry that
        // follows the pause gets a plain 404, which ends the loop.
        let reset_at = DateTime::from_timestamp(Utc::now().timestamp() + 6, 0).expect("current timestamp plus six seconds is valid");
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset_at.timestamp().to_string().as_str()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let (host, client) = github_client(&provider);
        let remaining_before_request = (reset_at - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        assert!(
            remaining_before_request > Duration::from_secs(1),
            "test setup must leave enough future reset time to observe the pause"
        );
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            provider.fetch_with_retry(client, host, github_repo_spec(), &tracker),
        )
        .await
        .expect("rate-limit retry should complete shortly after the advertised reset time");

        let timing_slack = Duration::from_millis(250);
        assert!(
            started.elapsed() + timing_slack >= remaining_before_request,
            "a future reset time must pause before retrying the request"
        );
        assert!(Utc::now() >= reset_at, "the retry must happen after the advertised reset time");
        assert!(!result.is_rate_limited, "the retry after the pause must not be rate limited");
        assert!(matches!(result.result, ProviderResult::Unavailable(_)));
        assert!(server.received_requests().await.is_some_and(|r| r.len() >= 2));
    }

    #[test]
    fn age_buckets_only_fill_the_windows_an_event_falls_into() {
        let now = Utc::now();
        let cutoffs = Cutoffs::new(now);
        let mut buckets = AgeBuckets::default();

        buckets.push(0.0, now, cutoffs);
        assert_eq!(buckets.all.len(), 1, "a zero-second age is valid and must be retained");

        // One event per window, plus one older than every window.
        buckets.push(1.0, now, cutoffs);
        buckets.push(2.0, now - chrono::Duration::days(120), cutoffs);
        buckets.push(3.0, now - chrono::Duration::days(200), cutoffs);
        buckets.push(4.0, now - chrono::Duration::days(400), cutoffs);

        // Negative and non-finite ages are dropped outright.
        buckets.push(-1.0, now, cutoffs);
        buckets.push(f64::NAN, now, cutoffs);
        buckets.push(f64::INFINITY, now, cutoffs);

        assert_eq!(buckets.all.len(), 5);
        assert_eq!(buckets.days_365.len(), 4);
        assert_eq!(buckets.days_180.len(), 3);
        assert_eq!(buckets.days_90.len(), 2);
    }
}
