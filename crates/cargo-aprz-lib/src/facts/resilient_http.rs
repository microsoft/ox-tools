// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resilient HTTP request utilities using retry and timeout middleware.
//!
//! Configures request resilience in [`fetch`] and retries complete streamed downloads
//! with [`seatbelt`] when a response body fails after the headers arrive.

use core::time::Duration;

use bytes::{Buf, Bytes};
use fetch::http::header::{AUTHORIZATION, CONTENT_LENGTH, LOCATION, RETRY_AFTER, USER_AGENT};
use fetch::resilience::retry::{HttpRetry, HttpRetryLayerExt};
use fetch::resilience::timeout::{HttpTimeout, HttpTimeoutLayerExt};
use fetch::tokio::TokioDeps;
use fetch::{HeaderMap, HeaderValue, HttpClient, HttpResponse, StatusCode};
use futures_util::{Stream, StreamExt};
use http_body_util::BodyExt;
use layered::{Execute, Service, Stack};
use ohno::{IntoAppError, app_err};
use seatbelt::retry::{Backoff, Retry};
use seatbelt::{RecoveryInfo, ResilienceContext};
use tick::{Clock, FutureExt};
use url::Url;

/// Construct a streaming transport with request retry and timeout middleware.
///
/// Must be called inside the Tokio runtime that will execute requests.
pub fn new_client() -> HttpClient {
    configure_request_resilience(
        HttpClient::builder_tokio(TokioDeps::default())
            .tls_options(fetch::tls::TlsOptions::new_native_tls())
            .insecure_allow_http(),
    )
    .build()
}

pub(crate) fn configure_request_resilience(builder: fetch::HttpClientBuilder) -> fetch::HttpClientBuilder {
    builder.custom_pipeline(|dispatch, context| {
        (
            HttpRetry::layer("retry", context.resilience_context())
                .http_configure_defaults()
                .recovery_with(|result, _| should_retry_response(result))
                .max_retry_attempts(MAX_RETRY_ATTEMPTS)
                .base_delay(RETRY_BASE_DELAY)
                .backoff(Backoff::Exponential)
                .on_retry(|_output, args| {
                    log::debug!(
                        "retrying HTTP GET (attempt {}, delay {}ms)",
                        args.attempt().index() + 1,
                        args.retry_delay().as_millis(),
                    );
                }),
            HttpTimeout::layer("timeout", context.resilience_context())
                .http_timeout_error()
                .timeout(DEFAULT_REQUEST_TIMEOUT),
            dispatch,
        )
            .into_service()
    })
}

/// The expected wire length, when the server supplies one.
pub fn content_length(response: &HttpResponse) -> Option<u64> {
    response.headers().get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// Adapt pooled transport frames to the byte chunks used by download consumers.
pub fn response_stream(response: HttpResponse) -> impl Stream<Item = Result<Bytes, fetch::HttpError>> {
    response.into_body().into_data_stream().map(|chunk| {
        chunk.map(|mut chunk| {
            let len = chunk.remaining();
            chunk.copy_to_bytes(len)
        })
    })
}

async fn get_following_redirects(client: &HttpClient, mut url: Url, authorization: Option<HeaderValue>) -> crate::Result<HttpResponse> {
    const MAX_REDIRECTS: usize = 10;
    let mut authorization = authorization;
    let mut redirects = 0;

    loop {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(app_err!("HTTP downloads require an http:// or https:// URL"));
        }
        let mut request = client.get(url.as_str()).header(USER_AGENT, "cargo-aprz");
        if let Some(value) = &authorization {
            request = request.header(AUTHORIZATION, value.clone());
        }
        let response = request.fetch().await?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let Some(location) = response.headers().get(LOCATION) else {
            return Ok(response);
        };
        if redirects == MAX_REDIRECTS {
            return Err(app_err!(
                "HTTP redirect limit exceeded (maximum {MAX_REDIRECTS}); check the endpoint's redirect configuration"
            ));
        }
        let next = url.join(location.to_str()?)?;
        if next.origin() != url.origin() {
            authorization = None;
        }
        url = next;
        redirects += 1;
    }
}

/// Default timeout for simple HTTP requests (API calls, badge fetches, etc.)
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// Default timeout for large file downloads (docs .zst, crates DB dump, etc.)
const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

/// Maximum retry attempts (on top of the original request).
const MAX_RETRY_ATTEMPTS: u32 = 3;

/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Parse a `Retry-After` header value into a number of seconds to wait.
///
/// RFC 9110 allows either delta-seconds or an HTTP-date. Treating a date-valued header as
/// unparseable would turn a transient rate limit into an immediate failure (403) or force a
/// much longer default wait (429), so both forms are accepted. A date in the past yields `0`.
pub fn parse_retry_after_value(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    let value = value.trim();

    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }

    let when = chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT")
                .ok()
                .map(|dt| dt.and_utc())
        })
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
                .ok()
                .map(|dt| dt.and_utc())
        })?;

    Some((when - now).num_seconds().max(0).cast_unsigned())
}

/// Parse the `Retry-After` header as seconds.
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    let s = headers.get(RETRY_AFTER).and_then(|h| h.to_str().ok())?;
    parse_retry_after_value(s, chrono::Utc::now())
}

/// Classify an HTTP response for retry purposes.
fn should_retry_response(result: &fetch::Result<HttpResponse>) -> RecoveryInfo {
    match result {
        // Network / connection errors are always transient.
        Err(_) => RecoveryInfo::retry(),

        // Server errors (5xx) are transient.
        Ok(resp) if resp.status().is_server_error() => RecoveryInfo::retry(),

        // Rate-limited (429) – honor Retry-After if present, otherwise default to 5s.
        // #[gamma::skip(match_guard.always_true, tag = "timeout", reason = "treating every non-server response as rate limited repeatedly incurs retry delays")]
        Ok(resp) if resp.status() == StatusCode::TOO_MANY_REQUESTS => {
            let delay = parse_retry_after(resp.headers()).unwrap_or(5);
            RecoveryInfo::retry().delay(Duration::from_secs(delay))
        }

        // Secondary rate limit (403 with Retry-After) – wait the requested duration and retry.
        Ok(resp) if resp.status() == StatusCode::FORBIDDEN => parse_retry_after(resp.headers())
            .map_or_else(RecoveryInfo::never, |delay| RecoveryInfo::retry().delay(Duration::from_secs(delay))),

        // Everything else (success, 4xx client errors) is not retried.
        _ => RecoveryInfo::never(),
    }
}

/// Send an HTTP GET request with automatic retry and timeout.
///
/// Retries on network errors, `5xx`, and 429 responses with exponential backoff.
pub async fn resilient_get(client: &HttpClient, url: &str) -> crate::Result<HttpResponse> {
    resilient_get_authorized(client, url, None).await
}

/// Send a GET with a sensitive authorization header and the common retry policy.
pub async fn resilient_get_authorized(client: &HttpClient, url: &str, authorization: Option<HeaderValue>) -> crate::Result<HttpResponse> {
    get_following_redirects(client, Url::parse(url)?, authorization).await
}

/// Execute an async download operation with automatic retry and timeout.
///
/// Wraps an entire download (connect + stream) so that mid-stream failures
/// cause a full retry from scratch. Use this for file downloads where the
/// streaming body can fail independently of the initial connection.
///
/// `name` is used for telemetry / logging identification.
/// `download_fn` is called on each attempt with a clone of `input`.
pub async fn resilient_download<In, Out, Fut, F>(
    name: &'static str,
    input: In,
    timeout: Option<Duration>,
    download_fn: F,
) -> crate::Result<Out>
where
    In: Clone + Send + Sync + 'static,
    Out: Send + 'static,
    Fut: Future<Output = crate::Result<Out>> + Send,
    F: Fn(In) -> Fut + Send + Sync + Clone + 'static,
{
    resilient_download_with_clock(name, input, timeout, download_fn, Clock::new_tokio()).await
}

pub(crate) async fn resilient_download_with_clock<In, Out, Fut, F>(
    name: &'static str,
    input: In,
    timeout: Option<Duration>,
    download_fn: F,
    clock: Clock,
) -> crate::Result<Out>
where
    In: Clone + Send + Sync + 'static,
    Out: Send + 'static,
    Fut: Future<Output = crate::Result<Out>> + Send,
    F: Fn(In) -> Fut + Send + Sync + Clone + 'static,
{
    let timeout_duration = timeout.unwrap_or(DEFAULT_DOWNLOAD_TIMEOUT);
    let retry_clock = clock.clone();
    retry_download_with_clock(
        name,
        input,
        move |input| {
            let clock = clock.clone();
            let download = download_fn.clone();
            async move {
                download(input)
                    .timeout(&clock, timeout_duration)
                    .await
                    .into_app_err("download timed out")?
            }
        },
        retry_clock,
    )
    .await
}

pub(crate) async fn retry_download_with_clock<In, Out, Fut, F>(
    name: &'static str,
    input: In,
    download_fn: F,
    clock: Clock,
) -> crate::Result<Out>
where
    In: Clone + Send + Sync + 'static,
    Out: Send + 'static,
    Fut: Future<Output = crate::Result<Out>> + Send,
    F: Fn(In) -> Fut + Send + Sync + Clone + 'static,
{
    let context = ResilienceContext::new(&clock).name(name);

    let service = (
        Retry::layer(stringify!(retry), &context)
            .clone_input()
            .recovery_with(|result: &crate::Result<Out>, _| match result {
                Err(_) => RecoveryInfo::retry(),
                Ok(_) => RecoveryInfo::never(),
            })
            .max_retry_attempts(MAX_RETRY_ATTEMPTS)
            .base_delay(RETRY_BASE_DELAY)
            .backoff(Backoff::Exponential)
            .on_retry(|_output, args| {
                log::debug!(
                    "retrying download (attempt {}, delay {}ms)",
                    args.attempt().index() + 1,
                    args.retry_delay().as_millis(),
                );
            }),
        Execute::new(move |input: In| {
            let f = download_fn.clone();
            async move { f(input).await }
        }),
    )
        .into_service();

    service.execute(input).await
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use seatbelt::RecoveryKind;

    use super::*;

    #[test]
    fn parses_delta_seconds() {
        let now = Utc.with_ymd_and_hms(1994, 11, 6, 8, 49, 37).unwrap();
        assert_eq!(parse_retry_after_value("120", now), Some(120));
        assert_eq!(parse_retry_after_value("  120 ", now), Some(120));
        assert_eq!(parse_retry_after_value("0", now), Some(0));
    }

    #[test]
    fn parses_http_date() {
        let now = Utc.with_ymd_and_hms(1994, 11, 6, 8, 49, 37).unwrap();

        // IMF-fixdate, the form servers are required to send.
        assert_eq!(parse_retry_after_value("Sun, 06 Nov 1994 08:51:37 GMT", now), Some(120));

        // Obsolete formats that RFC 9110 still requires recipients to accept.
        assert_eq!(parse_retry_after_value("Sunday, 06-Nov-94 08:51:37 GMT", now), Some(120));
        assert_eq!(parse_retry_after_value("Sun Nov  6 08:51:37 1994", now), Some(120));
    }

    #[test]
    fn past_dates_do_not_underflow() {
        let now = Utc.with_ymd_and_hms(1994, 11, 6, 8, 49, 37).unwrap();
        assert_eq!(parse_retry_after_value("Sun, 06 Nov 1994 08:00:00 GMT", now), Some(0));
    }

    #[test]
    fn rejects_unparseable_values() {
        let now = Utc.with_ymd_and_hms(1994, 11, 6, 8, 49, 37).unwrap();
        assert_eq!(parse_retry_after_value("soon", now), None);
        assert_eq!(parse_retry_after_value("", now), None);
        assert_eq!(parse_retry_after_value("-5", now), None);
    }

    #[test]
    fn default_resilience_durations_and_retry_count_are_pinned() {
        assert_eq!(DEFAULT_REQUEST_TIMEOUT, Duration::from_mins(1));
        assert_eq!(DEFAULT_DOWNLOAD_TIMEOUT, Duration::from_mins(10));
        assert_eq!(MAX_RETRY_ATTEMPTS, 3);
        assert_eq!(RETRY_BASE_DELAY, Duration::from_secs(1));
    }

    #[test]
    fn network_errors_are_classified_as_retryable() {
        let err = Err(fetch::HttpError::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection failed",
        )));

        let recovery = should_retry_response(&err);

        assert_eq!(recovery.kind(), RecoveryKind::Retry);
        assert_eq!(recovery.get_delay(), None);
    }

    // -----------------------------------------------------------------------
    // Retry behaviour
    // -----------------------------------------------------------------------

    use core::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A `Retry-After: 0` header keeps the retry delay at zero so tests stay fast.
    fn immediate_retry_after(status: u16) -> ResponseTemplate {
        ResponseTemplate::new(status).insert_header("retry-after", "0")
    }

    fn credential() -> HeaderValue {
        let mut value = HeaderValue::from_static("token secret");
        value.set_sensitive(true);
        value
    }

    #[tokio::test]
    async fn follows_relative_redirects_and_preserves_same_origin_credentials() {
        let server = MockServer::start().await;
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/final?download=1"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/final"))
            .and(wiremock::matchers::query_param("download", "1"))
            .and(wiremock::matchers::header("authorization", "token secret"))
            .and(wiremock::matchers::header("user-agent", "cargo-aprz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("download"))
            .expect(1)
            .mount(&server)
            .await;

        let response = resilient_get_authorized(&new_client(), &format!("{}/start", server.uri()), Some(credential()))
            .await
            .expect("relative redirects should resolve against the current URL");

        assert_eq!(response.into_body().into_text().await.unwrap(), "download");
    }

    #[tokio::test]
    async fn cross_origin_redirects_permanently_drop_credentials() {
        let origin = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(path("/start"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", format!("{}/redirect", other.uri())))
            .expect(1)
            .mount(&origin)
            .await;
        Mock::given(path("/redirect"))
            .respond_with(ResponseTemplate::new(308).insert_header("location", format!("{}/final", origin.uri())))
            .expect(1)
            .mount(&other)
            .await;
        Mock::given(path("/final"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&origin)
            .await;

        resilient_get_authorized(&new_client(), &format!("{}/start", origin.uri()), Some(credential()))
            .await
            .expect("redirecting away and back should succeed");

        let mut requests = origin.received_requests().await.unwrap();
        requests.sort_by(|left, right| left.url.path().cmp(right.url.path()));
        let others = other.received_requests().await.unwrap();
        assert_eq!(
            (
                requests[0].headers.get(AUTHORIZATION).cloned(),
                requests[1].headers.get(AUTHORIZATION).cloned(),
                others[0].headers.get(AUTHORIZATION).cloned()
            ),
            (None, Some(HeaderValue::from_static("token secret")), None)
        );
    }

    #[tokio::test]
    async fn redirect_loops_stop_after_ten_hops() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/loop"))
            .expect(11)
            .mount(&server)
            .await;

        let error = get_following_redirects(&new_client(), Url::parse(&server.uri()).unwrap(), None)
            .await
            .expect_err("a redirect loop must be bounded");

        assert!(error.to_string().contains("redirect limit exceeded"), "{error}");
    }

    #[tokio::test]
    async fn redirects_cannot_switch_to_non_http_schemes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "file:///outside"))
            .expect(1)
            .mount(&server)
            .await;

        let error = get_following_redirects(&new_client(), Url::parse(&server.uri()).unwrap(), None)
            .await
            .expect_err("redirects must not access other URL schemes");

        assert!(error.to_string().contains("http:// or https://"), "{error}");
    }

    #[tokio::test]
    async fn redirects_without_a_location_return_the_original_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302))
            .expect(1)
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &server.uri()).await.unwrap();

        assert_eq!(response.status(), StatusCode::FOUND);
    }

    #[tokio::test]
    async fn initial_endpoints_must_use_http_or_https() {
        let error = resilient_get(&new_client(), "file:///outside")
            .await
            .expect_err("non-HTTP endpoints must be rejected");

        assert!(error.to_string().contains("http:// or https://"), "{error}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn rate_limited_responses_are_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(immediate_retry_after(429))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &format!("{}/thing", server.uri())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.into_body().into_text().await.unwrap(), "ok");
    }

    async fn retry_classification(template: ResponseTemplate) -> RecoveryInfo {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(template).mount(&server).await;
        let result = HttpClient::builder_tokio(TokioDeps::default())
            .insecure_allow_http()
            .minimal_pipeline()
            .build()
            .get(server.uri())
            .fetch()
            .await;
        should_retry_response(&result)
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn rate_limit_retry_delay_uses_the_header_or_exact_default() {
        let explicit = retry_classification(ResponseTemplate::new(429).insert_header("retry-after", "17")).await;
        assert_eq!(explicit.kind(), RecoveryKind::Retry);
        assert_eq!(explicit.get_delay(), Some(Duration::from_secs(17)));

        let defaulted = retry_classification(ResponseTemplate::new(429)).await;
        assert_eq!(defaulted.kind(), RecoveryKind::Retry);
        assert_eq!(defaulted.get_delay(), Some(Duration::from_secs(5)));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn get_uses_exactly_the_configured_number_of_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(immediate_retry_after(429))
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &server.uri())
            .await
            .expect("HTTP responses are returned after retry exhaustion");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(server.received_requests().await.expect("wiremock records requests").len(), 4);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn forbidden_with_retry_after_is_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(immediate_retry_after(403))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &format!("{}/thing", server.uri())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn forbidden_without_retry_after_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &format!("{}/thing", server.uri())).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn server_errors_are_retried() {
        // The retry notice is logged at debug level; evaluate its arguments too.
        crate::facts::test_logging::enable_log_argument_evaluation();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &format!("{}/thing", server.uri())).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn client_errors_are_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &format!("{}/thing", server.uri())).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires network I/O")]
    async fn client_errors_with_retry_after_are_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/thing"))
            .respond_with(immediate_retry_after(404))
            .expect(1)
            .mount(&server)
            .await;

        let response = resilient_get(&new_client(), &format!("{}/thing", server.uri())).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers")]
    async fn download_returns_the_first_successful_result() {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&attempts);

        let result = resilient_download("test_download", 41_u32, None, move |input: u32| {
            let counter = Arc::clone(&counter);
            async move {
                let _ = counter.fetch_add(1, Ordering::Relaxed);
                Ok(input + 1)
            }
        })
        .await
        .unwrap();

        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers")]
    async fn download_retries_after_a_failure() {
        // The retry notice is logged at debug level; evaluate its arguments too.
        crate::facts::test_logging::enable_log_argument_evaluation();

        let attempts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&attempts);

        let result = resilient_download("test_download", "input", Some(Duration::from_secs(5)), move |input: &str| {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    return Err(app_err!("transient failure"));
                }
                Ok(input.to_owned())
            }
        })
        .await
        .unwrap();

        assert_eq!(result, "input");
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers")]
    async fn download_honors_a_custom_timeout() {
        let error = resilient_download("test_download", (), Some(Duration::from_millis(1)), delayed_success)
            .await
            .expect_err("every attempt must exceed the custom timeout");

        assert!(error.to_string().contains("download timed out"), "{error}");
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn delayed_success(_unit: ()) -> crate::Result<()> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "requires tokio timers")]
    async fn download_uses_exactly_the_configured_number_of_retries() {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&attempts);

        let error = resilient_download("test_download", (), Some(Duration::from_secs(30)), move |()| {
            let counter = Arc::clone(&counter);
            async move {
                let _ = counter.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(app_err!("always fails"))
            }
        })
        .await
        .expect_err("all attempts fail");

        assert!(error.to_string().contains("always fails"), "{error}");
        assert_eq!(attempts.load(Ordering::Relaxed), 4);
    }
}
