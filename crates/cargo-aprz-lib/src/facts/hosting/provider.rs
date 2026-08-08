// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::cached_repo::{CachedIssue, CachedRepo};
use super::client::{Client, HostingApiResult, Issue, RateLimitInfo, Repository};
use super::{AgeStats, BugLabelMatcher, HostingData, TimeWindowStats};
use crate::Result;
use crate::facts::ProviderResult;
use crate::facts::RepoSpec;
use crate::facts::cache::{Cache, CacheResult};
use crate::facts::crate_spec::{self, CrateSpec};
use crate::facts::path_utils::sanitize_path_component;
use crate::facts::request_tracker::{RequestTracker, TopicStatus, TrackedTopic};
use crate::facts::throttler::Throttler;
use chrono::{DateTime, Utc};
use compact_str::CompactString;
use core::time::Duration;
use futures_util::future::join_all;
use ohno::EnrichableExt;
use reqwest::header::LINK;
use crate::HashMap;
use std::sync::Arc;

const LOG_TARGET: &str = "   hosting";
const SECONDS_PER_DAY: f64 = 86400.0;
const ISSUE_LOOKBACK_DAYS: i64 = 365 * 10;
const ISSUE_PAGE_SIZE: u8 = 100;
const MAX_ISSUE_PAGES: u32 = 10;
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 3600;
const MAX_CONCURRENT_REQUESTS: usize = 5;

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
}

/// Supported hosting providers
static SUPPORTED_HOSTS: &[Host] = &[
    Host {
        host_domain: "github.com",
        base_url: "https://api.github.com",
        display_name: "GitHub",
        use_watchers_for_subscribers: false,
    },
    Host {
        host_domain: "codeberg.org",
        base_url: "https://codeberg.org/api/v1",
        display_name: "Codeberg",
        use_watchers_for_subscribers: true,
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
    ) -> Result<Self> {
        let mut hosts = Vec::with_capacity(SUPPORTED_HOSTS.len());

        for host in SUPPORTED_HOSTS {
            // Map host domain to appropriate token
            let token = match host.host_domain {
                "github.com" => github_token,
                "codeberg.org" => codeberg_token,
                _ => None,
            };

            let client = Client::new(token, host.base_url)?;
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
        let mut crates_by_host: HashMap<&'static str, HashMap<RepoSpec, Vec<CrateSpec>>> = crate::hash_map_with_capacity(SUPPORTED_HOSTS.len());
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
        let unknown_host_results = unknown_host_crates.into_iter().map(|(crate_spec, reason)| {
            (crate_spec, ProviderResult::Unavailable(reason))
        });

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
    async fn fetch_with_retry(
        &self,
        client: &Client,
        host: &Host,
        repo_spec: RepoSpec,
        tracker: &RequestTracker,
    ) -> RepoData {
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
                    let wait_until = reset_time.min(now + chrono::Duration::seconds(MAX_RATE_LIMIT_WAIT_SECS.cast_signed()));

                    if wait_until > now {
                        let wait_duration = (wait_until - now).to_std().unwrap_or(Duration::ZERO);
                        if self.throttler.pause_for(wait_duration) {
                            tracker.set_topic_status(TrackedTopic::Repos, TopicStatus::Blocked);
                            let formatted_time = wait_until.with_timezone(&chrono::Local).format("%T").to_string();
                            log::warn!(target: LOG_TARGET, "Hit {} rate limit for repository '{repo_spec}'", host.display_name);
                            if !log::log_enabled!(log::Level::Warn) {
                                tracker.println(&format!(
                                    "{} rate limit exceeded: Waiting until {formatted_time}...",
                                    host.display_name
                                ));
                            }

                            let throttler = Arc::clone(&self.throttler);
                            let tracker = tracker.clone();
                            let display_name = host.display_name;
                            drop(tokio::spawn(async move {
                                loop {
                                    tokio::time::sleep(Duration::from_mins(1)).await;
                                    if !throttler.is_paused() {
                                        tracker.set_topic_status(TrackedTopic::Repos, TopicStatus::Active);
                                        log::info!(target: LOG_TARGET, "{display_name} rate limit lifted, resuming requests");
                                        if !log::log_enabled!(log::Level::Info) {
                                            tracker.println(&format!("{display_name} rate limit lifted, resuming requests"));
                                        }
                                        break;
                                    }
                                    let remaining = wait_until - Utc::now();
                                    let remaining_mins = remaining.num_minutes();
                                    if remaining_mins > 0 {
                                        log::info!(
                                            target: LOG_TARGET,
                                            "{display_name} rate limit: ~{remaining_mins} minute(s) remaining until {formatted_time}"
                                        );
                                        if !log::log_enabled!(log::Level::Info) {
                                            tracker.println(&format!(
                                                "{display_name} rate limit: ~{remaining_mins} minute(s) remaining until {formatted_time}"
                                            ));
                                        }
                                    }
                                }
                            }));
                        }
                    }
                }
                continue;
            }

            tracker.complete_request(TrackedTopic::Repos);
            return result;
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
        if self.throttler.is_paused() {
            return RepoData {
                repo_spec: repo_spec.clone(),
                result: ProviderResult::Error(Arc::new(ohno::app_err!("rate limited"))),
                rate_limit: None,
                is_rate_limited: true,
            };
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
        if self.throttler.is_paused() {
            return RepoData {
                repo_spec: repo_spec.clone(),
                result: ProviderResult::Error(Arc::new(ohno::app_err!("rate limited"))),
                rate_limit: None,
                is_rate_limited: true,
            };
        }

        let issues_res = self.get_issues_and_pulls(client, owner, repo).await;
        let (raw_issues, issues_rate_limit) = unwrap_repo_result!(issues_res, repo_spec, "issues and pull request info", self.cache, &filename, "issues/PRs");

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

        let total_requests = 1 + raw_issues.request_count;
        log::debug!(target: LOG_TARGET, "Completed {total_requests} {} API request(s) for repository '{repo_spec}'", host.display_name);

        let result = match self.cache.save(&filename, &cached_repo) {
            Ok(()) => ProviderResult::Found(compute_hosting_data(&cached_repo, &self.bug_labels)),
            Err(e) => ProviderResult::Error(Arc::new(e)),
        };

        RepoData::success(repo_spec.clone(), result, rate_limit)
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

/// Compute age statistics from an iterator of durations in seconds.
fn compute_age_stats(seconds_iter: impl Iterator<Item = f64>) -> AgeStats {
    let mut seconds: Vec<f64> = seconds_iter
        .filter(|&s| s.is_finite() && s >= 0.0)
        .collect();

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
#[expect(clippy::struct_field_names, reason = "the shared prefix names the time unit, which each field needs")]
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
/// Since 90-day ⊂ 180-day ⊂ 365-day ⊂ all, each item is pushed to every applicable bucket.
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
mod tests {
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
        let provider = Provider::new(None, None, test_cache(), Arc::new(BugLabelMatcher::default())).unwrap();
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

    fn bug_patterns() -> BugLabelMatcher {
        BugLabelMatcher::new(&["bug".to_string(), "crash".to_string(), "defect".to_string(), "regression".to_string()]).unwrap()
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
                issue(now - chrono::Duration::days(20), Some(now - chrono::Duration::days(10)), &["enhancement"]),
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
    }
}
