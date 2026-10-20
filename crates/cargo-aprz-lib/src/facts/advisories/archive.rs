// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use flate2::read::MultiGzDecoder;
use futures_util::StreamExt;
use ohno::{IntoAppError, bail};
use tar::Archive;
use tick::FutureExt;
use tokio::io::AsyncWriteExt;

use crate::Result;
use crate::facts::resilient_http;

// A normal RustSec archive is a few MB compressed and tens of MB unpacked.
const MAX_COMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;

pub(super) async fn download(destination: &Path, url: &str, timeout: Duration, client: fetch::HttpClient) -> Result<()> {
    download_with_clock(destination, url, timeout, client, tick::Clock::new_tokio()).await
}

pub(super) async fn download_with_clock(
    destination: &Path,
    url: &str,
    timeout: Duration,
    client: fetch::HttpClient,
    clock: tick::Clock,
) -> Result<()> {
    let url = url.to_owned();
    let destination = destination.to_path_buf();
    let archive_file = resilient_http::retry_download_with_clock(
        "advisory_archive",
        (destination.clone(), url, client, clock.clone()),
        move |(destination, url, client, clock)| async move {
            // Request resilience is already exhausted; only body failures need a whole-transfer retry.
            let response = match resilient_http::resilient_get(&client, &url).await {
                Ok(response) => response,
                Err(error) => return Ok(Err(error)),
            };
            if !response.status().is_success() {
                return Ok(Err(ohno::app_err!(
                    "unable to download advisory archive: HTTP {}; configure a .tar.gz endpoint with --advisory-archive-url",
                    response.status()
                )));
            }
            if let Some(size) = resilient_http::content_length(&response)
                && let Err(error) = check_download_size(size)
            {
                return Ok(Err(error));
            }
            write_archive(response, destination)
                .timeout(&clock, timeout)
                .await
                .into_app_err("download timed out")?
        },
        clock,
    )
    .await??;
    tokio::task::spawn_blocking(move || {
        extract(archive_file.path(), &destination)?;
        archive_file.close()?;
        Ok(())
    })
    .await?
}

async fn write_archive(response: fetch::HttpResponse, destination: std::path::PathBuf) -> Result<Result<tempfile::NamedTempFile>> {
    let (archive_file, output) = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let archive_file = tempfile::NamedTempFile::new_in(destination)?;
        let output = archive_file.reopen()?;
        Ok((archive_file, output))
    })
    .await??;
    let mut output = tokio::fs::File::from_std(output);
    let mut stream = std::pin::pin!(resilient_http::response_stream(response));
    let mut downloaded = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        downloaded += chunk.len() as u64;
        if let Err(error) = check_download_size(downloaded) {
            return Ok(Err(error));
        }
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    drop(output);
    Ok(Ok(archive_file))
}

fn check_download_size(bytes: u64) -> Result<()> {
    if bytes > MAX_COMPRESSED_BYTES {
        bail!("advisory archive exceeds the {MAX_COMPRESSED_BYTES}-byte compressed size limit");
    }
    Ok(())
}

fn extract(archive_path: &Path, destination: &Path) -> Result<()> {
    let contents = tempfile::Builder::new().prefix(".unpack-").tempdir_in(destination)?;
    let input = File::open(archive_path)?;
    unpack(input, contents.path())?;
    let root = database_root(contents.path())?;
    // Never publish an empty or malformed database as a successful refresh.
    let database = rustsec::database::Database::open(&root).into_app_err("parsing the advisory archive")?;
    if database.iter().next().is_none() {
        bail!("advisory archive contains no advisories; check --advisory-archive-url");
    }
    std::fs::rename(root.join("crates"), destination.join("crates"))?;
    if root.join("rust").is_dir() {
        std::fs::rename(root.join("rust"), destination.join("rust"))?;
    }
    Ok(())
}

fn unpack(input: impl Read, destination: &Path) -> Result<()> {
    unpack_with_limits(input, destination, MAX_UNPACKED_BYTES, MAX_ENTRIES)
}

fn unpack_with_limits(input: impl Read, destination: &Path, max_bytes: u64, max_entries: usize) -> Result<()> {
    let decoder = MultiGzDecoder::new(input).take(max_bytes + 1);
    let mut archive = Archive::new(decoder);
    let mut declared_bytes = 0_u64;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= max_entries {
            bail!("advisory archive exceeds the {max_entries}-entry limit");
        }
        let mut entry = entry?;
        if entry.size() > max_bytes - declared_bytes {
            bail!("advisory archive exceeds the {max_bytes}-byte uncompressed size limit");
        }
        declared_bytes += entry.size();
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        if !kind.is_dir() && !kind.is_file() {
            bail!("advisory archives may contain only regular files and directories, not links or special entries");
        }
        let path = entry.path()?;
        let text = path
            .to_str()
            .ok_or_else(|| ohno::app_err!("advisory archive paths must be UTF-8"))?;
        if text.contains(['\\', ':'])
            || path
                .components()
                .any(|part| matches!(part, Component::Prefix(_) | Component::RootDir))
        {
            bail!("advisory archive contains an unsafe path; use an archive with only relative paths");
        }
        if !entry.unpack_in(destination)? {
            bail!("advisory archive contains an unsafe path that escapes the extraction directory");
        }
    }
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    if decoder.limit() == 0 {
        bail!("advisory archive exceeds the {max_bytes}-byte uncompressed size limit");
    }
    Ok(())
}

fn database_root(contents: &Path) -> Result<std::path::PathBuf> {
    if contents.join("crates").is_dir() {
        return Ok(contents.to_path_buf());
    }
    let mut roots = std::fs::read_dir(contents)?;
    if let Some(root) = roots.next() {
        let root = root?;
        if roots.next().is_none() && root.file_type()?.is_dir() && root.path().join("crates").is_dir() {
            return Ok(root.path());
        }
    }
    bail!("advisory archive must contain crates/ at its root or under one enclosing directory; check --advisory-archive-url")
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::io::Write;
    use std::sync::Arc;

    use super::*;

    fn extract_fixture(name: &str) -> Result<rustsec::database::Database> {
        let stage = tempfile::tempdir()?;
        let archive = stage.path().join("input.tar.gz");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("advisories")
            .join(name);
        std::fs::copy(source, &archive)?;

        extract(&archive, stage.path())?;

        Ok(rustsec::database::Database::open(stage.path())?)
    }

    #[test]
    fn reads_a_database_at_the_archive_root() {
        let database = extract_fixture("root.tar.gz").expect("root fixture contains a valid database");

        assert_eq!(
            database
                .iter()
                .map(|advisory| advisory.metadata.package.as_str())
                .collect::<Vec<_>>(),
            ["archive-crate"]
        );
    }

    #[test]
    fn reads_a_database_under_a_single_enclosing_directory() {
        let database = extract_fixture("wrapped.tar.gz").expect("wrapped fixture contains a valid database");

        assert_eq!(
            database
                .iter()
                .map(|advisory| advisory.metadata.package.as_str())
                .collect::<Vec<_>>(),
            ["archive-crate"]
        );
    }

    fn multi_member_archive() -> Vec<u8> {
        let mut archive = tar::Builder::new(Vec::new());
        let mut split = 0;
        for id in ["RUSTSEC-2020-0001", "RUSTSEC-2020-0002"] {
            let advisory = format!(
                "```toml\n[advisory]\nid = \"{id}\"\npackage = \"archive-crate\"\ndate = \"2020-01-01\"\n\
                 [versions]\npatched = []\n```\n\n# Test advisory\n\nTest advisory body.\n"
            );
            let mut header = tar::Header::new_gnu();
            header.set_size(advisory.len() as u64);
            header.set_mode(0o644);
            archive
                .append_data(&mut header, format!("crates/archive-crate/{id}.md"), advisory.as_bytes())
                .unwrap();
            if split == 0 {
                split = archive.get_ref().len();
            }
        }
        let tar = archive.into_inner().unwrap();
        let mut compressed = Vec::new();
        for member in [&tar[..split], &tar[split..]] {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(member).unwrap();
            compressed.extend(encoder.finish().unwrap());
        }
        compressed
    }

    #[test]
    fn reads_advisories_across_gzip_member_boundaries() {
        let stage = tempfile::tempdir().unwrap();
        let archive = stage.path().join("input.tar.gz");
        std::fs::write(&archive, multi_member_archive()).unwrap();

        extract(&archive, stage.path()).expect("gzip members form one complete tar archive");

        let database = rustsec::database::Database::open(stage.path()).unwrap();
        let mut ids = database.iter().map(|advisory| advisory.metadata.id.to_string()).collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["RUSTSEC-2020-0001", "RUSTSEC-2020-0002"]);
    }

    #[test]
    fn rejects_a_corrupt_later_gzip_member_before_publishing_database_files() {
        let stage = tempfile::tempdir().unwrap();
        let archive = stage.path().join("input.tar.gz");
        let mut compressed = multi_member_archive();
        let checksum = compressed.len() - 8;
        compressed[checksum] ^= 1;
        std::fs::write(&archive, compressed).unwrap();

        extract(&archive, stage.path()).expect_err("a later member's checksum must be validated");

        assert!(
            !stage.path().join("crates").exists(),
            "the first member must not be published alone"
        );
    }

    #[test]
    fn uncompressed_size_limit_covers_all_gzip_members() {
        let stage = tempfile::tempdir().unwrap();
        let compressed = multi_member_archive();

        unpack_with_limits(compressed.as_slice(), stage.path(), 2048, MAX_ENTRIES)
            .expect_err("the combined tar entries and padding exceed the budget");
    }

    #[test]
    fn accepts_the_global_pax_metadata_used_by_github_archives() {
        let database = extract_fixture("github-pax.tar.gz").expect("GitHub metadata is not a filesystem entry");

        assert_eq!(database.iter().count(), 1);
    }

    #[test]
    fn preserves_the_optional_rust_advisory_collection() {
        let stage = tempfile::tempdir().unwrap();
        let archive = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("advisories")
            .join("with-rust.tar.gz");

        extract(&archive, stage.path()).expect("an optional rust/ collection is supported");

        assert!(stage.path().join("rust").is_dir());
    }

    #[test]
    fn compressed_download_size_includes_the_exact_limit() {
        check_download_size(MAX_COMPRESSED_BYTES).expect("the limit itself is permitted");
    }

    #[test]
    fn rejects_downloads_one_byte_over_the_compressed_limit() {
        let error = check_download_size(MAX_COMPRESSED_BYTES + 1).expect_err("oversized downloads must fail");

        assert!(error.to_string().contains("compressed size limit"), "{error}");
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_before_reading_the_body() {
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&calls);
        let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_fn(move |_| {
            recorded.fetch_add(1, Ordering::SeqCst);
            fetch::HttpResponseBuilder::new_fake()
                .status(fetch::StatusCode::OK)
                .header("content-length", "67108865")
                .build()
        }));
        let destination = tempfile::tempdir().unwrap();

        let error = download(
            destination.path(),
            "https://example.test/database.tar.gz",
            Duration::from_mins(1),
            client,
        )
        .await
        .expect_err("the advertised size must be checked before parsing an empty body");

        assert!(error.to_string().contains("compressed size limit"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn http_status_failures_do_not_restart_the_whole_transfer() {
        for status in [fetch::StatusCode::NOT_FOUND, fetch::StatusCode::INTERNAL_SERVER_ERROR] {
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = Arc::clone(&calls);
            let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_fn(move |_| {
                recorded.fetch_add(1, Ordering::SeqCst);
                fetch::HttpResponseBuilder::new_fake().status(status).build()
            }));
            let destination = tempfile::tempdir().unwrap();

            let error = download(
                destination.path(),
                "https://example.test/database.tar.gz",
                Duration::from_mins(1),
                client,
            )
            .await
            .expect_err("HTTP failures must be returned without an outer retry");

            assert!(error.to_string().contains(&format!("HTTP {status}")), "{error}");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_request_errors_do_not_restart_the_whole_transfer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&calls);
        let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_error_fn(move |_| {
            recorded.fetch_add(1, Ordering::SeqCst);
            fetch::HttpError::from(std::io::Error::other("request resilience exhausted"))
        }));
        let destination = tempfile::tempdir().unwrap();

        let error = download(
            destination.path(),
            "https://example.test/database.tar.gz",
            Duration::from_mins(1),
            client,
        )
        .await
        .expect_err("request failures have already exhausted native resilience");

        assert!(format!("{error:#}").contains("request resilience exhausted"), "{error:#}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn request_retry_after_waits_do_not_consume_the_body_timeout() {
        for status in [fetch::StatusCode::TOO_MANY_REQUESTS, fetch::StatusCode::FORBIDDEN] {
            let control = tick::ClockControl::new();
            let clock = tick::Clock::from(control.clone());
            let calls = Arc::new(AtomicUsize::new(0));
            let recorded = Arc::clone(&calls);
            let handler = fetch::fake::FakeHandler::from_fn(move |_| {
                if recorded.fetch_add(1, Ordering::SeqCst) == 0 {
                    fetch::HttpResponseBuilder::new_fake()
                        .status(status)
                        .header("retry-after", "120")
                        .build()
                } else {
                    fetch::HttpResponseBuilder::new_fake()
                        .bytes(bytes::Bytes::from_static(include_bytes!(
                            "../../../tests/fixtures/advisories/root.tar.gz"
                        )))
                        .build()
                }
            });
            let client = resilient_http::configure_request_resilience(fetch::HttpClient::builder_fake(handler, &clock)).build();
            let destination = tempfile::tempdir().unwrap();
            let download = download_with_clock(
                destination.path(),
                "https://example.test/database.tar.gz",
                Duration::from_mins(1),
                client,
                clock,
            );
            let mut download = std::pin::pin!(download);
            tokio::time::timeout(Duration::from_secs(10), async {
                while calls.load(Ordering::SeqCst) == 0 {
                    assert!(futures_util::poll!(&mut download).is_pending());
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the fake request handler must be reached within the safety deadline");

            control.advance(Duration::from_secs(61));
            assert!(futures_util::poll!(&mut download).is_pending());
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the body timeout must not restart a waiting request"
            );
            control.advance(Duration::from_secs(58));
            assert!(futures_util::poll!(&mut download).is_pending());
            assert_eq!(calls.load(Ordering::SeqCst), 1, "Retry-After must elapse before the next GET");

            control.advance(Duration::from_secs(1));
            download
                .await
                .expect("the body receives its own timeout after the request retry succeeds");

            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(rustsec::database::Database::open(destination.path()).unwrap().iter().count(), 1);
        }
    }

    #[tokio::test]
    async fn streamed_compressed_size_violations_are_not_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&calls);
        let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_fn(move |_| {
            recorded.fetch_add(1, Ordering::SeqCst);
            fetch::HttpResponseBuilder::new_fake()
                .stream(futures_util::stream::iter(
                    (0..65).map(|_| Ok(bytes::Bytes::from(vec![0_u8; 1024 * 1024]).into())),
                ))
                .build()
        }));
        let destination = tempfile::tempdir().unwrap();

        let error = download(
            destination.path(),
            "https://example.test/database.tar.gz",
            Duration::from_mins(1),
            client,
        )
        .await
        .expect_err("streaming must enforce the limit without a content-length header");

        assert!(error.to_string().contains("compressed size limit"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn counts_entries_before_extracting_them() {
        let destination = tempfile::tempdir().unwrap();
        let input = include_bytes!("../../../tests/fixtures/advisories/wrapped.tar.gz");

        let error = unpack_with_limits(input.as_slice(), destination.path(), MAX_UNPACKED_BYTES, 0)
            .expect_err("a zero-entry budget cannot admit a file");

        assert!(error.to_string().contains("0-entry limit"), "{error}");
    }

    #[test]
    fn includes_tar_padding_in_the_uncompressed_size_limit() {
        let destination = tempfile::tempdir().unwrap();
        let input = include_bytes!("../../../tests/fixtures/advisories/wrapped.tar.gz");

        let error = unpack_with_limits(input.as_slice(), destination.path(), 4096, MAX_ENTRIES)
            .expect_err("the small fixture has more than 4096 uncompressed bytes, including padding");

        assert!(error.to_string().contains("4096-byte uncompressed size limit"), "{error}");
    }

    macro_rules! rejected_archive {
        ($name:ident, $fixture:literal, $message:literal) => {
            #[test]
            fn $name() {
                let error = extract_fixture($fixture).expect_err("invalid archive must not be accepted");

                assert!(format!("{error:#}").contains($message), "{error:#}");
            }
        };
    }

    rejected_archive!(
        rejects_malformed_advisories,
        "invalid-database.tar.gz",
        "parsing the advisory archive"
    );
    rejected_archive!(rejects_missing_database, "missing-crates.tar.gz", "must contain crates/");
    rejected_archive!(rejects_empty_database, "empty-database.tar.gz", "contains no advisories");
    rejected_archive!(rejects_parent_paths, "parent-path.tar.gz", "unsafe path");
    rejected_archive!(rejects_absolute_paths, "absolute-path.tar.gz", "unsafe path");
    rejected_archive!(rejects_windows_traversal_on_every_platform, "backslash-path.tar.gz", "unsafe path");
    rejected_archive!(rejects_symbolic_links, "symlink.tar.gz", "not links");
    rejected_archive!(rejects_hard_links, "hardlink.tar.gz", "not links");
    rejected_archive!(rejects_oversized_entries, "oversized.tar.gz", "uncompressed size limit");
    rejected_archive!(checks_the_gzip_trailer, "truncated.tar.gz", "unexpected end");
}
