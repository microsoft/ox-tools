// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the hosting provider against a `wiremock` server.
//!
//! The provider normally talks to the GitHub and Codeberg REST APIs. Every test here
//! redirects it at a local mock server through [`Endpoints`], so no test touches the
//! network and none of them depend on an ambient `GITHUB_TOKEN`.

#![cfg(not(miri))]

use core::time::Duration;
use std::sync::Arc;

use cargo_aprz_lib::internals::facts::cache::Cache;
use cargo_aprz_lib::internals::facts::hosting::{HostingData, Provider};
use cargo_aprz_lib::internals::facts::{BugLabelMatcher, CrateSpec, Endpoints, Progress, ProviderResult, RepoSpec, RequestTracker};
use chrono::{DateTime, SecondsFormat, Utc};
use semver::Version;
use serde_json::json;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// No-op progress reporter for testing
#[derive(Debug)]
struct NoOpProgress;

impl Progress for NoOpProgress {
    fn set_phase(&self, _phase: &str) {}
    fn set_determinate(&self, _callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {}
    fn set_indeterminate(&self, _callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {}
    fn println(&self, _msg: &str) {}
    fn done(&self) {}
}

/// Builds a crate spec pointing at `https://{host}/{owner}/{repo}`.
fn spec_for(name: &str, host: &str, owner: &str, repo: &str) -> CrateSpec {
    let url = Url::parse(&format!("https://{host}/{owner}/{repo}")).expect("test URL is well formed");
    let repo_spec = RepoSpec::parse(&url).expect("test URL has an owner and a repo segment");
    CrateSpec::from_arcs_with_repo(
        Arc::from(name),
        Arc::new(Version::parse("1.0.0").expect("test version is well formed")),
        repo_spec,
    )
}

/// A GitHub-shaped repository payload.
fn repo_body(stars: i64, forks: i64, subscribers: i64) -> serde_json::Value {
    json!({
        "stargazers_count": stars,
        "forks_count": forks,
        "subscribers_count": subscribers,
    })
}

fn days_ago(days: i64) -> DateTime<Utc> {
    Utc::now() - chrono::Duration::days(days)
}

fn ts(when: DateTime<Utc>) -> String {
    when.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A GitHub-shaped issue payload.
fn issue(created_days_ago: i64, closed_days_ago: Option<i64>, labels: &[&str]) -> serde_json::Value {
    json!({
        "created_at": ts(days_ago(created_days_ago)),
        "closed_at": closed_days_ago.map(|d| ts(days_ago(d))),
        "state": if closed_days_ago.is_some() { "closed" } else { "open" },
        "labels": labels.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>(),
    })
}

/// A GitHub-shaped pull request payload, as returned by the issues endpoint.
fn pull_request(created_days_ago: i64, closed_days_ago: Option<i64>, merged_days_ago: Option<i64>) -> serde_json::Value {
    json!({
        "created_at": ts(days_ago(created_days_ago)),
        "closed_at": closed_days_ago.map(|d| ts(days_ago(d))),
        "state": if closed_days_ago.is_some() { "closed" } else { "open" },
        "labels": [],
        "pull_request": { "merged_at": merged_days_ago.map(|d| ts(days_ago(d))) },
    })
}

/// Mounts the repository endpoint, returning the given JSON body.
async fn mount_repo(server: &MockServer, owner: &str, repo: &str, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner}/{repo}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}

/// Mounts a single-page issue list.
async fn mount_issues(server: &MockServer, owner: &str, repo: &str, issues: Vec<serde_json::Value>) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner}/{repo}/issues")))
        .respond_with(ResponseTemplate::new(200).set_body_json(issues))
        .expect(1)
        .mount(server)
        .await;
}

fn bug_matcher() -> Arc<BugLabelMatcher> {
    Arc::new(BugLabelMatcher::new(&["bug".to_owned(), "crash".to_owned()]).expect("test patterns are valid regexes"))
}

/// Creates a provider whose GitHub and Codeberg base URLs both point at `server`.
fn provider_for(server: &MockServer, cache: Cache) -> Provider {
    let endpoints = Endpoints::default().with_github_url(server.uri()).with_codeberg_url(server.uri());

    Provider::new(None, None, cache, bug_matcher(), &endpoints).expect("provider construction only fails on invalid headers")
}

fn cache_in(dir: &std::path::Path, ignore: bool) -> Cache {
    Cache::new(dir, Duration::from_hours(8760), ignore)
}

/// Runs the provider over a single crate spec and returns its result.
async fn fetch_one(provider: &Provider, spec: CrateSpec) -> ProviderResult<HostingData> {
    let progress = Arc::new(NoOpProgress) as Arc<dyn Progress>;
    let tracker = RequestTracker::new(&progress);
    let mut results: Vec<_> = provider.get_hosting_data(vec![spec].into(), &tracker).await.collect();

    assert_eq!(results.len(), 1, "one crate spec must produce exactly one result");
    results.remove(0).1
}

fn expect_found(result: ProviderResult<HostingData>) -> HostingData {
    match result {
        ProviderResult::Found(data) => data,
        other => panic!("expected Found, got {other:?}"),
    }
}

fn expect_unavailable(result: ProviderResult<HostingData>) -> String {
    match result {
        ProviderResult::Unavailable(reason) => reason.to_string(),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

fn expect_error(result: ProviderResult<HostingData>) -> String {
    match result {
        ProviderResult::Error(e) => format!("{e:#}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn derives_hosting_data_from_a_github_repository() {
    let server = MockServer::start().await;
    mount_repo(&server, "microsoft", "oxidizer", repo_body(1234, 56, 78)).await;
    mount_issues(
        &server,
        "microsoft",
        "oxidizer",
        vec![
            issue(10, None, &["C-bug"]),
            issue(400, None, &[]),
            issue(120, Some(100), &["enhancement"]),
            issue(60, Some(30), &["crash"]),
            pull_request(20, Some(18), Some(18)),
            pull_request(5, None, None),
            pull_request(300, Some(299), None),
        ],
    )
    .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("oxidizer", "github.com", "microsoft", "oxidizer")).await);

    assert_eq!(data.stars, 1234);
    assert_eq!(data.forks, 56);
    assert_eq!(data.subscribers, 78);

    assert_eq!(data.open_issues, 2);
    assert_eq!(data.issues_opened.total, 4);
    assert_eq!(data.issues_opened.last_365_days, 3);
    assert_eq!(data.issues_opened.last_90_days, 2);
    assert_eq!(data.issues_closed.total, 2);
    assert_eq!(data.issues_closed.last_90_days, 1);

    // The open issues are 10 and 400 days old.
    assert_eq!(data.open_issue_age.avg, 205);
    assert_eq!(data.open_issue_age.p95, 400);
    // The closed issues took 20 and 30 days to close, and only the latter closed
    // within the last 90 days.
    assert_eq!(data.closed_issue_age.avg, 25);
    assert_eq!(data.closed_issue_age_last_90_days.avg, 30);
    assert_eq!(data.closed_issue_age_last_180_days.avg, 25);
    assert_eq!(data.closed_issue_age_last_365_days.avg, 25);

    assert_eq!(data.open_bugs, 1);
    assert_eq!(data.bugs_opened.total, 2);
    assert_eq!(data.bugs_closed.total, 1);
    assert_eq!(data.closed_bug_age.avg, 30);
    assert_eq!(data.labeled_issue_ratio, 75);

    assert_eq!(data.open_prs, 1);
    assert_eq!(data.prs_opened.total, 3);
    assert_eq!(data.prs_merged.total, 1);
    assert_eq!(data.prs_closed.total, 2);
    assert_eq!(data.merged_pr_age.avg, 2);
    assert_eq!(data.merged_pr_age_last_90_days.avg, 2);
    assert_eq!(data.open_pr_age.p50, 5);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn consumes_every_page_advertised_by_the_link_header() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 1, 1)).await;

    let next = format!(r#"<{}/repos/o/r/issues?page=2>; rel="next""#, server.uri());
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vec![issue(1, None, &[]), issue(2, None, &[])])
                .insert_header("link", next.as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue(3, None, &[])]))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.open_issues, 3, "both pages must be consumed");
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn stops_paginating_at_an_empty_page() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 1, 1)).await;

    let next = format!(r#"<{}/repos/o/r/issues?page=2>; rel="next""#, server.uri());
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vec![issue(1, None, &[])])
                .insert_header("link", next.as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.open_issues, 1);
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn stops_paginating_at_the_page_limit() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 1, 1)).await;

    // Every page claims another page follows, so only the hard page cap can stop the loop.
    let next = format!(r#"<{}/repos/o/r/issues?page=99>; rel="next""#, server.uri());
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vec![issue(1, None, &[])])
                .insert_header("link", next.as_str()),
        )
        .expect(10)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.open_issues, 10, "pagination must stop after the maximum page count");
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn retries_after_a_primary_rate_limit() {
    let server = MockServer::start().await;

    // The first repository request is rate-limited, with a reset a few seconds out so the
    // provider pauses its throttler before retrying.
    let reset_at = (Utc::now() + chrono::Duration::seconds(3)).timestamp();
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset_at.to_string().as_str()),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(repo_body(7, 8, 9))
                .insert_header("x-ratelimit-remaining", "4999")
                .insert_header("x-ratelimit-reset", reset_at.to_string().as_str()),
        )
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    mount_issues(&server, "o", "r", vec![issue(1, None, &[])]).await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.stars, 7);
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn retries_immediately_when_the_rate_limit_has_already_reset() {
    let server = MockServer::start().await;

    // A reset time in the past means there is nothing to wait for, so the provider
    // retries without pausing.
    let reset_at = (Utc::now() - chrono::Duration::seconds(30)).timestamp();
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset_at.to_string().as_str()),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(3, 2, 1)))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    mount_issues(&server, "o", "r", vec![]).await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.stars, 3);
    assert_eq!(data.open_issues, 0);
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn rate_limited_issue_page_is_retried() {
    let server = MockServer::start().await;

    // The whole repository fetch restarts after a rate limit, so the repository endpoint
    // is queried once per attempt.
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(1, 1, 1)))
        .expect(2)
        .mount(&server)
        .await;

    let reset_at = Utc::now().timestamp();
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset_at.to_string().as_str()),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue(2, None, &["bug"])]))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.open_bugs, 1);
    assert_eq!(data.stars, 1);
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn forbidden_with_quota_remaining_is_a_failure() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/o/private"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "4999")
                .insert_header("x-ratelimit-reset", Utc::now().timestamp().to_string().as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let message = expect_error(fetch_one(&provider, spec_for("c", "github.com", "o", "private")).await);

    assert!(message.contains("core info"), "unexpected error: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn missing_repository_is_unavailable_and_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/gone"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let spec = spec_for("c", "github.com", "o", "gone");

    let reason = expect_unavailable(fetch_one(&provider, spec.clone()).await);
    assert!(reason.contains("not found"), "unexpected reason: {reason}");

    // The negative result is cached, so the second call issues no request at all.
    let reason = expect_unavailable(fetch_one(&provider, spec).await);
    assert!(reason.contains("not found"), "unexpected reason: {reason}");
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn server_errors_are_reported_as_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/broken"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let message = expect_error(fetch_one(&provider, spec_for("c", "github.com", "o", "broken")).await);

    assert!(message.contains("core info"), "unexpected error: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn unparseable_repository_body_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_string("this is not json"))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let message = expect_error(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert!(message.contains("core info"), "unexpected error: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn unparseable_issue_body_is_an_error() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 1, 1)).await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{ not an array }"))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let message = expect_error(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert!(message.contains("issues and pull request info"), "unexpected error: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn codeberg_reports_watchers_as_subscribers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/msrd0/tool"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "stars_count": 500,
            "forks_count": 100,
            "watchers_count": 25,
        })))
        .expect(1)
        .mount(&server)
        .await;
    mount_issues(&server, "msrd0", "tool", vec![issue(3, None, &["bug"])]).await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("tool", "codeberg.org", "msrd0", "tool")).await);

    assert_eq!(data.stars, 500, "Codeberg reports stars as stars_count");
    assert_eq!(data.forks, 100);
    assert_eq!(data.subscribers, 25, "Codeberg reports subscribers as watchers_count");
    assert_eq!(data.open_bugs, 1);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn negative_and_missing_counts_become_zero() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "subscribers_count": -1 })))
        .expect(1)
        .mount(&server)
        .await;
    mount_issues(&server, "o", "r", vec![]).await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.stars, 0);
    assert_eq!(data.forks, 0);
    assert_eq!(data.subscribers, 0, "a negative subscriber count is clamped to zero");
    assert_eq!(data.labeled_issue_ratio, 0, "a repository with no issues has no labeled ratio");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn unsupported_hosts_are_unavailable_and_cached() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let cache = cache_in(temp.path(), false);
    let provider = provider_for(&server, cache);
    let spec = spec_for("c", "gitlab.com", "o", "r");

    // The first call records the unsupported host, the second one is served from that record.
    for _ in 0..2 {
        let reason = expect_unavailable(fetch_one(&provider, spec.clone()).await);
        assert!(
            reason.contains("unsupported hosting provider: gitlab.com"),
            "unexpected reason: {reason}"
        );
    }

    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        0,
        "unsupported hosts must not be queried"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn crates_without_a_repository_yield_no_results() {
    let server = MockServer::start().await;
    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));

    let spec = CrateSpec::from_arcs(
        Arc::from("no-repo"),
        Arc::new(Version::parse("1.0.0").expect("test version is well formed")),
    );
    let progress = Arc::new(NoOpProgress) as Arc<dyn Progress>;
    let tracker = RequestTracker::new(&progress);

    assert_eq!(provider.get_hosting_data(vec![spec].into(), &tracker).await.count(), 0);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn every_crate_sharing_a_repository_gets_the_same_result() {
    let server = MockServer::start().await;
    mount_repo(&server, "tokio-rs", "tokio", repo_body(20, 3, 4)).await;
    mount_issues(&server, "tokio-rs", "tokio", vec![issue(1, None, &[])]).await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));

    let specs = vec![
        spec_for("tokio", "github.com", "tokio-rs", "tokio"),
        spec_for("tokio-util", "github.com", "tokio-rs", "tokio"),
    ];

    let progress = Arc::new(NoOpProgress) as Arc<dyn Progress>;
    let tracker = RequestTracker::new(&progress);
    let results: Vec<_> = provider.get_hosting_data(specs.into(), &tracker).await.collect();

    assert_eq!(results.len(), 2);
    for (spec, result) in results {
        let data = expect_found(result);
        assert_eq!(data.stars, 20, "unexpected data for {spec}");
    }

    // A single repository is fetched once, no matter how many crates point at it.
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn a_second_call_is_served_from_the_cache() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(11, 12, 13)).await;
    mount_issues(&server, "o", "r", vec![issue(4, None, &["bug"])]).await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let spec = spec_for("c", "github.com", "o", "r");

    let first = expect_found(fetch_one(&provider, spec.clone()).await);
    let second = expect_found(fetch_one(&provider, spec).await);

    assert_eq!(first.stars, second.stars);
    assert_eq!(first.open_bugs, second.open_bugs);

    // Each mock is mounted with `.expect(1)`, so a second HTTP call would fail here.
    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn ignoring_the_cache_refetches_every_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(1, 2, 3)))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue(4, None, &[])]))
        .expect(2)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), true));
    let spec = spec_for("c", "github.com", "o", "r");

    let _ = expect_found(fetch_one(&provider, spec.clone()).await);
    let _ = expect_found(fetch_one(&provider, spec).await);

    server.verify().await;
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn bug_labels_select_which_issues_count_as_bugs() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 1, 1)).await;
    mount_issues(
        &server,
        "o",
        "r",
        vec![
            issue(1, None, &["C-bug"]),
            issue(2, None, &["type: crash", "P-high"]),
            issue(3, None, &["enhancement"]),
            issue(4, None, &[]),
            issue(5, Some(1), &["kind/bug"]),
            // Pull requests are never counted as bugs, whatever they are labelled.
            pull_request(6, None, None),
        ],
    )
    .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.open_bugs, 2, "only the bug-labelled open issues count");
    assert_eq!(data.bugs_opened.total, 3);
    assert_eq!(data.bugs_closed.total, 1);
    assert_eq!(data.open_issues, 4);
    assert_eq!(data.labeled_issue_ratio, 80, "four of five issues carry a label");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn a_reopened_issue_counts_only_towards_the_open_ages() {
    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 1, 1)).await;

    // An issue that was closed and then reopened still carries `closed_at`.
    Mock::given(method("GET"))
        .and(path("/repos/o/r/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![json!({
            "created_at": ts(days_ago(40)),
            "closed_at": ts(days_ago(20)),
            "state": "open",
            "labels": [],
        })]))
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let data = expect_found(fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await);

    assert_eq!(data.open_issues, 1);
    assert_eq!(data.issues_closed.total, 1, "the previous closure is still counted as an event");
    assert_eq!(data.closed_issue_age.avg, 0, "a reopened issue contributes no closed age");
    assert_eq!(data.open_issue_age.p50, 40);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn a_rate_limit_in_one_repository_pauses_the_others() {
    let server = MockServer::start().await;

    // One repository is rate-limited straight away, with a reset far enough out that the
    // other two repositories observe the paused throttler mid-fetch: one while waiting
    // for its repository record, the other between issue pages.
    let reset_at = (Utc::now() + chrono::Duration::seconds(2)).timestamp();
    Mock::given(method("GET"))
        .and(path("/repos/o/limited"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset_at.to_string().as_str()),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/limited"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(1, 0, 0)))
        .with_priority(2)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/slow-info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(repo_body(2, 0, 0))
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/slow-issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_body(3, 0, 0)))
        .mount(&server)
        .await;

    let next = format!(r#"<{}/repos/o/slow-issues/issues?page=2>; rel="next""#, server.uri());
    Mock::given(method("GET"))
        .and(path("/repos/o/slow-issues/issues"))
        .and(query_param("page", "1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vec![issue(1, None, &[])])
                .insert_header("link", next.as_str())
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/slow-issues/issues"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue(2, None, &[])]))
        .mount(&server)
        .await;

    for repo in ["limited", "slow-info"] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/{repo}/issues")))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
    }

    let temp = tempfile::tempdir().expect("temp dirs are creatable");
    let provider = provider_for(&server, cache_in(temp.path(), false));
    let specs = vec![
        spec_for("limited", "github.com", "o", "limited"),
        spec_for("slow-info", "github.com", "o", "slow-info"),
        spec_for("slow-issues", "github.com", "o", "slow-issues"),
    ];

    let progress = Arc::new(NoOpProgress) as Arc<dyn Progress>;
    let tracker = RequestTracker::new(&progress);
    let results: Vec<_> = provider.get_hosting_data(specs.into(), &tracker).await.collect();

    assert_eq!(results.len(), 3);
    let mut stars: Vec<u64> = results.into_iter().map(|(_, result)| expect_found(result).stars).collect();
    stars.sort_unstable();

    assert_eq!(
        stars,
        vec![1, 2, 3],
        "every repository must be retried to completion after the pause"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
async fn a_cache_write_failure_is_reported_as_an_error() {
    let temp = tempfile::tempdir().expect("temp dirs are creatable");

    // A regular file standing where the cache directory belongs makes `create_dir_all`
    // fail on every platform, unlike permission bits, which only bite on Unix and which
    // a sufficiently privileged process ignores.
    let cache_dir = temp.path().join("blocked");
    std::fs::write(&cache_dir, b"not a directory").expect("temp dirs are writable");

    let server = MockServer::start().await;
    mount_repo(&server, "o", "r", repo_body(1, 2, 3)).await;
    mount_issues(&server, "o", "r", vec![issue(1, None, &[])]).await;

    let provider = provider_for(&server, cache_in(&cache_dir, false));
    let result = fetch_one(&provider, spec_for("c", "github.com", "o", "r")).await;

    let message = expect_error(result);
    assert!(message.contains("creating directory"), "unexpected error: {message}");
}
