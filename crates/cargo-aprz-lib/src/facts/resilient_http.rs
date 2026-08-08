// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resilient HTTP request utilities using retry and timeout middleware.
//!
//! Wraps HTTP operations with [`seatbelt`] retry and timeout middleware so that
//! transient network failures are masked automatically.

use core::time::Duration;
use layered::{Execute, Service, Stack};
use ohno::app_err;
use seatbelt::retry::{Backoff, Retry};
use seatbelt::timeout::Timeout;
use seatbelt::{RecoveryInfo, ResilienceContext};
use tick::Clock;

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
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let s = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())?;
    parse_retry_after_value(s, chrono::Utc::now())
}

/// Classify an HTTP response for retry purposes.
fn should_retry_response(result: &crate::Result<reqwest::Response>) -> RecoveryInfo {
    match result {
        // Network / connection errors are always transient.
        Err(_) => RecoveryInfo::retry(),

        // Server errors (5xx) are transient.
        Ok(resp) if resp.status().is_server_error() => RecoveryInfo::retry(),

        // Rate-limited (429) – honor Retry-After if present, otherwise default to 5s.
        Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
            let delay = parse_retry_after(resp.headers()).unwrap_or(5);
            RecoveryInfo::retry().delay(Duration::from_secs(delay))
        }

        // Secondary rate limit (403 with Retry-After) – wait the requested duration and retry.
        Ok(resp) if resp.status() == reqwest::StatusCode::FORBIDDEN => {
            parse_retry_after(resp.headers()).map_or_else(RecoveryInfo::never, |delay| {
                RecoveryInfo::retry().delay(Duration::from_secs(delay))
            })
        }

        // Everything else (success, 4xx client errors) is not retried.
        _ => RecoveryInfo::never(),
    }
}

/// Send an HTTP GET request with automatic retry and timeout.
///
/// Retries on network errors, 5xx, and 429 responses with exponential backoff.
pub async fn resilient_get(client: &reqwest::Client, url: &str) -> crate::Result<reqwest::Response> {
    let clock = Clock::new_tokio();
    let context = ResilienceContext::new(&clock).name("http_get");

    let client = client.clone();
    let service = (
        Retry::layer("retry", &context)
            .clone_input()
            .recovery_with(|result: &crate::Result<reqwest::Response>, _| should_retry_response(result))
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
        Timeout::layer("timeout", &context)
            .timeout_error(|_| app_err!("HTTP request timed out"))
            .timeout(DEFAULT_REQUEST_TIMEOUT),
        Execute::new(move |url: String| {
            let client = client.clone();
            async move {
                client
                    .get(&url)
                    .send()
                    .await
                    .map_err(ohno::AppError::from)
            }
        }),
    )
        .into_service();

    service.execute(url.to_string()).await
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
    let clock = Clock::new_tokio();
    let context = ResilienceContext::new(&clock).name(name);
    let timeout_duration = timeout.unwrap_or(DEFAULT_DOWNLOAD_TIMEOUT);

    let service = (
        Retry::layer("retry", &context)
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
        Timeout::layer("timeout", &context)
            .timeout_error(|_| app_err!("download timed out"))
            .timeout(timeout_duration),
        Execute::new(move |input: In| {
            let f = download_fn.clone();
            async move { f(input).await }
        }),
    )
        .into_service();

    service.execute(input).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone as _, Utc};

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
}
