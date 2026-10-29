// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::fs::{self, File};
use std::io::{Read as _, Write as _};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cargo_aprz_lib::Result;
use cargo_aprz_lib::internals::facts::advisories::archive::{
    database_root, database_root_from_entries, database_root_from_entries_with, download, download_with_clock, download_with_io, extract,
    extract_archive, extract_contents_with, unpack_with_limits, write_archive, write_archive_with,
};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};

const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;

use cargo_aprz_lib::internals::facts::advisories::configure_request_resilience;

type ArchiveHandles = (tempfile::NamedTempFile, Option<File>);
type ArchiveFactory = Box<dyn FnOnce() -> std::io::Result<ArchiveHandles> + Send>;
type Extraction = Box<dyn FnOnce(&tempfile::NamedTempFile, &Path) -> Result<()> + Send>;

thread_local! {
    static ARCHIVE_HANDLES: std::cell::RefCell<Option<ArchiveFactory>> = const { std::cell::RefCell::new(None) };
    static EXTRACTION: std::cell::RefCell<Option<Extraction>> = const { std::cell::RefCell::new(None) };
}

fn take_archive_handles() -> Option<ArchiveFactory> {
    ARCHIVE_HANDLES.with_borrow_mut(Option::take)
}

fn take_extraction() -> Option<Extraction> {
    EXTRACTION.with_borrow_mut(Option::take)
}

async fn write_scripted_archive(response: fetch::HttpResponse, destination: PathBuf) -> Result<Result<tempfile::NamedTempFile>> {
    let factory = take_archive_handles().expect("scripted archive tests must install their handle factory before writing");
    write_archive_with(response, destination, move |_| {
        let (archive, output) = factory()?;
        let output = output.map_or_else(|| archive.reopen(), Ok)?;
        Ok((archive, output))
    })
    .await
}

async fn download_scripted_archive(
    destination: &Path,
    url: &str,
    timeout: Duration,
    client: fetch::HttpClient,
    clock: tick::Clock,
) -> Result<()> {
    let factory = Arc::new(std::sync::Mutex::new(take_archive_handles()));
    let extraction = take_extraction();
    download_with_io(
        destination,
        url,
        timeout,
        client,
        clock,
        move |response, destination| {
            let factory = factory
                .lock()
                .expect("scripted factory access only takes a value, never invokes callbacks")
                .take();
            let factory = factory.expect("scripted successful transfers consume exactly one installed archive factory");
            write_archive_with(response, destination, move |_| {
                let (archive, output) = factory()?;
                let output = output.map_or_else(|| archive.reopen(), Ok)?;
                Ok((archive, output))
            })
        },
        move |archive, destination| {
            extraction.map_or_else(|| extract_archive(archive, destination), |extract| extract(archive, destination))
        },
    )
    .await
}

/// The immutable manifest supplies a real read-only handle, while its missing
/// `TempPath` makes reopen/input-open/close fail without deleting the manifest.
/// An optional second handle lets streaming reach write/flush failure instead.
fn read_only_archive_handles(output: bool) -> ArchiveHandles {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories/survivor-no-created-archive");
    assert!(!missing.exists());
    let mut path = tempfile::TempPath::try_from_path(missing).expect("the fixture path is absolute and names a file");
    path.disable_cleanup(true);
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    if !output {
        // Deny write sharing so tempfile cannot reopen this read-only backing handle.
        options.share_mode((FILE_SHARE_READ | FILE_SHARE_DELETE).0);
    }
    (
        tempfile::NamedTempFile::from_parts(options.open(&fixture).expect("the checked-in manifest is readable"), path),
        output.then(|| File::open(&fixture).expect("the checked-in manifest is readable")),
    )
}

struct RestoreArchiveHandles;

impl Drop for RestoreArchiveHandles {
    fn drop(&mut self) {
        ARCHIVE_HANDLES.with_borrow_mut(|handles| *handles = None);
    }
}

fn inject_archive_handles(handles: ArchiveHandles) -> RestoreArchiveHandles {
    ARCHIVE_HANDLES.with_borrow_mut(|slot| {
        assert!(slot.is_none());
        *slot = Some(Box::new(move || Ok(handles)));
    });
    RestoreArchiveHandles
}

fn empty_gzip_archive() -> Vec<u8> {
    let mut compressor = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    compressor
        .write_all(&[0_u8; 1024])
        .expect("compressing into an in-memory vector cannot fail");
    compressor
        .finish()
        .expect("finishing compression into an in-memory vector cannot fail")
}
#[test]
fn survivor_database_root_preserves_native_entry_type_errors() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories/database/crates");
    let error = database_root_from_entries_with(fs::read_dir(&root).unwrap(), |entry| {
        assert_eq!(entry.path(), root.join("fixture-crate"));
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "native directory-entry type error probe",
        ))
    })
    .unwrap_err();
    assert_eq!(
        error.find_source::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(error.to_string().contains("native directory-entry type error probe"), "{error}");
}
#[test]
fn database_root_rejects_extra_entries_even_beside_a_valid_database() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories");
    let database_entry = || {
        std::fs::read_dir(&fixtures)
            .unwrap()
            .map(std::result::Result::unwrap)
            .find(|entry| entry.file_name() == "database")
            .unwrap()
    };
    assert_eq!(
        database_root_from_entries(std::iter::once(Ok(database_entry()))).unwrap(),
        fixtures.join("database")
    );
    let error = database_root_from_entries([Ok(database_entry()), Ok(database_entry())].into_iter()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "advisory archive must contain crates/ at its root or under one enclosing directory; check --advisory-archive-url"
    );
}
#[test]
fn survivor_crates_publication_returns_native_rename_errors_before_any_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories/database");
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let expected = std::rc::Rc::new(std::cell::Cell::new(None));
    let expected_source = root.join("crates");
    let expected_destination = destination.join("crates");
    let error = extract_contents_with(
        empty_gzip_archive().as_slice(),
        &destination,
        &root,
        {
            let expected = std::rc::Rc::clone(&expected);
            move |source, target| {
                assert_eq!(source, expected_source);
                assert_eq!(target, expected_destination);
                let error = fs::rename(source, target).unwrap_err();
                expected.set(Some((error.kind(), error.raw_os_error())));
                Err(error)
            }
        },
        |_| panic!("failed crates publication must not query rust directory"),
    )
    .unwrap_err();
    let native = error.find_source::<std::io::Error>().unwrap();
    assert_eq!(Some((native.kind(), native.raw_os_error())), expected.get());
}
#[tokio::test]
async fn survivor_archive_handle_worker_panics_return_join_errors() {
    let _restore = RestoreArchiveHandles;
    ARCHIVE_HANDLES.with_borrow_mut(|slot| {
        assert!(slot.is_none());
        *slot = Some(Box::new(|| panic!("native archive factory panic probe")));
    });
    let response = fetch::HttpResponseBuilder::new_fake().build().unwrap();
    let error = write_scripted_archive(response, PathBuf::from("unused-injected-destination"))
        .await
        .unwrap_err();
    assert!(error.find_source::<tokio::task::JoinError>().unwrap().is_panic());
    assert!(error.to_string().contains("native archive factory panic probe"), "{error}");
}
#[tokio::test]
async fn extraction_worker_returns_the_extractors_error_without_panicking() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            EXTRACTION.with_borrow_mut(|hook| *hook = None);
        }
    }
    let _restore = Restore;
    let _handles = inject_archive_handles(read_only_archive_handles(true));
    EXTRACTION.with_borrow_mut(|slot| {
        assert!(slot.is_none());
        *slot = Some(Box::new(|_, destination| {
            assert_eq!(destination, Path::new("unused-injected-destination"));
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "scripted archive extraction refusal").into())
        }));
    });
    let handler = fetch::fake::FakeHandler::from_fn(|_| fetch::HttpResponseBuilder::new_fake().build());
    let error = download_scripted_archive(
        Path::new("unused-injected-destination"),
        "https://example.test/empty-archive",
        Duration::from_secs(1),
        fetch::HttpClient::new_fake(handler),
        tick::Clock::new_frozen(),
    )
    .await
    .unwrap_err();
    assert!(error.find_source::<tokio::task::JoinError>().is_none());
    let source = error.find_source::<std::io::Error>().unwrap();
    assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(source.to_string(), "scripted archive extraction refusal");
}
#[tokio::test]
async fn survivor_extraction_worker_panics_and_native_close_errors_stay_distinct() {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            EXTRACTION.with_borrow_mut(|hook| *hook = None);
        }
    }
    let _restore = Restore;
    for panic_in_extraction in [true, false] {
        let _handles = inject_archive_handles(read_only_archive_handles(true));
        EXTRACTION.with_borrow_mut(|slot| {
            assert!(slot.is_none());
            *slot = Some(Box::new(move |archive, destination| {
                assert_eq!(destination, Path::new("unused-injected-destination"));
                assert!(!archive.path().exists());
                assert_eq!(
                    std::io::read_to_string(archive.as_file().try_clone()?)?,
                    include_str!("../Cargo.toml")
                );
                assert!(!panic_in_extraction, "native archive extraction panic probe");
                Ok(())
            }));
        });
        let handler = fetch::fake::FakeHandler::from_fn(|_| fetch::HttpResponseBuilder::new_fake().build());
        let error = download_scripted_archive(
            Path::new("unused-injected-destination"),
            "https://example.test/empty-archive",
            Duration::from_secs(1),
            fetch::HttpClient::new_fake(handler),
            tick::Clock::new_frozen(),
        )
        .await
        .unwrap_err();
        if panic_in_extraction {
            assert!(error.find_source::<tokio::task::JoinError>().unwrap().is_panic());
            assert!(error.to_string().contains("native archive extraction panic probe"), "{error}");
        } else {
            assert!(error.find_source::<tokio::task::JoinError>().is_none());
            assert_eq!(error.find_source::<std::io::Error>().unwrap().kind(), std::io::ErrorKind::NotFound);
        }
    }
}
#[tokio::test]
async fn survivor_native_archive_reopen_failure_preserves_its_io_cause() {
    let handles = read_only_archive_handles(false);
    let expected = handles.0.reopen().unwrap_err();
    let _injected = inject_archive_handles(handles);
    let response = fetch::HttpResponseBuilder::new_fake().build().unwrap();
    let error = write_scripted_archive(response, PathBuf::from("unused-injected-destination"))
        .await
        .unwrap_err();
    assert!(
        std::iter::successors(error.source(), |cause| cause.source())
            .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
            .any(|cause| cause.kind() == expected.kind() && cause.raw_os_error() == expected.raw_os_error()),
        "{error}"
    );
}
#[tokio::test]
async fn survivor_native_archive_write_and_flush_failures_have_distinct_stream_boundaries() {
    for chunks in [1, 2] {
        let _injected = inject_archive_handles(read_only_archive_handles(true));
        let exhausted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut remaining = chunks;
        // Polling and the final observation occur on this awaited task; the
        // flag records only exhaustion and publishes no other state.
        let stream = futures_util::stream::poll_fn({
            let exhausted = std::sync::Arc::clone(&exhausted);
            move |_| {
                if remaining == 0 {
                    exhausted.store(true, std::sync::atomic::Ordering::Relaxed);
                    std::task::Poll::Ready(None)
                } else {
                    remaining -= 1;
                    std::task::Poll::Ready(Some(Ok::<_, fetch::HttpError>(bytes::Bytes::from_static(b"x").into())))
                }
            }
        });
        let response = fetch::HttpResponseBuilder::new_fake().stream(stream).build().unwrap();
        let error = write_scripted_archive(response, PathBuf::from("unused-injected-destination"))
            .await
            .unwrap_err();
        assert_eq!(exhausted.load(std::sync::atomic::Ordering::Relaxed), chunks == 1);
        assert!(
            std::iter::successors(error.source(), |cause| cause.source()).any(|cause| cause.downcast_ref::<std::io::Error>().is_some()),
            "{error}"
        );
    }
}
#[tokio::test]
async fn survivor_download_reports_archive_input_open_errors_without_extracting_or_writing() {
    let _injected = inject_archive_handles(read_only_archive_handles(true));
    let handler = fetch::fake::FakeHandler::from_fn(|_| fetch::HttpResponseBuilder::new_fake().build());
    let error = download_scripted_archive(
        Path::new("unused-injected-destination"),
        "https://example.test/empty-archive",
        Duration::from_secs(1),
        fetch::HttpClient::new_fake(handler),
        tick::Clock::new_frozen(),
    )
    .await
    .unwrap_err();
    assert!(
        std::iter::successors(error.source(), |cause| cause.source())
            .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
            .any(|cause| cause.kind() == std::io::ErrorKind::NotFound),
        "{error}"
    );
}
#[tokio::test]
#[cfg(windows)]
async fn survivor_compressed_transfer_accepts_the_exact_budget_with_a_native_discard_sink() {
    let (archive, _) = read_only_archive_handles(true);
    let sink = fs::OpenOptions::new().read(true).write(true).open("NUL").unwrap();
    let _injected = inject_archive_handles((archive, Some(sink)));
    let chunk = bytes::Bytes::from(vec![0_u8; 1024 * 1024]);
    let stream = futures_util::stream::iter((0..64).map(move |_| Ok(chunk.clone().into())));
    let response = fetch::HttpResponseBuilder::new_fake().stream(stream).build().unwrap();
    let archive = write_scripted_archive(response, PathBuf::from("unused-injected-destination"))
        .await
        .unwrap()
        .unwrap();
    assert!(!archive.path().exists());
}
#[test]
fn survivor_extraction_directory_creation_errors_keep_the_private_stage_prefix() {
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let error = extract(std::io::empty(), &destination).unwrap_err();
    let diagnostic = format!("{error:?}");
    assert!(diagnostic.contains(".unpack-"), "{diagnostic}");
}
#[test]
fn survivor_database_root_rejects_single_non_database_and_multiple_enclosing_directories() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories");
    for path in [fixtures.join("database/crates"), fixtures.join("partial-write"), fixtures.clone()] {
        let error = database_root(&path).unwrap_err().to_string();
        assert_eq!(
            error,
            "advisory archive must contain crates/ at its root or under one enclosing directory; check --advisory-archive-url"
        );
    }
    let database = fixtures.join("database");
    assert_eq!(database_root(&database).unwrap(), database);
}
#[test]
fn survivor_unsafe_archive_paths_fail_before_any_output_is_created() {
    for name in [r"folder\file", "folder:file", "/absolute", "../escape"] {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(0);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        let mut encoded = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoded.write_all(header.as_bytes()).unwrap();
        let encoded = encoded.finish().unwrap();
        let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories/survivor-absent-output");
        assert!(!destination.exists());
        let error = unpack_with_limits(encoded.as_slice(), &destination, 2048, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsafe path"), "{name}: {error}");
        assert!(!destination.exists());
    }
}
#[test]
fn survivor_unpack_into_an_immutable_file_returns_an_error_without_creating_output() {
    let mut header = tar::Header::new_gnu();
    header.set_path("entry").unwrap();
    header.set_size(0);
    header.set_mode(0o644);
    header.set_cksum();
    let compressor = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(compressor);
    builder.append(&header, std::io::empty()).unwrap();
    let encoded = builder.into_inner().unwrap().finish().unwrap();
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let error = unpack_with_limits(encoded.as_slice(), &destination, MAX_UNPACKED_BYTES, MAX_ENTRIES).unwrap_err();
    assert!(error.to_string().contains("Cargo.toml"), "{error}");
}
// The fake-handler atomics only count calls. Tests read them after awaiting the
// download (or alongside polling it on the same task), so they publish no other memory;
// SeqCst is kept for simplicity rather than because any ordering is required.
#[tokio::test]
async fn survivor_archive_creation_errors_preserve_the_io_cause_without_creating_files() {
    let destination = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let missing_child_kind = fs::metadata(destination.join("uncreatable-entry")).unwrap_err().kind();
    let response = fetch::HttpResponseBuilder::new_fake().build().unwrap();
    let error = write_archive(response, destination.clone()).await.unwrap_err();
    let cause = std::iter::successors(error.source(), |error| error.source())
        .find_map(|error| error.downcast_ref::<std::io::Error>())
        .unwrap();
    assert_eq!(cause.kind(), missing_child_kind);
    let error = database_root(&destination).unwrap_err();
    let cause = std::iter::successors(error.source(), |error| error.source())
        .find_map(|error| error.downcast_ref::<std::io::Error>())
        .unwrap();
    assert_eq!(cause.kind(), fs::read_dir(&destination).unwrap_err().kind());
    let error = extract(std::io::empty(), &destination).unwrap_err();
    let cause = std::iter::successors(error.source(), |error| error.source())
        .find_map(|error| error.downcast_ref::<std::io::Error>())
        .unwrap();
    assert_eq!(cause.kind(), missing_child_kind);
}
fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("advisories")
        .join(name)
}
fn extract_fixture(name: &str) -> Result<rustsec::database::Database> {
    let stage = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
    extract(File::open(fixture(name))?, stage.path())?;

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
const MULTI_MEMBER: &[u8] = include_bytes!("fixtures/advisories/multi-member.tar.gz");
#[test]
fn reads_advisories_across_gzip_member_boundaries() {
    let mut first_member = Vec::new();
    flate2::read::GzDecoder::new(MULTI_MEMBER).read_to_end(&mut first_member).unwrap();
    let contains = |needle: &[u8]| first_member.windows(needle.len()).any(|window| window == needle);
    assert!(
        contains(b"RUSTSEC-2020-0001") && !contains(b"RUSTSEC-2020-0002"),
        "the fixture must place a gzip member boundary between its two advisory entries"
    );

    let database = extract_fixture("multi-member.tar.gz").expect("gzip members form one complete tar archive");

    let mut ids = database.iter().map(|advisory| advisory.metadata.id.to_string()).collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, ["RUSTSEC-2020-0001", "RUSTSEC-2020-0002"]);
}
#[test]
fn rejects_a_corrupt_later_gzip_member_before_publishing_database_files() {
    let stage = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

    extract(File::open(fixture("corrupt-later-member.tar.gz")).unwrap(), stage.path())
        .expect_err("a later member's checksum must be validated");

    assert!(
        !stage.path().join("crates").exists(),
        "the first member must not be published alone"
    );
}
#[test]
fn uncompressed_size_limit_covers_all_gzip_members() {
    let stage = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

    unpack_with_limits(MULTI_MEMBER, stage.path(), 2048, MAX_ENTRIES).expect_err("the combined tar entries and padding exceed the budget");
}
#[test]
fn accepts_the_global_pax_metadata_used_by_github_archives() {
    let database = extract_fixture("github-pax.tar.gz").expect("GitHub metadata is not a filesystem entry");

    assert_eq!(database.iter().count(), 1);
}
#[test]
fn preserves_the_optional_rust_advisory_collection() {
    let stage = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

    extract(File::open(fixture("with-rust.tar.gz")).unwrap(), stage.path()).expect("an optional rust/ collection is supported");

    assert!(stage.path().join("rust").is_dir());
}
#[tokio::test]
async fn rejects_oversized_content_length_before_reading_the_body() {
    let calls = Arc::new(AtomicUsize::new(0));
    let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_fn({
        let calls = Arc::clone(&calls);
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            fetch::HttpResponseBuilder::new_fake()
                .status(fetch::StatusCode::OK)
                .header("content-length", "67108865")
                .build()
        }
    }));
    let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

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
        let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_fn({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                fetch::HttpResponseBuilder::new_fake().status(status).build()
            }
        }));
        let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

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
    let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_error_fn({
        let calls = Arc::clone(&calls);
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            fetch::HttpError::from(std::io::Error::other("request resilience exhausted"))
        }
    }));
    let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

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
        let handler = fetch::fake::FakeHandler::from_fn({
            let calls = Arc::clone(&calls);
            move |_| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    fetch::HttpResponseBuilder::new_fake()
                        .status(status)
                        .header("retry-after", "120")
                        .build()
                } else {
                    fetch::HttpResponseBuilder::new_fake()
                        .bytes(bytes::Bytes::from_static(include_bytes!("fixtures/advisories/root.tar.gz")))
                        .build()
                }
            }
        });
        let client = configure_request_resilience(fetch::HttpClient::builder_fake(handler, &clock)).build();
        let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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
    let client = fetch::HttpClient::new_fake(fetch::fake::FakeHandler::from_fn({
        let calls = Arc::clone(&calls);
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            fetch::HttpResponseBuilder::new_fake()
                .stream(futures_util::stream::iter(
                    (0..65).map(|_| Ok(bytes::Bytes::from(vec![0_u8; 1024 * 1024]).into())),
                ))
                .build()
        }
    }));
    let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();

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
    let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let input = include_bytes!("fixtures/advisories/wrapped.tar.gz");

    let error = unpack_with_limits(input.as_slice(), destination.path(), MAX_UNPACKED_BYTES, 0)
        .expect_err("a zero-entry budget cannot admit a file");

    assert!(error.to_string().contains("0-entry limit"), "{error}");
}
#[test]
fn includes_tar_padding_in_the_uncompressed_size_limit() {
    let destination = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let input = include_bytes!("fixtures/advisories/wrapped.tar.gz");

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
#[test]
fn rejects_malformed_advisories() {
    let error = extract_fixture("invalid-database.tar.gz").expect_err("invalid archive must not be accepted");

    let cause = error
        .find_source::<rustsec::Error>()
        .unwrap_or_else(|| panic!("the RustSec parser must reject the advisory: {error:#}"));
    assert_eq!(cause.kind(), rustsec::ErrorKind::Parse, "{error:#}");
}
rejected_archive!(rejects_missing_database, "missing-crates.tar.gz", "must contain crates/");
rejected_archive!(rejects_empty_database, "empty-database.tar.gz", "contains no advisories");
rejected_archive!(rejects_parent_paths, "parent-path.tar.gz", "unsafe path");
rejected_archive!(rejects_absolute_paths, "absolute-path.tar.gz", "unsafe path");
rejected_archive!(rejects_windows_traversal_on_every_platform, "backslash-path.tar.gz", "unsafe path");
rejected_archive!(rejects_symbolic_links, "symlink.tar.gz", "not links");
rejected_archive!(rejects_hard_links, "hardlink.tar.gz", "not links");
rejected_archive!(rejects_oversized_entries, "oversized.tar.gz", "uncompressed size limit");
rejected_archive!(checks_the_gzip_trailer, "truncated.tar.gz", "unexpected end");
