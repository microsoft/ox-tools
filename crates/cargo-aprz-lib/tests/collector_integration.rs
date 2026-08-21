// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for the fact [`Collector`] itself.
//!
//! The collector owns every provider, so building one requires the whole mocked world: a synthetic
//! crates.io dump served over HTTP, a seeded advisory database and 404-answering stand-ins for the
//! remaining services.

#![cfg(not(miri))]

mod support;

use core::time::Duration;
use std::sync::Arc;

use cargo_aprz_lib::internals::facts::{BugLabelMatcher, Collector, Endpoints};
use support::dump::Dump;
use support::{NoOpProgress, dump_server, dump_url, failing_server, seed_advisory_db};

const TTL: Duration = Duration::from_hours(1);

#[tokio::test]
#[cfg_attr(miri, ignore = "Miri cannot memory-map files or run a mock HTTP server")]
async fn collector_debug_lists_every_provider() {
    let dump = dump_server(Dump::sample(chrono::Utc::now()).to_tar_gz(), None).await;
    let services = failing_server(404).await;
    let cache = tempfile::tempdir().expect("creating a temp dir");
    seed_advisory_db(cache.path());

    let service_uri = services.uri();
    let endpoints = Endpoints::default()
        .with_dump_url(dump_url(&dump))
        .with_docs_url(&service_uri)
        .with_coverage_url(&service_uri)
        .with_github_url(&service_uri)
        .with_codeberg_url(service_uri);

    let collector = Collector::new(
        None,
        None,
        cache.path(),
        TTL,
        TTL,
        TTL,
        TTL,
        TTL,
        false,
        Arc::new(BugLabelMatcher::new(&[]).expect("an empty pattern list always compiles")),
        NoOpProgress,
        &endpoints,
    )
    .await
    .expect("the mocked world must let the collector start up");

    let debug = format!("{collector:?}");

    assert!(debug.starts_with("Collector {"), "unexpected Debug output: {debug}");
    for field in [
        "crates_provider",
        "hosting_provider",
        "advisories_provider",
        "codebase_provider",
        "coverage_provider",
        "docs_provider",
        "progress",
    ] {
        assert!(debug.contains(field), "Debug output should mention {field}: {debug}");
    }
    assert!(
        debug.contains("<dyn Progress>"),
        "the progress reporter is rendered opaquely: {debug}"
    );
    assert!(debug.ends_with('}'), "the struct must be closed: {debug}");
}
