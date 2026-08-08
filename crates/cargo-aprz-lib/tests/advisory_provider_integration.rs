// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the advisory provider against a local advisory database.
//!
//! Each test builds a miniature `RustSec` database — the same `crates/<name>/RUSTSEC-*.md`
//! layout the real database uses — inside a temporary directory, and seeds the provider's
//! cache with it so no test touches the network.
//!
//! The provider's own download step is exercised only through its failure paths: `rustsec`
//! refuses any database URL that does not start with `https://`, so a successful clone
//! cannot be simulated locally.

#![cfg(not(miri))]

use core::time::Duration;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use cargo_aprz_lib::internals::facts::advisories::{AdvisoryCounts, AdvisoryData, Provider};
use cargo_aprz_lib::internals::facts::cache::Cache;
use cargo_aprz_lib::internals::facts::{CrateSpec, DEFAULT_ADVISORY_URL, Progress, ProviderResult};
use semver::Version;
use tempfile::TempDir;

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

/// An address on the loopback interface where nothing listens, so connecting to it fails
/// immediately without ever leaving the machine.
const UNREACHABLE_URL: &str = "https://127.0.0.1:1/advisory-db.git";

/// The marker the provider writes once it has synchronized the database.
const SYNC_MARKER: &str = "last_synced.bin";

/// Writes one advisory into the database rooted at `root`.
///
/// `metadata` holds the advisory fields that vary between tests (severity, informational
/// kind, withdrawal date), and `patched` the contents of the `[versions] patched` list.
fn write_advisory(root: &Path, package: &str, id: &str, metadata: &str, patched: &str) {
    let dir = root.join("crates").join(package);
    fs::create_dir_all(&dir).expect("temporary directories are writable");

    let content = format!(
        "```toml\n\
         [advisory]\n\
         id = \"{id}\"\n\
         package = \"{package}\"\n\
         date = \"2021-01-01\"\n\
         url = \"https://example.invalid/{id}\"\n\
         {metadata}\n\
         [versions]\n\
         patched = [{patched}]\n\
         ```\n\
         \n\
         # {id}\n\
         \n\
         Advisory body used by the `cargo-aprz` test fixtures.\n"
    );

    fs::write(dir.join(format!("{id}.md")), content).expect("temporary directories are writable");
}

/// CVSS v3.1 vectors chosen so that each maps to a distinct severity band.
const LOW_CVSS: &str = "cvss = \"CVSS:3.1/AV:N/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N\"\n";
const MEDIUM_CVSS: &str = "cvss = \"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N\"\n";
const HIGH_CVSS: &str = "cvss = \"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N\"\n";
const CRITICAL_CVSS: &str = "cvss = \"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H\"\n";
const NO_SEVERITY_CVSS: &str = "cvss = \"CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N\"\n";

/// Populates a database covering every advisory shape the provider distinguishes.
///
/// Vulnerabilities are patched in 2.0.0, so version 1.0.0 of each crate is affected and
/// version 2.5.0 is not. Informational advisories carry no patched versions, so they
/// apply to every version.
fn write_fixture_database(root: &Path) {
    write_advisory(root, "low-crate", "RUSTSEC-2021-0001", LOW_CVSS, "\">= 2.0.0\"");
    write_advisory(root, "medium-crate", "RUSTSEC-2021-0002", MEDIUM_CVSS, "\">= 2.0.0\"");
    write_advisory(root, "high-crate", "RUSTSEC-2021-0003", HIGH_CVSS, "\">= 2.0.0\"");
    write_advisory(root, "critical-crate", "RUSTSEC-2021-0004", CRITICAL_CVSS, "\">= 2.0.0\"");
    write_advisory(root, "unscored-crate", "RUSTSEC-2021-0005", NO_SEVERITY_CVSS, "\">= 2.0.0\"");
    write_advisory(root, "no-cvss-crate", "RUSTSEC-2021-0006", "", "\">= 2.0.0\"");
    write_advisory(
        root,
        "unmaintained-crate",
        "RUSTSEC-2021-0007",
        "informational = \"unmaintained\"\n",
        "",
    );
    write_advisory(root, "unsound-crate", "RUSTSEC-2021-0008", "informational = \"unsound\"\n", "");
    write_advisory(root, "notice-crate", "RUSTSEC-2021-0009", "informational = \"notice\"\n", "");
    write_advisory(
        root,
        "other-info-crate",
        "RUSTSEC-2021-0010",
        "informational = \"future-category\"\n",
        "",
    );
    write_advisory(
        root,
        "withdrawn-crate",
        "RUSTSEC-2021-0011",
        "informational = \"unmaintained\"\nwithdrawn = \"2021-06-01\"\n",
        "",
    );
    write_advisory(
        root,
        "withdrawn-vuln-crate",
        "RUSTSEC-2021-0012",
        &format!("{CRITICAL_CVSS}withdrawn = \"2021-06-01\"\n"),
        "\">= 2.0.0\"",
    );
    // A crate carrying two advisories at once, to check that counts accumulate.
    write_advisory(root, "double-crate", "RUSTSEC-2021-0013", HIGH_CVSS, "\">= 2.0.0\"");
    write_advisory(root, "double-crate", "RUSTSEC-2021-0014", "informational = \"unsound\"\n", "");
}

/// A cache directory pre-seeded with the fixture advisory database.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// Creates a cache directory holding an already-synchronized fixture database.
    fn new() -> Self {
        let fixture = Self {
            dir: tempfile::tempdir().expect("temporary directories are creatable"),
        };

        write_fixture_database(&fixture.dir.path().join("repo"));
        fixture.cache().save(SYNC_MARKER, &()).expect("temporary directories are writable");

        fixture
    }

    fn cache(&self) -> Cache {
        Cache::new(self.dir.path(), Duration::from_hours(8760), false)
    }

    /// Builds a provider over the fixture database.
    ///
    /// The database URL is deliberately unreachable: the seeded synchronization marker
    /// means a correct provider never attempts to fetch it.
    async fn provider(&self) -> Provider {
        Provider::new(&self.cache(), Arc::new(NoOpProgress), UNREACHABLE_URL)
            .await
            .expect("opening a seeded advisory database must succeed")
    }
}

/// Helper to create a [`CrateSpec`] from name and version strings.
fn make_spec(name: &str, version: &str) -> CrateSpec {
    CrateSpec::from_arcs(Arc::from(name), Arc::new(Version::parse(version).expect("valid version")))
}

/// Runs the provider over a single crate and returns its advisory data.
async fn data_for(provider: &Provider, name: &str, version: &str) -> AdvisoryData {
    let results: Vec<(CrateSpec, ProviderResult<AdvisoryData>)> =
        provider.get_advisory_data(vec![make_spec(name, version)].into()).await.collect();

    assert_eq!(results.len(), 1, "one crate must produce exactly one result");
    let (spec, result) = &results[0];
    assert_eq!(spec.name(), name);

    match result {
        ProviderResult::Found(data) => data.clone(),
        other => panic!("Expected Found, got {other:?}"),
    }
}

/// Total number of advisories represented by a set of counts.
fn total_count(counts: &AdvisoryCounts) -> u64 {
    counts.low_vulnerability_count
        + counts.medium_vulnerability_count
        + counts.high_vulnerability_count
        + counts.critical_vulnerability_count
        + counts.notice_warning_count
        + counts.unmaintained_warning_count
        + counts.unsound_warning_count
        + counts.yanked_warning_count
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_known_vulnerable_crate() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let data = data_for(&provider, "critical-crate", "1.0.0").await;

    assert_eq!(data.per_version.critical_vulnerability_count, 1);
    assert_eq!(data.total.critical_vulnerability_count, 1);
    assert_eq!(total_count(&data.per_version), 1);
    assert_eq!(total_count(&data.total), 1);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_clean_crate() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let data = data_for(&provider, "itoa", "1.0.14").await;

    assert_eq!(total_count(&data.per_version), 0, "a crate with no advisories must report none");
    assert_eq!(total_count(&data.total), 0);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_multiple_crates() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let crates = vec![
        make_spec("high-crate", "1.0.0"),
        make_spec("unmaintained-crate", "1.0.0"),
        make_spec("itoa", "1.0.14"),
    ];

    let results: Vec<(CrateSpec, ProviderResult<AdvisoryData>)> = provider.get_advisory_data(crates.into()).await.collect();

    assert_eq!(results.len(), 3);
    for (spec, result) in &results {
        assert!(
            matches!(result, ProviderResult::Found(_)),
            "Expected Found for {}, got {result:?}",
            spec.name()
        );
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_reports_every_version_of_the_same_crate() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let crates = vec![make_spec("high-crate", "1.0.0"), make_spec("high-crate", "2.5.0")];
    let results: Vec<(CrateSpec, ProviderResult<AdvisoryData>)> = provider.get_advisory_data(crates.into()).await.collect();

    assert_eq!(results.len(), 2, "each version is reported separately");
    for (spec, result) in &results {
        let ProviderResult::Found(data) = result else {
            panic!("Expected Found, got {result:?}");
        };

        assert_eq!(
            data.total.high_vulnerability_count, 1,
            "the advisory is historical for every version"
        );
        let expected_per_version = u64::from(spec.version() < &Version::parse("2.0.0").expect("valid version"));
        assert_eq!(data.per_version.high_vulnerability_count, expected_per_version);
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_nonexistent_crate() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let data = data_for(&provider, "this-crate-definitely-does-not-exist-xyz-98765", "0.0.1").await;

    assert_eq!(total_count(&data.total), 0, "Non-existent crate should have no advisories");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_empty_input() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    assert!(
        provider.get_advisory_data(vec![].into()).await.next().is_none(),
        "Empty input should produce empty output"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_historical_vs_per_version() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    // The advisory is patched in 2.0.0, so a later version carries it only historically.
    let data = data_for(&provider, "critical-crate", "2.5.0").await;

    assert_eq!(
        data.total.critical_vulnerability_count, 1,
        "the advisory remains part of the crate's history"
    );
    assert_eq!(total_count(&data.per_version), 0, "a patched version is not affected");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_maps_each_severity() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    assert_eq!(
        data_for(&provider, "low-crate", "1.0.0").await.per_version.low_vulnerability_count,
        1
    );
    assert_eq!(
        data_for(&provider, "medium-crate", "1.0.0")
            .await
            .per_version
            .medium_vulnerability_count,
        1
    );
    assert_eq!(
        data_for(&provider, "high-crate", "1.0.0")
            .await
            .per_version
            .high_vulnerability_count,
        1
    );
    assert_eq!(
        data_for(&provider, "critical-crate", "1.0.0")
            .await
            .per_version
            .critical_vulnerability_count,
        1
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_ignores_unscored_advisories() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    // An advisory that scores zero has no severity band, and one without a CVSS vector
    // has no score at all. Neither contributes to any count.
    for package in ["unscored-crate", "no-cvss-crate"] {
        let data = data_for(&provider, package, "1.0.0").await;
        assert_eq!(total_count(&data.per_version), 0, "unexpected counts for {package}");
        assert_eq!(total_count(&data.total), 0, "unexpected counts for {package}");
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_counts_informational_warnings() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let unmaintained = data_for(&provider, "unmaintained-crate", "1.0.0").await;
    assert_eq!(unmaintained.per_version.unmaintained_warning_count, 1);
    assert_eq!(unmaintained.total.unmaintained_warning_count, 1);

    let unsound = data_for(&provider, "unsound-crate", "1.0.0").await;
    assert_eq!(unsound.per_version.unsound_warning_count, 1);
    assert_eq!(unsound.total.unsound_warning_count, 1);

    let notice = data_for(&provider, "notice-crate", "1.0.0").await;
    assert_eq!(notice.per_version.notice_warning_count, 1);
    assert_eq!(notice.total.notice_warning_count, 1);

    // Informational kinds this tool does not model are simply not counted.
    let other = data_for(&provider, "other-info-crate", "1.0.0").await;
    assert_eq!(total_count(&other.per_version), 0);
    assert_eq!(total_count(&other.total), 0);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_accumulates_multiple_advisories() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let data = data_for(&provider, "double-crate", "1.0.0").await;

    assert_eq!(data.per_version.high_vulnerability_count, 1);
    assert_eq!(data.per_version.unsound_warning_count, 1);
    assert_eq!(total_count(&data.per_version), 2);
    assert_eq!(total_count(&data.total), 2);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_ignores_withdrawn_advisories() {
    let fixture = Fixture::new();
    let provider = fixture.provider().await;

    let warning = data_for(&provider, "withdrawn-crate", "1.0.0").await;
    assert_eq!(total_count(&warning.per_version), 0, "a withdrawn warning must not be counted");
    assert_eq!(total_count(&warning.total), 0);

    let vulnerability = data_for(&provider, "withdrawn-vuln-crate", "1.0.0").await;
    assert_eq!(
        total_count(&vulnerability.per_version),
        0,
        "a withdrawn vulnerability must not be counted"
    );
    assert_eq!(total_count(&vulnerability.total), 0);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_cache_reuse() {
    // Creating a second provider over the same cache directory must reuse the
    // synchronized database rather than fetching it again — which is observable here
    // because the configured database URL cannot be reached.
    let fixture = Fixture::new();

    let provider1 = Provider::new(&fixture.cache(), Arc::new(NoOpProgress), UNREACHABLE_URL)
        .await
        .expect("First provider creation should succeed");
    let provider2 = Provider::new(&fixture.cache(), Arc::new(NoOpProgress), UNREACHABLE_URL)
        .await
        .expect("Second provider creation with cache should succeed");

    let data1 = data_for(&provider1, "critical-crate", "1.0.0").await;
    let data2 = data_for(&provider2, "critical-crate", "1.0.0").await;

    assert_eq!(
        data1.total.critical_vulnerability_count, data2.total.critical_vulnerability_count,
        "Cached results should match"
    );
    assert_eq!(
        data1.per_version.critical_vulnerability_count, data2.per_version.critical_vulnerability_count,
        "Cached per-version results should match"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_fetches_when_the_marker_is_missing() {
    // Without the synchronization marker the provider must try to fetch the database,
    // which fails here because nothing is listening on the configured address.
    let dir = tempfile::tempdir().expect("temporary directories are creatable");
    write_fixture_database(&dir.path().join("repo"));

    let cache = Cache::new(dir.path(), Duration::from_hours(8760), false);
    let error = Provider::new(&cache, Arc::new(NoOpProgress), UNREACHABLE_URL)
        .await
        .expect_err("an unreachable database URL must not yield a provider");

    let message = format!("{error:#}");
    assert!(message.contains("downloading the advisory database"), "unexpected error: {message}");
    assert!(
        !dir.path().join(SYNC_MARKER).exists(),
        "a failed fetch must not mark the database as synchronized"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_rejects_a_non_git_database_url() {
    let dir = tempfile::tempdir().expect("temporary directories are creatable");
    let cache = Cache::new(dir.path(), Duration::from_hours(8760), false);

    let error = Provider::new(&cache, Arc::new(NoOpProgress), "not-a-git-repository")
        .await
        .expect_err("a URL that is not a git repository must not yield a provider");

    let message = format!("{error:#}");
    assert!(message.contains("downloading the advisory database"), "unexpected error: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
async fn test_advisory_provider_reports_a_missing_database_directory() {
    // The database is marked as synchronized but its directory was removed, so opening
    // it must fail rather than silently reporting every crate as advisory-free.
    let dir = tempfile::tempdir().expect("temporary directories are creatable");
    let cache = Cache::new(dir.path(), Duration::from_hours(8760), false);
    cache.save(SYNC_MARKER, &()).expect("temporary directories are writable");

    let error = Provider::new(&cache, Arc::new(NoOpProgress), UNREACHABLE_URL)
        .await
        .expect_err("an absent database directory must not yield a provider");

    let message = format!("{error:#}");
    assert!(message.contains("opening the advisory database"), "unexpected error: {message}");
}

#[test]
fn default_advisory_url_points_at_the_rustsec_database() {
    assert!(
        DEFAULT_ADVISORY_URL.starts_with("https://"),
        "the advisory database is cloned over https, got {DEFAULT_ADVISORY_URL}"
    );
}
