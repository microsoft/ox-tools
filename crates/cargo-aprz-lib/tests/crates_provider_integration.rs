// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the crates.io database dump provider.
//!
//! Every test serves a synthetic dump (see `support::dump`) from a `wiremock` server, so the whole
//! pipeline runs for real — download, gzip, tar, CSV to binary table conversion, memory mapping and
//! querying — without downloading the 1.5 GB production dump.

#![cfg(not(miri))]

mod support;

use core::time::Duration as StdDuration;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use cargo_aprz_lib::internals::facts::crates::Provider;
use cargo_aprz_lib::internals::facts::{CrateRef, CratesData, Progress, ProviderResult};
use chrono::{DateTime, Duration, Utc};
use semver::Version;
use support::dump::Dump;
use support::{NoOpProgress, dump_server, dump_url, failing_server};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A generous TTL: tests that care about staleness pass a future `now` instead.
const LONG_TTL: StdDuration = StdDuration::from_hours(24 * 30);

fn progress() -> Arc<dyn Progress> {
    Arc::new(NoOpProgress)
}

async fn make_provider(cache_dir: &Path, server: &MockServer, now: DateTime<Utc>) -> Provider {
    Provider::new(cache_dir, LONG_TTL, progress(), now, false, Some(&dump_url(server)))
        .await
        .expect("the synthetic dump must be accepted by the provider")
}

/// Queries the provider and returns the results keyed by crate name.
async fn query(provider: &Provider, refs: &[CrateRef]) -> HashMap<String, ProviderResult<CratesData>> {
    provider
        .get_crates_data(refs, &NoOpProgress, true)
        .await
        .map(|(spec, result)| (spec.name().to_owned(), result))
        .collect()
}

fn crate_ref(name: &str, version: Option<&str>) -> CrateRef {
    CrateRef::new(name, version.map(|v| Version::parse(v).expect("test versions are valid semver")))
}

fn found(results: &HashMap<String, ProviderResult<CratesData>>, name: &str) -> CratesData {
    match results.get(name) {
        Some(ProviderResult::Found(data)) => data.clone(),
        other => panic!("expected '{name}' to be found, got {other:?}"),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn reports_full_facts_for_a_requested_version() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(&provider, &[crate_ref("serde", Some("1.0.200"))]).await;
    let data = found(&results, "serde");

    let version = &data.version_data;
    assert_eq!(version.description, "A generic serialization framework");
    assert_eq!(version.license, "MIT OR Apache-2.0");
    assert_eq!(version.rust_version, "1.61.0");
    assert_eq!(format!("{:?}", version.edition), "Some(Edition2018)");
    assert!(!version.yanked);
    assert_eq!(version.downloads, 900_000);
    assert_eq!(version.documentation.as_ref().map(url::Url::as_str), Some("https://docs.rs/serde"));
    assert_eq!(version.homepage.as_ref().map(url::Url::as_str), Some("https://serde.rs/"));
    assert_eq!(version.features.get("default").map(Vec::as_slice), Some(["std".into()].as_slice()));
    assert!(version.features.contains_key("derive"));

    let overall = &data.overall_data;
    assert_eq!(overall.downloads, 12_000_000);
    assert_eq!(overall.repository, None);
    assert_eq!(overall.categories, ["encoding", "parser-implementations"]);
    assert_eq!(overall.keywords, ["serialization", "no-std"]);

    // `serde_json` is the only crate in the dump depending on `serde`.
    assert_eq!(overall.dependents, 1);

    // Four versions total: one 40 days old, one 200 days old, one 10 days old and one 400 days old.
    assert_eq!(overall.versions_last_90_days, 2);
    assert_eq!(overall.versions_last_180_days, 2);
    assert_eq!(overall.versions_last_365_days, 3);

    // The daily row 200 days back is outside the 90-day window and must not be aggregated.
    let version_total: u64 = version.monthly_downloads.iter().map(|(_, count)| count).sum();
    assert_eq!(version_total, 12_000);
    let crate_total: u64 = overall.monthly_downloads.iter().map(|(_, count)| count).sum();
    assert_eq!(crate_total, 12_100, "the crate series also includes the 1.0.100 downloads");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn reports_both_kinds_of_owner() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(&provider, &[crate_ref("serde", Some("1.0.200")), crate_ref("itoa", None)]).await;

    let serde_owners = &found(&results, "serde").overall_data.owners;
    assert_eq!(serde_owners.len(), 2);
    assert_eq!(serde_owners[0].login, "alice");
    assert_eq!(serde_owners[0].name.as_deref(), Some("Alice Example"));
    assert_eq!(format!("{:?}", serde_owners[0].kind), "User");
    assert_eq!(serde_owners[1].login, "github:fake-org:maintainers");
    assert_eq!(serde_owners[1].name.as_deref(), Some("Fake Org Maintainers"));
    assert_eq!(format!("{:?}", serde_owners[1].kind), "Team");

    // `bob` has no display name in the dump, which the provider reports as `None`.
    let itoa = found(&results, "itoa");
    assert_eq!(itoa.overall_data.owners.len(), 1);
    assert_eq!(itoa.overall_data.owners[0].login, "bob");
    assert_eq!(itoa.overall_data.owners[0].name, None);

    // `itoa` also covers the columns that may be absent: no edition, no MSRV.
    assert_eq!(itoa.version_data.edition, None);
    assert_eq!(itoa.version_data.rust_version, "");

    // Both `serde` and `serde_json` depend on `itoa`.
    assert_eq!(itoa.overall_data.dependents, 2);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn resolves_latest_version_skipping_prereleases_and_yanked_versions() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let resolved: Vec<_> = provider
        .get_crates_data(&[crate_ref("serde", None)], &NoOpProgress, false)
        .await
        .collect();

    assert_eq!(resolved.len(), 1);
    let (spec, result) = &resolved[0];
    assert_eq!(spec.version().to_string(), "1.0.200", "2.0.0-alpha.1 and 1.0.150 must be skipped");
    assert!(result.is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn reports_missing_crates_and_versions() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(
        &provider,
        &[
            crate_ref("serde", Some("99.99.99")),
            crate_ref("versionless-crate", None),
            crate_ref("no-such-crate-anywhere", Some("1.0.0")),
        ],
    )
    .await;

    assert!(
        matches!(results["serde"], ProviderResult::VersionNotFound),
        "an unknown version of a known crate is VersionNotFound"
    );
    assert!(
        matches!(results["versionless-crate"], ProviderResult::VersionNotFound),
        "a crate with no published versions has no latest version to resolve"
    );
    match &results["no-such-crate-anywhere"] {
        ProviderResult::CrateNotFound(suggestions) => {
            assert!(suggestions.is_empty(), "nothing in the dump is close to that name");
        }
        other => panic!("expected CrateNotFound, got {other:?}"),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn suggests_similar_names_for_misspellings() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(&provider, &[crate_ref("serdee", Some("1.0.0"))]).await;

    match &results["serdee"] {
        ProviderResult::CrateNotFound(suggestions) => {
            assert!(
                suggestions.iter().any(|name| name == "serde"),
                "expected 'serde' among the suggestions, got {suggestions:?}"
            );
        }
        other => panic!("expected CrateNotFound, got {other:?}"),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn suppresses_suggestions_when_not_requested() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results: HashMap<_, _> = provider
        .get_crates_data(&[crate_ref("serdee", Some("1.0.0"))], &NoOpProgress, false)
        .await
        .map(|(spec, result)| (spec.name().to_owned(), result))
        .collect();

    match &results["serdee"] {
        ProviderResult::CrateNotFound(suggestions) => assert!(suggestions.is_empty()),
        other => panic!("expected CrateNotFound, got {other:?}"),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn normalizes_repository_urls() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(
        &provider,
        &[
            crate_ref("schemeless-repo-crate", Some("0.1.0")),
            crate_ref("broken-repo-crate", Some("0.1.0")),
        ],
    )
    .await;

    let schemeless = found(&results, "schemeless-repo-crate");
    assert_eq!(
        schemeless.overall_data.repository.as_ref().map(url::Url::as_str),
        Some("https://github.com/fake-org/schemeless-repo-crate"),
        "a scheme-less repository URL gains an https:// prefix"
    );

    let broken = found(&results, "broken-repo-crate");
    assert_eq!(
        broken.overall_data.repository, None,
        "an unparseable repository URL is dropped rather than failing the import"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn reuses_cached_tables() {
    let now = Utc::now();
    // The mock asserts on drop that the dump was downloaded exactly once.
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    {
        let provider = make_provider(cache.path(), &server, now).await;
        assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
    }

    let provider = make_provider(cache.path(), &server, now + Duration::days(1)).await;
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn redownloads_when_cached_tables_are_stale() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(2)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    drop(make_provider(cache.path(), &server, now).await);

    // Far enough in the future that the cached tables exceed their TTL.
    let provider = make_provider(cache.path(), &server, now + Duration::days(400)).await;
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn redownloads_when_cache_is_ignored() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(2)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    drop(make_provider(cache.path(), &server, now).await);

    let provider = Provider::new(cache.path(), LONG_TTL, progress(), now, true, Some(&dump_url(&server)))
        .await
        .expect("the synthetic dump must be accepted by the provider");
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn redownloads_when_a_cached_table_is_corrupt() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(2)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    drop(make_provider(cache.path(), &server, now).await);

    // Clobber the header so its magic number no longer matches.
    let table = cache.path().join("crates.table");
    std::fs::write(&table, [0xAB_u8; 64]).expect("overwriting a cached table");

    let provider = make_provider(cache.path(), &server, now).await;
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn rejects_a_dump_that_is_not_served() {
    let server = failing_server(404).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let error = Provider::new(
        cache.path(),
        LONG_TTL,
        progress(),
        Utc::now(),
        false,
        Some(&format!("{}/db-dump.tar.gz", server.uri())),
    )
    .await
    .expect_err("a 404 must not produce a usable provider");

    let message = format!("{error:#}");
    assert!(message.contains("404"), "error should mention the status code, got: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn recovers_from_a_transient_server_error() {
    let now = Utc::now();
    let server = MockServer::start().await;

    // The first attempt fails with a 5xx; the retry layer then gets the real dump.
    Mock::given(method("GET"))
        .and(path(support::DUMP_PATH))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(support::DUMP_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(Dump::sample(now).to_tar_gz()))
        .expect(1)
        .mount(&server)
        .await;

    let cache = tempfile::tempdir().expect("creating a temp dir");
    let provider = make_provider(cache.path(), &server, now).await;

    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn rejects_a_truncated_dump() {
    let now = Utc::now();
    let body = support::dump::truncate(&Dump::sample(now).to_tar_gz());
    let server = dump_server(body, None).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let _error = Provider::new(cache.path(), LONG_TTL, progress(), now, false, Some(&dump_url(&server)))
        .await
        .expect_err("a truncated gzip stream must not produce a usable provider");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn rejects_a_dump_with_a_malformed_column() {
    let now = Utc::now();
    let dump = Dump::sample(now);
    let mut files = dump.csv_files();

    for (name, contents) in &mut files {
        if name == "crates.csv" {
            // `id` must parse as an integer.
            *contents = contents.replace("\n1,serde,", "\nnot-a-number,serde,");
        }
    }

    let server = dump_server(support::dump::tar_gz(&files), None).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let error = Provider::new(cache.path(), LONG_TTL, progress(), now, false, Some(&dump_url(&server)))
        .await
        .expect_err("a malformed column must not produce a usable provider");

    let message = format!("{error:#}");
    assert!(
        message.contains("not-a-number"),
        "error should quote the offending value, got: {message}"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn rejects_a_dump_with_a_missing_column() {
    let now = Utc::now();
    let dump = Dump::sample(now);
    let mut files = dump.csv_files();

    for (name, contents) in &mut files {
        if name == "versions.csv" {
            // Drop the header the `versions` table deserializes `num` from.
            *contents = contents.replacen("num,", "version_number,", 1);
        }
    }

    let server = dump_server(support::dump::tar_gz(&files), None).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let _error = Provider::new(cache.path(), LONG_TTL, progress(), now, false, Some(&dump_url(&server)))
        .await
        .expect_err("a missing column must not produce a usable provider");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn handles_an_empty_request_list() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let mut results = provider.get_crates_data(&[], &NoOpProgress, true).await;

    assert!(results.next().is_none());
}

// ---------------------------------------------------------------------------
// Dumps with awkward content
// ---------------------------------------------------------------------------

/// The CSV files of the sample dump, with `extra` crates appended.
fn files_with_extra_crates(now: DateTime<Utc>, extra: Vec<support::dump::Crate>) -> Vec<(String, String)> {
    let mut dump = Dump::sample(now);
    dump.crates.extend(extra);
    dump.csv_files()
}

/// Appends `rows` to the named CSV of `files`.
fn append_rows(files: &mut [(String, String)], name: &str, rows: &str) {
    let entry = files
        .iter_mut()
        .find(|(file_name, _)| file_name == name)
        .unwrap_or_else(|| panic!("the sample dump always contains {name}"));
    entry.1.push_str(rows);
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn reports_every_edition_and_drops_unparseable_version_urls() {
    let now = Utc::now();

    // One crate per edition the table knows about, plus one with an edition it does not, and one
    // whose documentation and homepage URLs cannot be parsed at all.
    let editions = vec![
        support::dump::Crate {
            versions: vec![support::dump::Version {
                edition: Some(2015),
                ..support::dump::Version::new(10_001, "1.0.0", now, 5)
            }],
            ..support::dump::Crate::new(101, "edition-2015-crate", now, 100)
        },
        support::dump::Crate {
            versions: vec![support::dump::Version {
                edition: Some(2024),
                ..support::dump::Version::new(10_002, "1.0.0", now, 5)
            }],
            ..support::dump::Crate::new(102, "edition-2024-crate", now, 100)
        },
        support::dump::Crate {
            versions: vec![support::dump::Version {
                edition: Some(1999),
                ..support::dump::Version::new(10_003, "1.0.0", now, 5)
            }],
            ..support::dump::Crate::new(103, "edition-unknown-crate", now, 100)
        },
        support::dump::Crate {
            versions: vec![support::dump::Version {
                documentation: "not a valid doc url".to_owned(),
                homepage: "not a valid home url".to_owned(),
                ..support::dump::Version::new(10_004, "1.0.0", now, 5)
            }],
            ..support::dump::Crate::new(104, "bad-version-urls-crate", now, 100)
        },
    ];

    let files = files_with_extra_crates(now, editions);
    let server = dump_server(support::dump::tar_gz(&files), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(
        &provider,
        &[
            crate_ref("edition-2015-crate", None),
            crate_ref("edition-2024-crate", None),
            crate_ref("edition-unknown-crate", None),
            crate_ref("bad-version-urls-crate", None),
        ],
    )
    .await;

    assert_eq!(
        format!("{:?}", found(&results, "edition-2015-crate").version_data.edition),
        "Some(Edition2015)"
    );
    assert_eq!(
        format!("{:?}", found(&results, "edition-2024-crate").version_data.edition),
        "Some(Edition2024)"
    );
    assert_eq!(
        format!("{:?}", found(&results, "edition-unknown-crate").version_data.edition),
        "Some(Unknown)"
    );

    let bad_urls = found(&results, "bad-version-urls-crate");
    assert_eq!(
        bad_urls.version_data.documentation, None,
        "an unparseable documentation URL is dropped"
    );
    assert_eq!(bad_urls.version_data.homepage, None, "an unparseable homepage URL is dropped");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn resolves_the_latest_version_when_versions_ascend() {
    let now = Utc::now();

    // The sample dump lists `serde`'s newest version first, so the "this candidate beats the one
    // we already have" branch never runs for it. This crate lists its versions the other way up.
    let ascending = support::dump::Crate {
        versions: vec![
            support::dump::Version::new(11_001, "0.1.0", now, 90),
            support::dump::Version::new(11_002, "0.9.0", now, 30),
        ],
        ..support::dump::Crate::new(110, "ascending-crate", now, 200)
    };

    let files = files_with_extra_crates(now, vec![ascending]);
    let server = dump_server(support::dump::tar_gz(&files), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let resolved: Vec<_> = provider
        .get_crates_data(&[crate_ref("ascending-crate", None)], &NoOpProgress, false)
        .await
        .collect();

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0.version().to_string(), "0.9.0");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn keeps_only_the_closest_suggestions() {
    let now = Utc::now();

    // Three candidates at the same distance fill every suggestion slot, then a closer one has to
    // evict the weakest of them.
    let candidates = [
        "suggestionbaseline11",
        "suggestionbaseline22",
        "suggestionbaseline33",
        "suggestionbaseline01",
    ];
    let extra: Vec<_> = candidates
        .iter()
        .enumerate()
        .map(|(offset, name)| {
            let id = 120 + offset as u64;
            support::dump::Crate {
                versions: vec![support::dump::Version::new(12_000 + id, "1.0.0", now, 10)],
                ..support::dump::Crate::new(id, name, now, 100)
            }
        })
        .collect();

    let files = files_with_extra_crates(now, extra);
    let server = dump_server(support::dump::tar_gz(&files), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results = query(&provider, &[crate_ref("suggestionbaseline00", Some("1.0.0"))]).await;

    match &results["suggestionbaseline00"] {
        ProviderResult::CrateNotFound(suggestions) => {
            assert_eq!(suggestions.len(), 3, "at most three suggestions are kept, got {suggestions:?}");
            assert_eq!(
                suggestions[0], "suggestionbaseline01",
                "the closest name is reported first, got {suggestions:?}"
            );
        }
        other => panic!("expected CrateNotFound, got {other:?}"),
    }
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn tolerates_dangling_references_in_the_dump() {
    let now = Utc::now();

    // The real dump is a consistent snapshot, but nothing in the reader depends on that: rows
    // pointing at owners, categories, keywords and versions that do not exist are dropped rather
    // than failing the query.
    let mut files = Dump::sample(now).csv_files();
    append_rows(
        &mut files,
        "crate_owners.csv",
        "1,999,2016-06-01 10:00:00.000000+00,1,0\n1,998,2016-06-01 10:00:00.000000+00,1,1\n",
    );
    append_rows(&mut files, "crates_categories.csv", "1,777\n");
    append_rows(&mut files, "crates_keywords.csv", "1,888\n");
    append_rows(&mut files, "dependencies.csv", "9999,999999,1,^1,f,t,{},,0,\n");

    let server = dump_server(support::dump::tar_gz(&files), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let serde = found(&query(&provider, &[crate_ref("serde", Some("1.0.200"))]).await, "serde");

    let logins: Vec<_> = serde.overall_data.owners.iter().map(|owner| owner.login.as_str()).collect();
    assert_eq!(logins, ["alice", "github:fake-org:maintainers"], "unknown owner ids are dropped");
    assert_eq!(serde.overall_data.categories, ["encoding", "parser-implementations"]);
    assert_eq!(serde.overall_data.keywords, ["serialization", "no-std"]);
    assert_eq!(
        serde.overall_data.dependents, 1,
        "a dependency from an unknown version is not counted"
    );
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn keeps_a_repository_url_that_is_not_a_known_repository() {
    let now = Utc::now();

    // A well-formed URL that has no owner/repo path, so `RepoSpec` cannot be derived from it.
    let odd_repo = support::dump::Crate {
        repository: "https://example.com/onlyonesegment".to_owned(),
        versions: vec![support::dump::Version::new(13_001, "1.0.0", now, 10)],
        ..support::dump::Crate::new(130, "odd-repo-crate", now, 100)
    };

    let files = files_with_extra_crates(now, vec![odd_repo]);
    let server = dump_server(support::dump::tar_gz(&files), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    let results: Vec<_> = provider
        .get_crates_data(&[crate_ref("odd-repo-crate", Some("1.0.0"))], &NoOpProgress, false)
        .await
        .collect();

    assert_eq!(results.len(), 1);
    let (_, result) = &results[0];
    let data = match result {
        ProviderResult::Found(data) => data.clone(),
        other => panic!("expected Found, got {other:?}"),
    };
    assert_eq!(
        data.overall_data.repository.as_ref().map(url::Url::as_str),
        Some("https://example.com/onlyonesegment"),
        "the raw repository URL is still reported"
    );
}

// ---------------------------------------------------------------------------
// Cache and table file handling
// ---------------------------------------------------------------------------

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn redownloads_when_a_cached_table_is_too_short() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(2)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    drop(make_provider(cache.path(), &server, now).await);

    // Shorter than the 24-byte table header, so validation cannot even read the magic number.
    std::fs::write(cache.path().join("crates.table"), [0xAB_u8; 8]).expect("truncating a cached table");

    let provider = make_provider(cache.path(), &server, now).await;
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn fails_when_a_table_file_cannot_be_replaced() {
    // Covers the diagnostics that only evaluate their arguments when debug logging is on.
    log::set_max_level(log::LevelFilter::Debug);

    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), None).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    // A directory sitting where a table file belongs: it cannot be removed with `remove_file`,
    // and the download cannot create the table over it either.
    std::fs::create_dir(cache.path().join("crates.table")).expect("creating the blocking directory");

    let error = Provider::new(cache.path(), LONG_TTL, progress(), now, false, Some(&dump_url(&server)))
        .await
        .expect_err("a table file that cannot be written must not produce a usable provider");

    let message = format!("{error:#}");
    assert!(message.contains("crates"), "error should mention the table, got: {message}");
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn logs_diagnostics_while_downloading_and_reusing_tables() {
    // The download and cache-hit paths carry `log::` statements whose arguments are only
    // evaluated once logging is turned up.
    log::set_max_level(log::LevelFilter::Debug);

    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    {
        let provider = make_provider(cache.path(), &server, now).await;
        assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
    }

    // The second provider takes the "opened the cached tables" path instead.
    let provider = make_provider(cache.path(), &server, now).await;
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

// ---------------------------------------------------------------------------
// Progress reporting
// ---------------------------------------------------------------------------

/// A progress reporter that immediately invokes every callback handed to it.
#[derive(Debug)]
struct EagerProgress;

impl Progress for EagerProgress {
    fn set_phase(&self, _phase: &str) {}

    fn set_determinate(&self, callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {
        let (total, done, message) = callback();
        assert!(done <= total || message.is_empty() || total == 0 || done > 0);
    }

    fn set_indeterminate(&self, callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {
        assert!(!callback().is_empty());
    }

    fn println(&self, _msg: &str) {}
    fn done(&self) {}
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn renders_the_progress_callbacks() {
    let now = Utc::now();
    let server = dump_server(Dump::sample(now).to_tar_gz(), Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let eager: Arc<dyn Progress> = Arc::new(EagerProgress);

    {
        let provider = Provider::new(cache.path(), LONG_TTL, Arc::clone(&eager), now, false, Some(&dump_url(&server)))
            .await
            .expect("the synthetic dump must be accepted by the provider");
        assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
    }

    // Opening the cached tables installs its own progress callback.
    let provider = Provider::new(cache.path(), LONG_TTL, eager, now, false, Some(&dump_url(&server)))
        .await
        .expect("the cached tables must be accepted by the provider");
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Reads and discards an HTTP request from `stream`.
fn read_request(stream: &mut std::net::TcpStream) {
    use std::io::Read as _;

    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => request.push(byte[0]),
        }
    }
}

/// Writes `body` to `stream` using `chunked` transfer encoding, which leaves the response without a
/// `Content-Length`. When `truncated`, only half the body is sent and the terminating chunk is
/// omitted, so the client sees the connection die mid-body.
fn write_chunked(stream: &mut std::net::TcpStream, body: &[u8], truncated: bool) {
    use std::io::Write as _;

    let mut response =
        b"HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();

    let keep = if truncated { body.len() / 2 } else { body.len() };
    for chunk in body[..keep].chunks(4096) {
        response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        response.extend_from_slice(chunk);
        response.extend_from_slice(b"\r\n");
    }

    if !truncated {
        response.extend_from_slice(b"0\r\n\r\n");
    }

    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

/// Serves `body` without a `Content-Length`, dying mid-body on the first request and answering
/// every later request in full. Returns the URL the dump is served from.
fn chunked_dump_server(body: Vec<u8>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binding a loopback port");
    let address = listener.local_addr().expect("a bound listener has an address");

    let _ = std::thread::spawn(move || {
        let mut served = 0_u32;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            read_request(&mut stream);
            served += 1;
            write_chunked(&mut stream, &body, served == 1);
        }
    });

    format!("http://{address}{}", support::DUMP_PATH)
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn retries_a_response_without_a_content_length_that_dies_mid_body() {
    let now = Utc::now();
    let url = chunked_dump_server(Dump::sample(now).to_tar_gz());
    let cache = tempfile::tempdir().expect("creating a temp dir");

    // A chunked response has no `Content-Length`, so the download reports indeterminate progress.
    let provider = Provider::new(cache.path(), LONG_TTL, Arc::new(EagerProgress), now, false, Some(&url))
        .await
        .expect("the retry after the aborted response must succeed");

    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn stops_streaming_once_the_archive_has_been_read() {
    let now = Utc::now();

    // Padding past the end of the archive: the reader stops at the end-of-archive marker while
    // the download is still pumping bytes, so the sender finds nobody left to hand them to.
    let mut body = Dump::sample(now).to_tar_gz();
    body.extend(core::iter::repeat_n(0_u8, 16 * 1024 * 1024));

    let server = dump_server(body, Some(1)).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let provider = make_provider(cache.path(), &server, now).await;
    assert!(query(&provider, &[crate_ref("itoa", None)]).await["itoa"].is_found());
}

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn rejects_a_dump_with_an_unknown_owner_kind() {
    let now = Utc::now();

    // crates.io only has user (0) and team (1) owners; anything else means the table layout
    // changed under us, which must be reported instead of silently mis-decoded.
    let mut files = Dump::sample(now).csv_files();
    append_rows(&mut files, "crate_owners.csv", "1,3,2016-06-01 10:00:00.000000+00,1,7\n");

    let server = dump_server(support::dump::tar_gz(&files), None).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");

    let error = Provider::new(cache.path(), LONG_TTL, progress(), now, false, Some(&dump_url(&server)))
        .await
        .expect_err("an unknown owner kind must not produce a usable provider");

    let message = format!("{error:#}");
    assert!(message.contains("owner_kind"), "error should name the column, got: {message}");
}
