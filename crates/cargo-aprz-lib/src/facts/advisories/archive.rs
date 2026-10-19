// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use futures_util::StreamExt;
use ohno::{IntoAppError, bail};
use tar::Archive;
use tokio::io::AsyncWriteExt;

use crate::Result;
use crate::facts::resilient_http;

// A normal RustSec archive is a few MB compressed and tens of MB unpacked.
const MAX_COMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;

pub(super) async fn download(destination: &Path, url: &str, timeout: Duration, client: fetch::HttpClient) -> Result<()> {
    let url = url.to_owned();
    let destination = destination.to_path_buf();
    let archive_file = resilient_http::resilient_download(
        "advisory_archive",
        (destination.clone(), url, client),
        Some(timeout),
        |(destination, url, client)| async move {
            let response = resilient_http::resilient_get(&client, &url).await?;
            if !response.status().is_success() {
                bail!(
                    "unable to download advisory archive: HTTP {}; configure a .tar.gz endpoint with --advisory-archive-url",
                    response.status()
                );
            }
            if let Some(size) = resilient_http::content_length(&response) {
                check_download_size(size)?;
            }
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
                check_download_size(downloaded)?;
                output.write_all(&chunk).await?;
            }
            output.flush().await?;
            drop(output);
            Ok(archive_file)
        },
    )
    .await?;
    tokio::task::spawn_blocking(move || {
        extract(archive_file.path(), &destination)?;
        archive_file.close()?;
        Ok(())
    })
    .await?
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
    let decoder = GzDecoder::new(input).take(max_bytes + 1);
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
        let response = fetch::HttpResponseBuilder::new_fake()
            .status(fetch::StatusCode::OK)
            .header("content-length", "67108865")
            .build()
            .expect("the fake response has valid headers");
        let client = fetch::HttpClient::new_fake(response);
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
