// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared helpers for the integration tests.
//!
//! The helpers here let a test run the real code paths — including the whole CLI — without
//! touching the network: [`dump`] builds a synthetic crates.io database dump, which is served
//! from a [`wiremock`] server, and [`seed_advisory_db`] pre-populates the advisory cache so the
//! advisory provider never clones its git repository.

#![allow(
    dead_code,
    reason = "each integration test binary is its own crate and uses a different subset of these helpers"
)]

pub mod dump;

use core::time::Duration;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use cargo_aprz_lib::Host;
use cargo_aprz_lib::internals::facts::Progress;
use cargo_aprz_lib::internals::facts::cache::Cache;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Path the mock server serves the database dump from.
pub const DUMP_PATH: &str = "/db-dump.tar.gz";

/// Progress reporter that discards everything.
#[derive(Debug)]
pub struct NoOpProgress;

impl Progress for NoOpProgress {
    fn set_phase(&self, _phase: &str) {}
    fn set_determinate(&self, _callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {}
    fn set_indeterminate(&self, _callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {}
    fn println(&self, _msg: &str) {}
    fn done(&self) {}
}

/// Test host that captures output to in-memory buffers.
#[derive(Debug)]
pub struct TestHost {
    output_buf: Vec<u8>,
    error_buf: Vec<u8>,
    pub exit_code: Option<i32>,
}

impl TestHost {
    pub const fn new() -> Self {
        Self {
            output_buf: Vec::new(),
            error_buf: Vec::new(),
            exit_code: None,
        }
    }

    pub fn output_str(&self) -> String {
        String::from_utf8_lossy(&self.output_buf).into_owned()
    }

    pub fn error_str(&self) -> String {
        String::from_utf8_lossy(&self.error_buf).into_owned()
    }
}

impl Host for TestHost {
    fn output(&mut self) -> impl std::io::Write {
        Cursor::new(&mut self.output_buf)
    }

    fn error(&mut self) -> impl std::io::Write {
        Cursor::new(&mut self.error_buf)
    }

    fn exit(&mut self, code: i32) {
        self.exit_code = Some(code);
    }
}

/// Starts a mock server that serves `body` from [`DUMP_PATH`].
///
/// `expected_requests`, when given, asserts on drop how many times the dump was fetched, which is
/// how the tests tell a cache hit from a fresh download.
pub async fn dump_server(body: Vec<u8>, expected_requests: Option<u64>) -> MockServer {
    let server = MockServer::start().await;

    let mock = Mock::given(method("GET")).and(path(DUMP_PATH)).respond_with(
        ResponseTemplate::new(200)
            .set_body_bytes(body)
            .insert_header("content-type", "application/gzip"),
    );

    if let Some(count) = expected_requests {
        mock.expect(count).mount(&server).await;
    } else {
        mock.mount(&server).await;
    }

    server
}

/// Starts a mock server that answers every request with `status`.
pub async fn failing_server(status: u16) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;

    server
}

/// Address of the dump on `server`.
pub fn dump_url(server: &MockServer) -> String {
    format!("{}{DUMP_PATH}", server.uri())
}

/// A fully mocked outside world for end-to-end command tests.
///
/// Holds a mock server serving the synthetic database dump, a second mock server that answers
/// every other service (docs.rs, Codecov, GitHub, Codeberg) with a 404 so those providers record
/// the data as unavailable, a seeded advisory database, and a private cache directory.
#[derive(Debug)]
pub struct MockWorld {
    cache: tempfile::TempDir,
    _dump_server: MockServer,
    _service_server: MockServer,
    args: Vec<String>,
}

impl MockWorld {
    /// Builds a world serving the standard synthetic dump.
    pub async fn new() -> Self {
        Self::with_dump(dump::Dump::sample(chrono::Utc::now()).to_tar_gz()).await
    }

    /// Builds a world serving the given dump bytes.
    pub async fn with_dump(body: Vec<u8>) -> Self {
        let dump_server = dump_server(body, None).await;
        let service_server = failing_server(404).await;

        let cache = tempfile::tempdir().expect("creating a temp dir");
        seed_advisory_db(cache.path());

        let service_uri = service_server.uri();
        let args = vec![
            "--cache-dir".to_owned(),
            cache.path().to_str().expect("temp dir paths are UTF-8").to_owned(),
            "--dump-url".to_owned(),
            dump_url(&dump_server),
            "--docs-url".to_owned(),
            service_uri.clone(),
            "--coverage-url".to_owned(),
            service_uri.clone(),
            "--github-url".to_owned(),
            service_uri.clone(),
            "--codeberg-url".to_owned(),
            service_uri,
            "--color".to_owned(),
            "never".to_owned(),
        ];

        Self {
            cache,
            _dump_server: dump_server,
            _service_server: service_server,
            args,
        }
    }

    /// The command-line arguments that redirect every service at this world.
    pub fn args(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// The cache directory this world hands to the tool.
    pub fn cache_dir(&self) -> &Path {
        self.cache.path()
    }
}

/// Runs the `cargo aprz` CLI against a mocked world, appending `extra` to the world's arguments.
pub async fn run_cli(world: &MockWorld, extra: &[&str]) -> TestHost {
    let mut args: Vec<&str> = vec!["cargo", "aprz"];
    args.extend_from_slice(extra);
    args.extend(world.args());

    let mut host = TestHost::new();
    cargo_aprz_lib::run(&mut host, args).await;
    host
}

/// Pre-populates the advisory cache under `cache_dir` with a tiny advisory database.
///
/// The advisory provider clones a git repository on a cache miss, which no test can afford to do.
/// Writing the "last synced" marker and a database directory in the layout `rustsec` expects makes
/// the provider skip the clone and read the synthetic advisories instead.
pub fn seed_advisory_db(cache_dir: &Path) {
    let advisories_dir = cache_dir.join("advisories");
    let repo_dir = advisories_dir.join("repo");
    let crate_dir = repo_dir.join("crates").join("adler2");
    fs::create_dir_all(&crate_dir).expect("creating the advisory database directory");

    fs::write(
        crate_dir.join("RUSTSEC-2020-0001.md"),
        "```toml\n\
         [advisory]\n\
         id = \"RUSTSEC-2020-0001\"\n\
         package = \"adler2\"\n\
         date = \"2020-01-01\"\n\
         informational = \"unmaintained\"\n\
         \n\
         [versions]\n\
         patched = []\n\
         ```\n\
         \n\
         # adler2 is unmaintained\n\
         \n\
         Synthetic advisory used by the integration tests.\n",
    )
    .expect("writing the synthetic advisory");

    let cache = Cache::new(&advisories_dir, Duration::MAX, false);
    cache.save("last_synced.bin", &()).expect("writing the advisory sync marker");
}
