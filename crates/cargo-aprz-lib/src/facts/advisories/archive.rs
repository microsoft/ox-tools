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

pub async fn download(destination: &Path, url: &str, timeout: Duration, client: fetch::HttpClient) -> Result<()> {
    download_with_clock(destination, url, timeout, client, tick::Clock::new_tokio()).await
}

pub async fn download_with_clock(
    destination: &Path,
    url: &str,
    timeout: Duration,
    client: fetch::HttpClient,
    clock: tick::Clock,
) -> Result<()> {
    download_with_io(destination, url, timeout, client, clock, write_archive, extract_archive).await
}

pub async fn download_with_io<W, F>(
    destination: &Path,
    url: &str,
    timeout: Duration,
    client: fetch::HttpClient,
    clock: tick::Clock,
    write: W,
    extract: impl FnOnce(&tempfile::NamedTempFile, &Path) -> Result<()> + Send + 'static,
) -> Result<()>
where
    W: Fn(fetch::HttpResponse, std::path::PathBuf) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = Result<Result<tempfile::NamedTempFile>>> + Send,
{
    let url = url.to_owned();
    let destination = destination.to_path_buf();
    let archive_file = resilient_http::retry_download_with_clock(
        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "This private resilience context enables neither logs nor metrics; its name is not exported.", tag = "equivalent")]
        "advisory_archive",
        (destination.clone(), url, client, clock.clone(), write),
        move |(destination, url, client, clock, write)| async move {
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
            write(response, destination)
                .timeout(&clock, timeout)
                .await
                .into_app_err("download timed out")?
        },
        clock,
    )
    .await??;
    tokio::task::spawn_blocking(move || {
        extract(&archive_file, &destination)?;
        archive_file.close()?;
        Ok(())
    })
    .await?
}

pub fn extract_archive(archive: &tempfile::NamedTempFile, destination: &Path) -> Result<()> {
    extract(File::open(archive.path())?, destination)
}

pub async fn write_archive(response: fetch::HttpResponse, destination: std::path::PathBuf) -> Result<Result<tempfile::NamedTempFile>> {
    write_archive_with(response, destination, |destination| {
        let archive = tempfile::NamedTempFile::new_in(destination)?;
        let output = archive.reopen()?;
        Ok((archive, output))
    })
    .await
}

pub async fn write_archive_with(
    response: fetch::HttpResponse,
    destination: std::path::PathBuf,
    create: impl FnOnce(std::path::PathBuf) -> std::io::Result<(tempfile::NamedTempFile, File)> + Send + 'static,
) -> Result<Result<tempfile::NamedTempFile>> {
    let (archive_file, output) = tokio::task::spawn_blocking(move || -> std::io::Result<_> { create(destination) }).await??;
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

pub fn extract(input: impl Read, destination: &Path) -> Result<()> {
    let contents = tempfile::Builder::new().prefix(".unpack-").tempdir_in(destination)?;
    extract_contents(input, destination, contents.path())
}

fn extract_contents(input: impl Read, destination: &Path, contents: &Path) -> Result<()> {
    extract_contents_with(
        input,
        destination,
        contents,
        |source, target| std::fs::rename(source, target),
        |root| root.join("rust").is_dir(),
    )
}

pub fn extract_contents_with(
    input: impl Read,
    destination: &Path,
    contents: &Path,
    publish: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    has_rust: impl FnOnce(&Path) -> bool,
) -> Result<()> {
    unpack(input, contents)?;
    let root = database_root(contents)?;
    // Never publish an empty or malformed database as a successful refresh.
    let database = rustsec::database::Database::open(&root).into_app_err("parsing the advisory archive")?;
    if database.iter().next().is_none() {
        bail!("advisory archive contains no advisories; check --advisory-archive-url");
    }
    publish_database(&root, destination, publish, has_rust)
}

fn publish_database(
    root: &Path,
    destination: &Path,
    mut publish: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    has_rust: impl FnOnce(&Path) -> bool,
) -> Result<()> {
    publish(&root.join("crates"), &destination.join("crates"))?;
    if has_rust(root) {
        publish(&root.join("rust"), &destination.join("rust"))?;
    }
    Ok(())
}

fn unpack(input: impl Read, destination: &Path) -> Result<()> {
    unpack_with_limits(input, destination, MAX_UNPACKED_BYTES, MAX_ENTRIES)
}

/// Unpacks an untrusted gzip-compressed tar stream into a private directory.
///
/// Declared entry sizes are budgeted before any file is written, while the decoder
/// separately caps every decoded byte, including tar headers and padding.
pub fn unpack_with_limits(input: impl Read, destination: &Path, max_bytes: u64, max_entries: usize) -> Result<()> {
    let decoder = MultiGzDecoder::new(input).take(max_bytes + 1);
    let mut archive = Archive::new(decoder);
    let mut declared_bytes = 0_u64;
    for (index, entry) in
        // #[gamma::skip(try.propagate_to_unwrap, reason = "The archive was just constructed at position zero; entries can fail only after its reader has advanced.", tag = "equivalent")]
        archive.entries()?.enumerate()
    {
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
            // GitHub archives start with global PAX metadata, which is not a filesystem entry.
            continue;
        }
        if !kind.is_dir() && !kind.is_file() {
            bail!("advisory archives may contain only regular files and directories, not links or special entries");
        }
        let path = entry.path()?;
        let text =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "On Windows tar rejects non-Unicode paths before this conversion; the Unix conversion failure has a direct test.", tag = "platform")]
        path
            .to_str()
            .ok_or_else(|| ohno::app_err!("advisory archive paths must be UTF-8"))?;
        if text.contains(
            // #[gamma::skip(collection.reverse_array, reason = "This pattern is a character membership set; reversing its order cannot change matching.", tag = "equivalent")]
            ['\\', ':'],
        ) || path
            .components()
            .any(|part| matches!(part, Component::Prefix(_) | Component::RootDir))
        {
            bail!("advisory archive contains an unsafe path; use an archive with only relative paths");
        }
        if !entry.unpack_in(destination)? {
            bail!("advisory archive contains an unsafe path that escapes the extraction directory");
        }
    }
    // Tar traversal stops at the end-of-archive marker. Draining the rest validates every
    // remaining gzip member and trailer; the extra budget byte detects decoded overflow.
    let mut decoder = archive.into_inner();
    std::io::copy(&mut decoder, &mut std::io::sink())?;
    if decoder.limit() == 0 {
        bail!("advisory archive exceeds the {max_bytes}-byte uncompressed size limit");
    }
    Ok(())
}

pub fn database_root(contents: &Path) -> Result<std::path::PathBuf> {
    if contents.join("crates").is_dir() {
        return Ok(contents.to_path_buf());
    }
    database_root_from_entries(std::fs::read_dir(contents)?)
}

pub fn database_root_from_entries(roots: impl Iterator<Item = std::io::Result<std::fs::DirEntry>>) -> Result<std::path::PathBuf> {
    database_root_from_entries_with(roots, std::fs::DirEntry::file_type)
}

pub fn database_root_from_entries_with(
    mut roots: impl Iterator<Item = std::io::Result<std::fs::DirEntry>>,
    entry_type: impl FnOnce(&std::fs::DirEntry) -> std::io::Result<std::fs::FileType>,
) -> Result<std::path::PathBuf> {
    if let Some(root) = roots.next() {
        let root = root?;
        if roots.next().is_none() && entry_type(&root)?.is_dir() && root.path().join("crates").is_dir() {
            return Ok(root.path());
        }
    }
    bail!("advisory archive must contain crates/ at its root or under one enclosing directory; check --advisory-archive-url")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn database_root_returns_directory_iteration_errors_without_panicking() {
        let error = database_root_from_entries(std::iter::once(Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "scripted archive directory iteration failure",
        ))))
        .unwrap_err();
        let cause = error.find_source::<std::io::Error>().unwrap();
        assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(cause.to_string(), "scripted archive directory iteration failure");
    }

    #[test]
    fn survivor_optional_rust_publication_uses_the_exact_child_and_propagates_errors() {
        let root = Path::new("validated-database");
        let destination = Path::new("in-memory-extraction-destination");
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let error = publish_database(
            root,
            destination,
            {
                let calls = std::rc::Rc::clone(&calls);
                move |source, target| {
                    let child = match calls.get() {
                        0 => "crates",
                        1 => "rust",
                        _ => panic!("only the two database children may be published"),
                    };
                    assert_eq!(source, root.join(child));
                    assert_eq!(target, destination.join(child));
                    calls.set(calls.get() + 1);
                    if child == "rust" {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "scripted rust-directory publication probe",
                        ))
                    } else {
                        Ok(())
                    }
                }
            },
            |actual| {
                assert_eq!(actual, root);
                true
            },
        )
        .unwrap_err();
        assert_eq!(
            error.find_source::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(error.to_string().contains("scripted rust-directory publication probe"), "{error}");
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn survivor_empty_archives_enforce_the_exact_decoded_byte_budget_without_filesystem_writes() {
        let mut encoded = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoded.write_all(&[0; 1024]).unwrap();
        let encoded = encoded.finish().unwrap();
        let destination = Path::new("unused-extraction-destination");
        unpack_with_limits(encoded.as_slice(), destination, 1024, 0).unwrap();
        let error = unpack_with_limits(encoded.as_slice(), destination, 1023, 0)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "advisory archive exceeds the 1023-byte uncompressed size limit");
        unpack_with_limits(encoded.as_slice(), destination, 1025, 0).unwrap();
        let error = unpack_with_limits(&b"not gzip"[..], destination, 2048, 10).unwrap_err().to_string();
        assert!(!error.is_empty());
    }

    #[test]
    fn survivor_default_unpacked_budget_accepts_the_exact_number_of_decoded_bytes() {
        const MEMBER_BYTES: u64 = 64 * 1024;
        let mut encoded = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::copy(&mut std::io::repeat(0).take(MEMBER_BYTES), &mut encoded).unwrap();
        let member = encoded.finish().unwrap();
        assert_eq!(MAX_UNPACKED_BYTES % MEMBER_BYTES, 0);
        let encoded = member.repeat(usize::try_from(MAX_UNPACKED_BYTES / MEMBER_BYTES).unwrap());
        let destination = Path::new("unused-extraction-destination");
        unpack(encoded.as_slice(), destination).unwrap();
    }

    #[test]
    fn survivor_default_entry_budget_counts_global_metadata_without_creating_files() {
        const HEADERS_PER_MEMBER: usize = 1000;
        let mut header = tar::Header::new_gnu();
        header.set_path("metadata").unwrap();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::XGlobalHeader);
        header.set_cksum();
        let destination = Path::new("unused-extraction-destination");
        let compress = |bytes: &[u8]| {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut encoder, bytes).unwrap();
            encoder.finish().unwrap()
        };
        assert_eq!(MAX_ENTRIES % HEADERS_PER_MEMBER, 0);
        let block = compress(&header.as_bytes().repeat(HEADERS_PER_MEMBER));
        let extra_header = compress(header.as_bytes());
        let end = compress(&[0; 1024]);
        for entries in [MAX_ENTRIES, MAX_ENTRIES + 1] {
            let mut encoded = block.repeat(MAX_ENTRIES / HEADERS_PER_MEMBER);
            if entries > MAX_ENTRIES {
                encoded.extend_from_slice(&extra_header);
            }
            encoded.extend_from_slice(&end);
            let result = unpack(encoded.as_slice(), destination);
            if entries == MAX_ENTRIES {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().to_string(), "advisory archive exceeds the 100000-entry limit");
            }
        }
    }

    #[test]
    fn survivor_declared_payload_budget_is_cumulative_and_distinct_from_truncation() {
        let header = |size| {
            let mut header = tar::Header::new_gnu();
            header.set_path("metadata").unwrap();
            header.set_size(size);
            header.set_entry_type(tar::EntryType::XGlobalHeader);
            header.set_cksum();
            header
        };
        let destination = Path::new("unused-extraction-destination");
        let mut source = header(512).as_bytes().to_vec();
        source.extend_from_slice(&[0; 512]);
        source.extend_from_slice(header(2048).as_bytes());
        let mut encoded = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut encoded, &source).unwrap();
        let error = unpack_with_limits(encoded.finish().unwrap().as_slice(), destination, 2048, 10).unwrap_err();
        assert_eq!(error.to_string(), "advisory archive exceeds the 2048-byte uncompressed size limit");

        let mut encoded = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut encoded, header(1024).as_bytes()).unwrap();
        let error = unpack_with_limits(encoded.finish().unwrap().as_slice(), destination, 1024, 10).unwrap_err();
        assert!(!error.to_string().contains("uncompressed size limit"), "{error}");
        assert_eq!(error.to_string(), "unexpected EOF during skip");
    }

    #[test]
    fn survivor_invalid_utf8_archive_paths_return_errors_before_writing() {
        let mut header = tar::Header::new_gnu();
        header.set_path("entry").unwrap();
        header.as_mut_bytes()[0] = 0xff;
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        let compressor = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(compressor);
        builder.append(&header, std::io::empty()).unwrap();
        let encoded = builder.into_inner().unwrap().finish().unwrap();
        let destination = Path::new("unused-extraction-destination");
        let error = unpack(encoded.as_slice(), destination).unwrap_err();
        if cfg!(windows) {
            assert!(error.to_string().contains("only Unicode paths are supported on Windows"), "{error}");
        } else {
            assert_eq!(error.to_string(), "advisory archive paths must be UTF-8");
        }
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
}
