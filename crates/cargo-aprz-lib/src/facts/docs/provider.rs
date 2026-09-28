// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::sync::atomic::{AtomicU64, Ordering};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::Stream;
use futures::stream::StreamExt;
use futures_util::future::join_all;
use ohno::{EnrichableExt, IntoAppError, app_err};
use tokio::io::AsyncWriteExt;

use super::DocsData;
use crate::Result;
use crate::facts::ProviderResult;
use crate::facts::cache::{Cache, CacheResult};
use crate::facts::crate_spec::CrateSpec;
use crate::facts::path_utils::sanitize_path_component;
use crate::facts::request_tracker::{RequestTracker, TrackedTopic};
use crate::facts::throttler::Throttler;

pub(super) const LOG_TARGET: &str = "      docs";

/// Default base URL for docs.rs
pub const DOCS_BASE_URL: &str = "https://docs.rs";

const MAX_CONCURRENT_REQUESTS: usize = 5;
const DOWNLOAD_ID_STEP: u64 = 1;
const DOWNLOAD_OPERATION: &str = "docs_download";
const UNREADABLE_BODY_PLACEHOLDER: &str = "<unable to read body>";

#[derive(Debug, Clone)]
pub struct Provider {
    client: Arc<reqwest::Client>,
    cache: Cache,
    base_url: String,
    throttler: Arc<Throttler>,
}

impl Provider {
    /// Create a new docs provider
    #[must_use]
    pub fn new(cache: Cache, base_url: Option<&str>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("cargo-aprz")
            .build()
            .expect("unable to create HTTP client");

        Self {
            client: Arc::new(client),
            cache,
            base_url: base_url.unwrap_or(DOCS_BASE_URL).to_string(),
            throttler: Throttler::new(MAX_CONCURRENT_REQUESTS),
        }
    }

    /// Get documentation data for multiple crates
    pub async fn get_docs_data(
        &self,
        crates: Arc<[CrateSpec]>,
        tracker: &RequestTracker,
    ) -> impl Iterator<Item = (CrateSpec, ProviderResult<DocsData>)> {
        join_all(crates.iter().cloned().map(|crate_spec| {
            tracker.add_requests(TrackedTopic::Docs, 1);

            let provider = self.clone();
            let tracker = tracker.clone();

            tokio::spawn(provider.fetch_docs_for_crate(crate_spec, tracker))
        }))
        .await
        .into_iter()
        .map(|task_result| task_result.expect("tasks must not panic"))
        .inspect(|(crate_spec, result)| {
            if let ProviderResult::Error(e) = result {
                log::error!(target: LOG_TARGET, "Could not fetch documentation data for {crate_spec}: {e:#}");
            } else if let ProviderResult::Unavailable(reason) = result {
                log::warn!(target: LOG_TARGET, "Documentation unavailable for {crate_spec}: {reason}");
            }
        })
    }

    async fn fetch_docs_for_crate(self, crate_spec: CrateSpec, tracker: RequestTracker) -> (CrateSpec, ProviderResult<DocsData>) {
        let _completion = RequestCompletion::new(tracker, TrackedTopic::Docs);
        let _permit = self.throttler.acquire().await;
        let result = self.fetch_docs_for_crate_core(&crate_spec).await;

        (crate_spec, result)
    }

    async fn fetch_docs_for_crate_core(&self, crate_spec: &CrateSpec) -> ProviderResult<DocsData> {
        let filename = Self::get_cache_filename(crate_spec);

        match self.cache.load::<DocsData>(&filename) {
            CacheResult::Data(data) => return ProviderResult::Found(data),
            CacheResult::NoData(reason) => return ProviderResult::Unavailable(reason.into()),
            CacheResult::Miss => {}
        }

        log::info!(target: LOG_TARGET, "Querying {} for documentation on {crate_spec}", self.base_url);

        let provider = self.clone();
        let spec = crate_spec.clone();

        // resilient_download retries on Err, passes through Ok(None) for 404.
        let result = crate::facts::resilient_http::resilient_download(DOWNLOAD_OPERATION, spec, None, move |spec| {
            let provider = provider.clone();
            async move { provider.download_zst_core(&spec).await }
        })
        .await;

        let temp_file = match result {
            Ok(Some(path)) => path,
            Ok(None) => {
                let reason = format!("could not find documentation for {crate_spec} on {}", self.base_url);
                if let Err(e) = self.cache.save_no_data(&filename, &reason) {
                    log::debug!(target: LOG_TARGET, "Could not save cache for {crate_spec}: {e:#}");
                }
                return ProviderResult::Unavailable(reason.into());
            }
            Err(e) => {
                return ProviderResult::Error(Arc::new(e.enrich_with(|| format!("downloading docs for {crate_spec}"))));
            }
        };

        let docs_data = match Self::calculate_docs_metrics_and_remove(&temp_file, crate_spec).await {
            Ok(data) => {
                let m = &data.metrics;
                log::debug!(target: LOG_TARGET, "Successfully calculated docs metrics for {crate_spec}");
                log::debug!(target: LOG_TARGET, "Metrics: coverage={}%, public_api={}, documented={}, examples={}, crate_docs={}",
                    m.doc_coverage_percentage,
                    m.public_api_elements,
                    m.public_api_elements - m.undocumented_elements,
                    m.examples_in_docs,
                    m.has_crate_level_docs);
                data
            }
            Err(e) => {
                let reason = format!("{e:#}");
                if let Err(e) = self.cache.save_no_data(&filename, &reason) {
                    log::debug!(target: LOG_TARGET, "Could not save cache for {crate_spec}: {e:#}");
                }

                return ProviderResult::Unavailable(reason.into());
            }
        };

        match self.cache.save(&filename, &docs_data) {
            Ok(()) => ProviderResult::Found(docs_data),
            Err(e) => ProviderResult::Error(Arc::new(e)),
        }
    }

    /// Get the cache filename for a specific crate and version
    fn get_cache_filename(crate_spec: &CrateSpec) -> String {
        let safe_name = sanitize_path_component(crate_spec.name());
        let safe_version = sanitize_path_component(&crate_spec.version().to_string());
        format!("{safe_name}@{safe_version}.bin")
    }

    /// Download logic for a single attempt.
    /// Returns `Ok(None)` for 404 (not retryable), `Ok(Some(path))` on success.
    async fn download_zst_core(&self, crate_spec: &CrateSpec) -> Result<Option<PathBuf>> {
        self.download_zst_to(crate_spec, temp_zst_path(crate_spec.name(), &crate_spec.version().to_string()))
            .await
    }

    async fn download_zst_to(&self, crate_spec: &CrateSpec, temp_file: PathBuf) -> Result<Option<PathBuf>> {
        let crate_name = crate_spec.name();
        let version = crate_spec.version().to_string();

        let url = format!("{}/crate/{crate_name}/{version}/json", self.base_url);

        let response = crate::facts::resilient_http::resilient_get(&self.client, &url).await?;

        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let body = match response.text().await {
                Ok(body) => body,
                Err(_) => UNREADABLE_BODY_PLACEHOLDER.to_owned(),
            };
            log::debug!(target: LOG_TARGET, "Response body (first 500 chars): {}", body.chars().take(500).collect::<String>());
            return Err(app_err!("could not download docs for {crate_spec}: HTTP {status}"));
        }

        let mut file = tokio::fs::File::create(&temp_file)
            .await
            .into_app_err_with(|| format!("creating temp file '{}'", temp_file.display()))?;

        // #[gamma::skip(try.propagate_to_unwrap, reason = "streaming an HTTP response into a file is an external I/O adapter boundary")]
        let total_bytes = write_response_body(response.bytes_stream(), &mut file, &temp_file).await?;

        log::debug!(target: LOG_TARGET, "Downloaded {total_bytes} bytes for {crate_spec} to temp file '{}'", temp_file.display());
        Ok(Some(temp_file))
    }

    fn calculate_docs_metrics(zst_path: impl AsRef<Path>, crate_spec: &CrateSpec) -> Result<DocsData> {
        let path = zst_path.as_ref();
        log::debug!(target: LOG_TARGET, "Opening .zst file for {crate_spec}: {}", path.display());
        // #[gamma::skip(try.propagate_to_unwrap, reason = "opening downloaded documentation is a filesystem adapter boundary whose errors must remain recoverable")]
        let file = fs::File::open(path).into_app_err_with(|| format!("opening file '{}' for {crate_spec}", path.display()))?;
        let reader = std::io::BufReader::new(file);

        let json_bytes = zstd::stream::decode_all(reader).into_app_err_with(|| format!("decompressing docs for {crate_spec}"))?;

        super::calc_metrics::calculate_docs_metrics(&json_bytes, crate_spec)
    }

    async fn calculate_docs_metrics_and_remove(zst_path: impl AsRef<Path>, crate_spec: &CrateSpec) -> Result<DocsData> {
        let path = zst_path.as_ref().to_path_buf();
        let worker_path = path.clone();
        let worker_spec = crate_spec.clone();
        let result = tokio::task::spawn_blocking(move || Self::calculate_docs_metrics(worker_path, &worker_spec))
            .await
            .map_err(|cause| app_err!("documentation metric worker failed for {crate_spec}: {cause}"))?;
        tokio::fs::remove_file(&path)
            .await
            .unwrap_or_else(|e| log::debug!(target: LOG_TARGET, "Could not remove temp file '{}': {e:#}", path.display()));
        result
    }
}

struct RequestCompletion {
    tracker: RequestTracker,
    topic: TrackedTopic,
}

impl RequestCompletion {
    fn new(tracker: RequestTracker, topic: TrackedTopic) -> Self {
        Self { tracker, topic }
    }
}

impl Drop for RequestCompletion {
    fn drop(&mut self) {
        self.tracker.complete_request(self.topic);
    }
}

async fn write_response_body<S, E, W>(stream: S, writer: &mut W, temp_file: &Path) -> Result<usize>
where
    S: Stream<Item = core::result::Result<bytes::Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::pin!(stream);
    let mut total_bytes = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.into_app_err_with(|| "reading response chunk".to_owned())?;
        total_bytes += chunk.len();
        writer
            .write_all(&chunk)
            .await
            .into_app_err_with(|| format!("writing to temp file '{}'", temp_file.display()))?;
    }
    writer
        .flush()
        .await
        .into_app_err_with(|| format!("flushing temp file '{}'", temp_file.display()))?;
    Ok(total_bytes)
}

/// Builds a unique path for a crate's downloaded `.zst` file.
///
/// Naming the file after the crate alone is not enough. The same crate can be downloaded more
/// than once concurrently: `resilient_download` retries a failed attempt, and separate
/// `cargo aprz` processes share the system temp directory. Because every download both truncates
/// the file on create and deletes it once decoded, a shared path lets one download destroy
/// another's data, which surfaces as a spurious "data corruption" failure while decompressing.
/// The process id and a per-process counter keep each download on its own path.
fn temp_zst_path(crate_name: &str, version: &str) -> PathBuf {
    static NEXT_DOWNLOAD_ID: AtomicU64 = AtomicU64::new(0);

    let safe_name = sanitize_path_component(crate_name);
    let safe_version = sanitize_path_component(version);
    let pid = std::process::id();
    let id = next_download_id(&NEXT_DOWNLOAD_ID);

    std::env::temp_dir().join(format!("{safe_name}@{safe_version}-{pid}-{id}.zst"))
}

fn next_download_id(counter: &AtomicU64) -> u64 {
    counter.fetch_add(DOWNLOAD_ID_STEP, Ordering::Relaxed)
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use std::io;
    use std::process::Command;
    use std::sync::Mutex;

    use semver::Version;

    use super::*;
    use crate::facts::Progress;
    use crate::facts::docs::DocsMetrics;
    use crate::facts::request_tracker::TopicStatus;

    #[derive(Debug)]
    struct NoOpProgress;

    impl Progress for NoOpProgress {
        fn set_phase(&self, _phase: &str) {}
        fn set_determinate(&self, _callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {}
        fn set_indeterminate(&self, _callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {}
        fn println(&self, _msg: &str) {}
        fn done(&self) {}
    }

    fn test_crate_spec(name: &str, version: &str) -> CrateSpec {
        CrateSpec::from_arcs(Arc::from(name), Arc::new(Version::parse(version).unwrap()))
    }

    #[tokio::test]
    async fn get_docs_data_tracks_each_crate_exactly_once() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(temp.path(), core::time::Duration::MAX, false);
        let spec = test_crate_spec("cached", "1.0.0");
        cache
            .save(
                &Provider::get_cache_filename(&spec),
                &DocsData {
                    metrics: DocsMetrics {
                        doc_coverage_percentage: 100.0,
                        public_api_elements: 1,
                        undocumented_elements: 0,
                        examples_in_docs: 0,
                        has_crate_level_docs: true,
                        broken_doc_links: 0,
                    },
                },
            )
            .unwrap();
        let provider = Provider::new(cache, None);
        let tracker = RequestTracker::new(&(Arc::new(NoOpProgress) as Arc<dyn Progress>));

        assert_eq!(provider.get_docs_data(Arc::from([spec]), &tracker).await.count(), 1);
        assert_eq!(tracker.topic_state(TrackedTopic::Docs), (1, 1, TopicStatus::Done));
    }

    struct FailingWriter {
        fail_write: bool,
        fail_flush: bool,
    }

    impl tokio::io::AsyncWrite for FailingWriter {
        fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            if self.fail_write {
                Poll::Ready(Err(io::Error::other("synthetic write failure")))
            } else {
                Poll::Ready(Ok(buf.len()))
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.fail_flush {
                Poll::Ready(Err(io::Error::other("synthetic flush failure")))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
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

    /// The successful path logs the computed metrics at debug level; those arguments are only
    /// evaluated when a logger is installed, so install one that evaluates and discards records.
    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call CreateIoCompletionPort")]
    async fn successful_metrics_calculation_is_logged() {
        crate::facts::test_logging::enable_log_argument_evaluation();

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anyhow-1.0.100.json.zst");
        let zst_data = fs::read(&fixture).expect("the fixture is checked into the repository");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/crate/anyhow/1.0.100/json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(zst_data))
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().expect("temporary directories are creatable");
        let cache = Cache::new(temp_dir.path(), core::time::Duration::MAX, false);
        let provider = Provider::new(cache, Some(&server.uri()));

        let result = provider.fetch_docs_for_crate_core(&test_crate_spec("anyhow", "1.0.100")).await;

        let has_metrics = matches!(&result, ProviderResult::Found(data) if data.metrics.public_api_elements > 0);
        assert!(has_metrics, "the fixture must yield documentation metrics, got {result:?}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn download_log_reports_exact_byte_count() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anyhow-1.0.100.json.zst");
        let expected_bytes = fs::read(&fixture).expect("the fixture is checked into the repository").len();
        let logs = run_ignored_helper("helper_capture_download_log");

        assert!(logs.contains(&format!("Downloaded {expected_bytes} bytes for anyhow")), "{logs}");
    }

    #[tokio::test]
    #[ignore = "spawned by download_log_reports_exact_byte_count"]
    async fn helper_capture_download_log() {
        if std::env::var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
            return;
        }

        install_capturing_logger();

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anyhow-1.0.100.json.zst");
        let zst_data = fs::read(&fixture).expect("the fixture is checked into the repository");

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/crate/anyhow/1.0.100/json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(zst_data))
            .mount(&server)
            .await;

        let temp_dir = tempfile::tempdir().expect("temporary directories are creatable");
        let cache = Cache::new(temp_dir.path(), core::time::Duration::MAX, false);
        let provider = Provider::new(cache, Some(&server.uri()));

        let result = provider.fetch_docs_for_crate_core(&test_crate_spec("anyhow", "1.0.100")).await;
        assert!(matches!(result, ProviderResult::Found(_)), "{result:?}");

        println!("{}", captured_logs());
    }

    #[test]
    fn test_get_cache_filename() {
        let spec = test_crate_spec("tokio", "1.2.3");
        let filename = Provider::get_cache_filename(&spec);
        assert_eq!(filename, "tokio@1.2.3.bin");
    }

    #[test]
    fn test_get_cache_filename_with_special_chars() {
        let spec = test_crate_spec("my-crate", "0.1.0-beta.1");
        let filename = Provider::get_cache_filename(&spec);
        assert!(filename.contains("my-crate"));
        assert!(Path::new(&filename).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("bin")));
    }

    #[test]
    fn test_provider_new_default_url() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(temp.path(), core::time::Duration::from_hours(1), false);
        let provider = Provider::new(cache, None);
        assert_eq!(provider.base_url, DOCS_BASE_URL);
        let debug = format!("{:?}", provider.throttler);
        assert!(debug.contains("permits: 5"), "unexpected throttler state: {debug}");
    }

    #[test]
    fn test_provider_new_custom_url() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(temp.path(), core::time::Duration::from_hours(1), false);
        let provider = Provider::new(cache, Some("https://custom.docs.rs"));
        assert_eq!(provider.base_url, "https://custom.docs.rs");
    }

    #[tokio::test]
    async fn provider_sends_the_declared_user_agent() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let provider = Provider::new(Cache::new(temp.path(), core::time::Duration::MAX, false), Some(&server.uri()));

        assert!(
            provider
                .download_zst_core(&test_crate_spec("anyhow", "1.0.100"))
                .await
                .unwrap()
                .is_none()
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].headers.get("user-agent").unwrap().to_str().unwrap(), "cargo-aprz");
    }

    #[tokio::test]
    async fn transport_errors_are_returned_instead_of_panicking() {
        let temp = tempfile::tempdir().unwrap();
        let provider = Provider::new(Cache::new(temp.path(), core::time::Duration::MAX, false), Some("http://[::1"));

        let error = provider.download_zst_core(&test_crate_spec("anyhow", "1.0.100")).await.unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[tokio::test]
    async fn a_destination_that_cannot_be_created_is_an_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("payload"))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let provider = Provider::new(Cache::new(temp.path(), core::time::Duration::MAX, false), Some(&server.uri()));
        let destination = temp.path().join("missing").join("download.zst");

        let error = provider
            .download_zst_to(&test_crate_spec("anyhow", "1.0.100"), destination.clone())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("creating temp file"), "{error}");
        assert!(error.to_string().contains(&destination.display().to_string()), "{error}");
    }

    #[test]
    fn download_diagnostics_constants_are_exact() {
        assert_eq!(DOWNLOAD_OPERATION, "docs_download");
        assert_eq!(UNREADABLE_BODY_PLACEHOLDER, "<unable to read body>");
    }

    #[tokio::test]
    async fn response_stream_errors_are_propagated_with_context() {
        let stream = futures::stream::iter([Err::<bytes::Bytes, _>(io::Error::other("truncated body"))]);
        let mut writer = tokio::io::sink();

        let error = write_response_body(stream, &mut writer, Path::new("download.zst"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("reading response chunk"), "{error}");
        assert_eq!(error.source().map(ToString::to_string).as_deref(), Some("truncated body"));
    }

    #[tokio::test]
    async fn response_write_errors_are_propagated_with_the_destination() {
        let stream = futures::stream::iter([Ok::<_, io::Error>(bytes::Bytes::from_static(b"chunk"))]);
        let mut writer = FailingWriter {
            fail_write: true,
            fail_flush: false,
        };

        let error = write_response_body(stream, &mut writer, Path::new("download.zst"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("writing to temp file 'download.zst'"), "{error}");
    }

    #[tokio::test]
    async fn response_flush_errors_are_propagated_with_the_destination() {
        let stream = futures::stream::empty::<core::result::Result<bytes::Bytes, io::Error>>();
        let mut writer = FailingWriter {
            fail_write: false,
            fail_flush: true,
        };

        let error = write_response_body(stream, &mut writer, Path::new("download.zst"))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("flushing temp file 'download.zst'"), "{error}");
    }

    #[tokio::test]
    async fn downloaded_file_is_removed_after_successful_or_failed_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anyhow-1.0.100.json.zst");
        let valid = temp.path().join("valid.zst");
        fs::copy(fixture, &valid).unwrap();
        Provider::calculate_docs_metrics_and_remove(&valid, &test_crate_spec("anyhow", "1.0.100"))
            .await
            .unwrap();
        assert!(!valid.exists());

        let invalid = temp.path().join("invalid.zst");
        fs::write(&invalid, b"not zstd").unwrap();
        let _ = Provider::calculate_docs_metrics_and_remove(&invalid, &test_crate_spec("anyhow", "1.0.100"))
            .await
            .unwrap_err();
        assert!(!invalid.exists());
    }

    #[test]
    fn temp_zst_path_is_unique_per_download() {
        // Two downloads of the same crate must not share a path, otherwise one truncates or
        // deletes the other's file and decompression fails with a bogus corruption error.
        let first = temp_zst_path("anyhow", "1.0.100");
        let second = temp_zst_path("anyhow", "1.0.100");

        assert_ne!(first, second);
        assert_eq!(DOWNLOAD_ID_STEP, 1);
    }

    #[test]
    fn download_ids_advance_by_exactly_one() {
        let counter = AtomicU64::new(0);

        assert_eq!(next_download_id(&counter), 0);
        assert_eq!(next_download_id(&counter), 1);
        assert_eq!(next_download_id(&counter), 2);
    }

    #[test]
    fn temp_zst_path_identifies_the_crate_and_is_a_zst() {
        let path = temp_zst_path("anyhow", "1.0.100");
        let name = path.file_name().unwrap().to_str().unwrap();

        assert!(name.starts_with("anyhow@1.0.100-"), "unexpected file name: {name}");
        assert_eq!(path.extension().unwrap(), "zst");
        assert_eq!(path.parent().unwrap(), std::env::temp_dir());
    }

    #[test]
    fn temp_zst_path_sanitizes_path_separators() {
        // A crate name is attacker-influenced data; it must not be able to escape the temp dir.
        let path = temp_zst_path("evil/../../name", "1.0.0");

        assert_eq!(path.parent().unwrap(), std::env::temp_dir());
    }
}
