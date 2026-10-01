// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::sync::atomic::Ordering;
use core::time::Duration;
use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{BufRead, Error as IoError, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
#[cfg(not(miri))]
use std::sync::Mutex;
use std::time::Instant;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use flate2::bufread::GzDecoder;
use futures_util::{Stream, StreamExt};
use ohno::{EnrichableExt, IntoAppError, bail};
use tar::Archive;
use tokio::sync::mpsc;
use url::Url;

use super::{
    CategoriesTable, CrateDownloadsTable, CrateOwnersTable, CratesCategoriesTable, CratesKeywordsTable, CratesTable, DependenciesTable,
    KeywordsTable, Table, TeamsTable, UsersTable, VersionDownloadsTable, VersionsTable,
};
use crate::facts::progress::Progress;
use crate::{HashMap, Result};

/// Log target for crates tables
const LOG_TARGET: &str = "    crates";

/// Generates the `TableMgr` struct and associated methods from a list of table field definitions.
///
/// Creates:
/// - `TableMgr` struct with fields for each table (wrapped in `Arc`)
/// - Accessor methods for each table (e.g., `crates_table()`, `versions_table()`)
/// - `open_tables_from_scratch()` - Opens all tables from disk
/// - `open_tables_from_files()` - Opens tables from already-open file handles
/// - `delete_all_tables()` - Removes all table files from disk
///
/// Also generates the helper function `process_csv_entry()` used during download.
///
/// See the macro invocation below (lines 189-211) for usage.
macro_rules! define_tables {
    ($(
        $(#[$meta:meta])*
        $field:ident: $type:ty
    ),* $(,)?) => {
        /// Manager for downloading and accessing all crates.io database tables.
        #[derive(Debug)]
        pub struct TableMgr {
            $(
                $(#[$meta])*
                $field: Arc<$type>,
            )*
        }

        impl TableMgr {
            $(
                $(#[$meta])*
                #[must_use]
                pub fn $field(&self) -> &$type {
                    &self.$field
                }
            )*

            fn open_tables_from_scratch(
                tables_root: impl AsRef<Path>,
                max_ttl: Duration,
                now: DateTime<Utc>,
                progress: &dyn Progress,
            ) -> Result<Self> {
                const NUM_TABLES: u64 = count_tables!($($field)*);

                let finished_tables = Arc::new(core::sync::atomic::AtomicU64::new(0));
                let finished_tables_clone = Arc::clone(&finished_tables);
                progress.set_determinate(Box::new(move || {
                    (NUM_TABLES, finished_tables_clone.load(Ordering::Relaxed), "FOpening tables".to_string())
                }));

                $(
                    $(#[$meta])*
                    let table_start = Instant::now();
                    $(#[$meta])*
                    log::debug!(target: LOG_TARGET, "Opening table '{}'", <$type>::TABLE_NAME);

                    $(#[$meta])*
                    let table = <$type>::open(&tables_root, max_ttl, now)
                        .into_app_err(concat!("opening ", stringify!($field), " table"))?;
                    $(#[$meta])*
                    let $field = Arc::new(table);

                    $(#[$meta])*
                    {
                        log::debug!(target: LOG_TARGET, "Finished opening table '{}' in {:.3}s", <$type>::TABLE_NAME, table_start.elapsed().as_secs_f64());
                        let _ = finished_tables.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    }
                )*

                Ok(Self {
                    $(
                        $(#[$meta])*
                        $field,
                    )*
                })
            }

            fn open_tables_from_files(
                files: HashMap<&'static str, File>,
                max_ttl: Duration,
                now: DateTime<Utc>,
                progress: &dyn Progress,
            ) -> Result<Self> {
                const NUM_TABLES: u64 = count_tables!($($field)*);

                let finished_tables = Arc::new(core::sync::atomic::AtomicU64::new(0));
                let finished_tables_clone = Arc::clone(&finished_tables);
                progress.set_determinate(Box::new(move || {
                    (NUM_TABLES, finished_tables_clone.load(Ordering::Relaxed), "Opening tables".to_string())
                }));

                $(
                    $(#[$meta])*
                    let table_start = Instant::now();
                    $(#[$meta])*
                    log::debug!(target: LOG_TARGET, "Opening table '{}'", <$type>::TABLE_NAME);

                    $(#[$meta])*
                    let file = files.get(<$type>::TABLE_NAME)
                        .into_app_err_with(|| format!("missing file for table {}", <$type>::TABLE_NAME))?;

                    $(#[$meta])*
                    let mmap_start = Instant::now();

                    $(#[$meta])*
                    // Get file size for the anonymous snapshot.
                    let metadata = file.metadata()
                        .into_app_err_with(|| format!("getting metadata for {}", <$type>::TABLE_NAME))?;
                    $(#[$meta])*
                    #[expect(clippy::cast_possible_truncation, reason = "Table files won't exceed usize::MAX on any supported platform")]
                    let file_size = metadata.len() as usize;

                    $(#[$meta])*
                    let mmap = super::map_table_file(file, file_size)
                        .into_app_err_with(|| format!("loading {}", <$type>::TABLE_NAME))?;

                    $(#[$meta])*
                    log::debug!(target: LOG_TARGET, "Finished loading snapshot '{}' in {:.3}s", <$type>::TABLE_NAME, mmap_start.elapsed().as_secs_f64());

                    $(#[$meta])*
                    let open_start = Instant::now();
                    $(#[$meta])*
                    let table = <$type>::open_with(mmap, max_ttl, now)
                        .into_app_err(concat!("opening ", stringify!($field), " table"))?;
                    $(#[$meta])*
                    log::debug!(target: LOG_TARGET, "Finished validating {} in {:.3}s", <$type>::TABLE_NAME, open_start.elapsed().as_secs_f64());

                    $(#[$meta])*
                    let $field = Arc::new(table);

                    $(#[$meta])*
                    {
                        log::debug!(target: LOG_TARGET, "Finished opening '{}' in {:.3}s", <$type>::TABLE_NAME, table_start.elapsed().as_secs_f64());
                        let _ = finished_tables.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    }
                )*

                Ok(Self {
                    $(
                        $(#[$meta])*
                        $field,
                    )*
                })
            }
        }

        /// Delete all known table files from the tables directory.
        /// Returns false if any file failed to delete because it was still locked.
        /// Returns an error for any other deletion failure.
        fn delete_all_tables(tables_root: impl AsRef<Path>) -> Result<bool> {
            let tables_root = tables_root.as_ref();
            let mut any_locked = false;

            $(
                $(#[$meta])*
                let table_path = tables_root.join(<$type>::TABLE_NAME);
                $(#[$meta])*
                if table_path.exists() {
                    // Non-short-circuiting on purpose: every table is attempted even once one is locked.
                    any_locked |= removal_left_file_locked(fs::remove_file(&table_path), &table_path)?;
                }
            )*

            Ok(!any_locked)
        }

        fn process_csv_entry(
            filename: &str,
            entry: &mut tar::Entry<impl Read>,
            tables_root: &Path,
            now: DateTime<Utc>,
        ) -> Result<Option<(&'static str, File)>> {
            match filename {
                $(
                    $(#[$meta])*
                    <$type>::CSV_NAME => {
                        log::info!(target: LOG_TARGET, "Processing CSV file '{}' from database", <$type>::CSV_NAME);
                        let file = <$type>::create_table(tables_root, entry, now)?;
                        Ok(Some((<$type>::TABLE_NAME, file)))
                    }
                )*
                _ => Ok(None),
            }
        }
    };
}

macro_rules! count_tables {
    () => (0);
    ($head:ident $($tail:ident)*) => (1 + count_tables!($($tail)*));
}

/// Windows raw OS error 32: "the process cannot access the file because it is being used by
/// another process".
const SHARING_VIOLATION: i32 = 32;

/// Reports whether removing a table file left it behind because something else still has it open.
///
/// Windows surfaces a table file still held by this or another process as [`SHARING_VIOLATION`],
/// which the caller retries rather than treats as fatal; every other failure is a genuine error.
/// This is deliberately not `#[cfg(windows)]`: `remove_file` cannot produce raw OS error 32 on
/// Unix, so the classification is correct everywhere while staying testable on every platform.
fn removal_left_file_locked(result: core::result::Result<(), IoError>, table_path: &Path) -> Result<bool> {
    match result {
        Ok(()) => Ok(false),
        Err(e) if e.raw_os_error() == Some(SHARING_VIOLATION) => Ok(true),
        Err(e) => Err(e).into_app_err_with(|| format!("removing {}", table_path.display())),
    }
}

define_tables! {
    crates_table: CratesTable,
    versions_table: VersionsTable,
    version_downloads_table: VersionDownloadsTable,
    dependencies_table: DependenciesTable,
    crate_downloads_table: CrateDownloadsTable,
    crates_categories_table: CratesCategoriesTable,
    crates_keywords_table: CratesKeywordsTable,
    categories_table: CategoriesTable,
    keywords_table: KeywordsTable,
    teams_table: TeamsTable,
    users_table: UsersTable,
    crate_owners_table: CrateOwnersTable,
}

impl TableMgr {
    pub async fn new(
        source: &Url,
        tables_root: impl AsRef<Path>,
        max_ttl: Duration,
        now: DateTime<Utc>,
        ignore_cached: bool,
        progress: Arc<dyn Progress>,
    ) -> Result<Self> {
        let tables_root = tables_root.as_ref();

        if !ignore_cached {
            log::info!("Opening the crates database");
            let result = Self::open_tables_from_scratch(tables_root, max_ttl, now, progress.as_ref());

            if let Ok(ref table_mgr) = result {
                log::debug!(
                    target: LOG_TARGET,
                    "successfully opened cached crates.io tables from {} (created at {})",
                    tables_root.display(),
                    table_mgr.created_at()
                );
                return result;
            }
        }

        log::info!(target: LOG_TARGET, "Cached crates database not found or out of date, downloading a fresh copy");

        if let Err(e) = Self::cleanup_tables(tables_root) {
            log::debug!(
                target: LOG_TARGET,
                "unable to cleanup stale table files from {}, continuing anyway: {}",
                tables_root.display(),
                e
            );
        }

        match prep_tables(source, tables_root, max_ttl, now, progress).await {
            Ok(table_mgr) => Ok(table_mgr),
            Err(e) => Err(e.enrich("could not prepare crates.io tables")),
        }
    }

    #[must_use]
    pub fn created_at(&self) -> DateTime<Utc> {
        self.crates_table.timestamp()
    }

    /// Deletes every table file, retrying while any of them is still locked.
    ///
    /// Coverage is turned off because a real filesystem only ever reports a table file as still
    /// locked on Windows; on every other platform `delete_all_tables` either succeeds outright or
    /// returns the error, so the retry loop cannot be reached from a test running there.
    /// `delete_all_tables` itself stays instrumented.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn cleanup_tables(tables_root: impl AsRef<Path>) -> Result<()> {
        const MAX_WAIT_MS: u64 = 4000;
        const INITIAL_DELAY_MS: u64 = 100;
        const MAX_DELAY_MS: u64 = 1000;

        let tables_root = tables_root.as_ref();

        // Another process or an outstanding file handle can transiently keep a Windows table file
        // locked. Retry with exponential backoff for up to four seconds.

        let start = Instant::now();
        let mut delay_ms = INITIAL_DELAY_MS;

        loop {
            if cleanup_delete_all_tables(tables_root)? {
                return Ok(());
            }

            let elapsed_ms = cleanup_elapsed_ms(start);

            // If we've already waited MAX_WAIT_MS, give up
            // #[gamma::skip(cond.always_false, relational.ge_to_gt, tag = "timeout", reason = "the exact deadline check is what terminates retries when locked files never clear")]
            if elapsed_ms >= MAX_WAIT_MS {
                return Err(ohno::app_err!(
                    "unable to remove all table files in {}: some files remain locked after {}ms of retrying",
                    tables_root.display(),
                    elapsed_ms,
                ));
            }

            // Calculate how long to sleep (don't exceed MAX_WAIT_MS total)
            let remaining_ms = MAX_WAIT_MS - elapsed_ms;
            let sleep_ms = delay_ms.min(remaining_ms);

            #[expect(
                clippy::cast_precision_loss,
                reason = "sleep_ms is capped at 1000ms, well within f64 precision range"
            )]
            let sleep_seconds = sleep_ms as f64 / 1000.0;
            #[cfg(test)]
            #[cfg(not(miri))]
            record_retry_sleep_seconds(sleep_seconds);

            log::debug!(
                target: LOG_TARGET,
                "unable to delete all table files in {}, retrying in {} seconds",
                tables_root.display(),
                sleep_seconds
            );

            cleanup_sleep(Duration::from_millis(sleep_ms));

            // Exponential backoff for next iteration, capped at MAX_DELAY_MS
            delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
        }
    }
}

fn cleanup_delete_all_tables(tables_root: &Path) -> Result<bool> {
    #[cfg(test)]
    #[cfg(not(miri))]
    {
        if let Some(hook) = &*DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") {
            return hook(tables_root);
        }
    }

    delete_all_tables(tables_root)
}

fn cleanup_elapsed_ms(start: Instant) -> u64 {
    #[cfg(test)]
    #[cfg(not(miri))]
    {
        if let Some(hook) = &*ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") {
            return hook();
        }
    }

    // #[gamma::skip(expr.increment, reason = "the real monotonic clock is only observable at scheduler-dependent millisecond precision; deterministic deadline tests use the injected elapsed-time hook above")]
    // #[gamma::skip(expr.decrement, reason = "u128-to-u64 overflow would require this process to run for over 584 million years")]
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn cleanup_sleep(duration: Duration) {
    #[cfg(test)]
    #[cfg(not(miri))]
    {
        if let Some(hook) = &*SLEEP_HOOK.lock().expect("cleanup sleep hook mutex is not poisoned") {
            hook(duration);
            return;
        }
    }

    // #[gamma::skip(stmt.delete_call, reason = "wall-clock sleep duration is scheduler-dependent and the deterministic injected sleep hook above verifies every requested backoff duration")]
    std::thread::sleep(duration);
}

#[cfg(test)]
#[cfg(not(miri))]
fn record_retry_sleep_seconds(seconds: f64) {
    if let Some(hook) = &*SLEEP_SECONDS_HOOK.lock().expect("cleanup sleep seconds hook mutex is not poisoned") {
        hook(seconds);
    }
}

#[cfg(test)]
#[cfg(not(miri))]
type DeleteAllTablesHook = dyn Fn(&Path) -> Result<bool> + Send + Sync;
#[cfg(test)]
#[cfg(not(miri))]
type ElapsedMsHook = dyn Fn() -> u64 + Send + Sync;
#[cfg(test)]
#[cfg(not(miri))]
type SleepHook = dyn Fn(Duration) + Send + Sync;
#[cfg(test)]
#[cfg(not(miri))]
type SleepSecondsHook = dyn Fn(f64) + Send + Sync;

#[cfg(test)]
#[cfg(not(miri))]
static DELETE_ALL_TABLES_HOOK: Mutex<Option<Box<DeleteAllTablesHook>>> = Mutex::new(None);
#[cfg(test)]
#[cfg(not(miri))]
static ELAPSED_MS_HOOK: Mutex<Option<Box<ElapsedMsHook>>> = Mutex::new(None);
#[cfg(test)]
#[cfg(not(miri))]
static SLEEP_HOOK: Mutex<Option<Box<SleepHook>>> = Mutex::new(None);
#[cfg(test)]
#[cfg(not(miri))]
static SLEEP_SECONDS_HOOK: Mutex<Option<Box<SleepSecondsHook>>> = Mutex::new(None);

// As we get data off the socket, we transfer the chunks over to the thread responsible for decompression and saving to disk.
// There can be up to NUM_CHANNEL_BUFFERS chunks "in flight" at any given time. If we can't keep up writing to disk,
// the channel will fill up, which will eventually cause the network to stop pumping data until there is space in the channel.
const NUM_CHANNEL_BUFFERS: usize = 64;
const CRATES_DB_DOWNLOAD_OPERATION: &str = "crates_db_download";
const CRATES_DB_DOWNLOAD_TIMEOUT: Option<Duration> = Some(Duration::from_mins(30));

fn determinate_download_progress(total: u64, downloaded_bytes: u64) -> (u64, u64, String) {
    let downloaded_mb = downloaded_bytes / (1024 * 1024);
    let total_mb = total / (1024 * 1024);
    let message = format!("{downloaded_mb}/{total_mb} MB: Downloading crates database");
    (total, downloaded_bytes, message)
}

fn indeterminate_download_progress(downloaded_bytes: u64) -> String {
    let downloaded_mb = downloaded_bytes / (1024 * 1024);
    format!("{downloaded_mb} MB: Downloading crates database")
}

async fn prep_tables(
    source: &Url,
    tables_root: impl AsRef<Path>,
    max_ttl: Duration,
    now: DateTime<Utc>,
    progress: Arc<dyn Progress>,
) -> Result<TableMgr> {
    let tables_root = tables_root.as_ref().to_path_buf();
    let source = source.clone();

    crate::facts::resilient_http::resilient_download(
        CRATES_DB_DOWNLOAD_OPERATION,
        (source, tables_root, max_ttl, now, progress),
        CRATES_DB_DOWNLOAD_TIMEOUT,
        move |(source, tables_root, max_ttl, now, progress)| async move {
            prep_tables_core(&source, tables_root, max_ttl, now, progress).await
        },
    )
    .await
}

async fn prep_tables_core(
    source: &Url,
    tables_root: std::path::PathBuf,
    max_ttl: Duration,
    now: DateTime<Utc>,
    progress: Arc<dyn Progress>,
) -> Result<TableMgr> {
    log::info!(target: LOG_TARGET, "Starting crates database download from {source}");

    let client = reqwest::Client::builder()
        .user_agent("cargo-aprz")
        .build()
        .into_app_err("creating HTTP client")?;

    let response = download_response(&client, source)
        .await
        .into_app_err("requesting crates database dump")?;

    if !response.status().is_success() {
        bail!("unable to download crates database dump: HTTP {}", response.status());
    }

    let content_length = response.content_length();

    // Set up progress callback for download
    // #[gamma::skip(literal.int_increment, reason = "the initial byte count is transient telemetry sampled concurrently with network I/O; exact transferred and final totals are tested through deterministic seams")]
    let downloaded_bytes = Arc::new(core::sync::atomic::AtomicU64::new(0));
    let downloaded_bytes_clone = Arc::clone(&downloaded_bytes);

    set_download_progress(progress.as_ref(), content_length, downloaded_bytes_clone);

    let (tx, rx) = download_channel();
    let processing_progress = Arc::clone(&progress);
    let processing_handle =
        tokio::task::spawn_blocking(move || process_download(rx, &tables_root, max_ttl, now, processing_progress.as_ref()));
    stream_download(response, &tx, &downloaded_bytes, content_length).await;

    drop(tx);
    let table_mgr = join_processing_task(processing_handle).await?;

    Ok(table_mgr)
}

async fn download_response(client: &reqwest::Client, source: &Url) -> Result<reqwest::Response> {
    crate::facts::resilient_http::resilient_get(client, source.as_str())
        .await
        .into_app_err("starting crates database dump download")
}

fn download_channel() -> (mpsc::Sender<Result<Bytes>>, mpsc::Receiver<Result<Bytes>>) {
    mpsc::channel(NUM_CHANNEL_BUFFERS)
}

fn finish_download(downloaded_bytes: &core::sync::atomic::AtomicU64, content_length: Option<u64>) {
    if let Some(total) = content_length {
        downloaded_bytes.store(total, Ordering::Relaxed);
    }
}

fn set_download_progress(progress: &dyn Progress, content_length: Option<u64>, downloaded_bytes: Arc<core::sync::atomic::AtomicU64>) {
    if let Some(total) = content_length {
        progress.set_determinate(Box::new(move || {
            let downloaded_bytes = downloaded_bytes.load(Ordering::Relaxed);
            determinate_download_progress(total, downloaded_bytes)
        }));
    } else {
        progress.set_indeterminate(Box::new(move || {
            let downloaded_bytes = downloaded_bytes.load(Ordering::Relaxed);
            indeterminate_download_progress(downloaded_bytes)
        }));
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
async fn stream_download(
    response: reqwest::Response,
    tx: &mpsc::Sender<Result<Bytes>>,
    downloaded_bytes: &core::sync::atomic::AtomicU64,
    content_length: Option<u64>,
) {
    let stream = response.bytes_stream().map(|chunk| chunk.map_err(Into::into));
    forward_download_stream(stream, tx, downloaded_bytes, content_length).await;
}

async fn forward_download_stream(
    mut stream: impl Stream<Item = Result<Bytes>> + Unpin,
    tx: &mpsc::Sender<Result<Bytes>>,
    downloaded_bytes: &core::sync::atomic::AtomicU64,
    content_length: Option<u64>,
) {
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let _ = downloaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                if !send_download_chunk(tx, bytes).await {
                    return;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error)).await;
                return;
            }
        }
    }
    finish_download(downloaded_bytes, content_length);
}

async fn join_processing_task(handle: tokio::task::JoinHandle<Result<TableMgr>>) -> Result<TableMgr> {
    handle.await.into_app_err("joining crates database processing task")?
}

async fn send_download_chunk(tx: &mpsc::Sender<Result<Bytes>>, bytes: Bytes) -> bool {
    tx.send(Ok(bytes)).await.is_ok()
}

fn process_download(
    rx: mpsc::Receiver<Result<Bytes>>,
    tables_root: &Path,
    max_ttl: Duration,
    now: DateTime<Utc>,
    progress: &dyn Progress,
) -> Result<TableMgr> {
    log::info!(target: LOG_TARGET, "Processing crates database download");
    let reader = ChannelReader::new(rx);
    let decoder = GzDecoder::new(reader);
    let mut archive = Archive::new(decoder);

    let mut files = HashMap::default();
    // #[gamma::skip(try.propagate_to_unwrap, literal.str_to_empty, literal.str_to_xyzzy, tag = "unreachable", reason = "tar::Archive::entries only configures iteration and does not inspect input; malformed archive errors are produced by the returned iterator instead")]
    let entries = archive.entries().into_app_err("reading crates database archive")?;
    for entry in entries {
        let mut entry = entry?;
        let path = archive_entry_path(entry.path())?;
        let filename = archive_filename(&path).unwrap_or_default();
        let start = Instant::now();
        if let Some((table_name, file)) = process_csv_entry(filename, &mut entry, tables_root, now)? {
            let _ = files.insert(table_name, file);
            log::info!(
                target: LOG_TARGET,
                "Finished processing CSV file '{}' in {:.3}s",
                filename,
                start.elapsed().as_secs_f64()
            );
        }
    }

    let table_mgr = TableMgr::open_tables_from_files(files, max_ttl, now, progress)?;

    Ok(table_mgr)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn archive_filename(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn archive_entry_path(path: std::io::Result<Cow<'_, Path>>) -> Result<PathBuf> {
    Ok(path.into_app_err("reading crates database archive entry path")?.into_owned())
}

struct ChannelReader {
    rx: mpsc::Receiver<Result<Bytes>>,
    current_chunk: Option<Bytes>,
    position: usize,
}

impl ChannelReader {
    const fn new(rx: mpsc::Receiver<Result<Bytes>>) -> Self {
        Self {
            rx,
            current_chunk: None,
            // #[gamma::skip(assign_value.default, reason = "usize::default() is exactly zero, so the replacement is behavior-identical")]
            position: 0,
        }
    }
}

impl BufRead for ChannelReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.current_chunk.as_ref().is_none_or(|chunk| self.position >= chunk.len()) {
            match self.rx.blocking_recv() {
                Some(Ok(chunk)) => {
                    self.current_chunk = Some(chunk);
                    // #[gamma::skip(assign_value.default, reason = "usize::default() is exactly zero, so the replacement is behavior-identical")]
                    self.position = 0;
                }
                Some(Err(e)) => return Err(IoError::other(e.to_string())),
                None => return Ok(&[]),
            }
        }

        Ok(&self.current_chunk.as_ref().expect("guaranteed by while condition")[self.position..])
    }

    fn consume(&mut self, amount: usize) {
        self.position += amount;
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let data = self.fill_buf()?;
        let to_copy = data.len().min(buf.len());
        buf[..to_copy].copy_from_slice(&data[..to_copy]);
        self.consume(to_copy);
        Ok(to_copy)
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        CRATES_DB_DOWNLOAD_OPERATION, CRATES_DB_DOWNLOAD_TIMEOUT, ChannelReader, CratesTable, DELETE_ALL_TABLES_HOOK, ELAPSED_MS_HOOK,
        IoError, Path, SHARING_VIOLATION, SLEEP_HOOK, SLEEP_SECONDS_HOOK, Table, TableMgr, archive_entry_path, delete_all_tables,
        determinate_download_progress, download_channel, download_response, finish_download, forward_download_stream, fs,
        indeterminate_download_progress, join_processing_task, prep_tables_core, process_download, removal_left_file_locked,
        send_download_chunk, set_download_progress,
    };
    use crate::facts::progress::Progress;

    static CLEANUP_HOOK_TEST_LOCK: StdMutex<()> = StdMutex::new(());
    type DeterminateCallback = Box<dyn Fn() -> (u64, u64, String) + Send + Sync>;

    struct HookGuard;

    #[derive(Default)]
    struct RecordingProgress {
        determinate: StdMutex<Option<DeterminateCallback>>,
        indeterminate: StdMutex<Option<Box<dyn Fn() -> String + Send + Sync>>>,
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    impl Progress for RecordingProgress {
        fn set_phase(&self, _phase: &str) {}

        fn set_determinate(&self, callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {
            *self.determinate.lock().expect("determinate callback mutex is not poisoned") = Some(callback);
        }

        fn set_indeterminate(&self, callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {
            *self.indeterminate.lock().expect("indeterminate callback mutex is not poisoned") = Some(callback);
        }

        fn println(&self, _msg: &str) {}

        fn done(&self) {}
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            *DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") = None;
            *ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") = None;
            *SLEEP_HOOK.lock().expect("cleanup sleep hook mutex is not poisoned") = None;
            *SLEEP_SECONDS_HOOK.lock().expect("cleanup sleep seconds hook mutex is not poisoned") = None;
        }
    }

    fn install_cleanup_hooks() -> HookGuard {
        HookGuard
    }

    fn compressed_tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in files {
            let mut archive_header = tar::Header::new_gnu();
            archive_header.set_size(contents.len() as u64);
            archive_header.set_mode(0o644);
            archive_header.set_cksum();
            builder
                .append_data(&mut archive_header, *name, *contents)
                .expect("writing an in-memory tar entry");
        }
        builder
            .into_inner()
            .expect("finishing the in-memory tar")
            .finish()
            .expect("finishing the gzip stream")
    }

    fn process_bytes(bytes: Vec<u8>, tables_root: &Path) -> crate::Result<TableMgr> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.blocking_send(Ok(bytes::Bytes::from(bytes))).expect("the test receiver is open");
        drop(tx);
        process_download(
            rx,
            tables_root,
            Duration::from_hours(1),
            chrono::DateTime::UNIX_EPOCH,
            &RecordingProgress::default(),
        )
    }

    #[test]
    fn a_successful_removal_leaves_nothing_locked() {
        let locked = removal_left_file_locked(Ok(()), Path::new("crates.table")).expect("a successful removal is not an error");

        assert!(!locked, "a file that was removed cannot still be locked");
    }

    #[test]
    fn a_sharing_violation_is_reported_as_a_locked_file() {
        let result = Err(IoError::from_raw_os_error(SHARING_VIOLATION));

        let locked =
            removal_left_file_locked(result, Path::new("crates.table")).expect("a locked file is retried, not turned into an error");

        assert!(locked, "raw OS error 32 means another process still holds the file open");
    }

    #[test]
    fn any_other_removal_failure_is_an_error() {
        let result = Err(IoError::new(ErrorKind::PermissionDenied, "nope"));

        let e = removal_left_file_locked(result, Path::new("crates.table")).expect_err("a permission failure is fatal");

        assert!(
            format!("{e}").contains("removing crates.table"),
            "the error should name the file: {e}"
        );
    }

    #[test]
    fn an_unlocked_table_file_is_deleted() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let table_path = dir.path().join(CratesTable::TABLE_NAME);
        fs::write(&table_path, b"not a real table").expect("writing the placeholder table file");

        let deleted_everything = delete_all_tables(dir.path()).expect("deleting an unlocked table file");

        assert!(deleted_everything, "nothing was locked, so every table file should be gone");
        assert!(!table_path.exists(), "the table file should have been deleted");
    }

    /// Windows refuses to delete a file while a handle is open without `FILE_SHARE_DELETE`, which
    /// is the only way a real filesystem can drive `delete_all_tables` down the locked-file path.
    #[cfg(windows)]
    #[test]
    fn a_locked_table_file_is_reported_instead_of_failing_the_delete() {
        use std::fs::OpenOptions;
        use std::os::windows::fs::OpenOptionsExt;

        /// `FILE_SHARE_READ`, deliberately without `FILE_SHARE_DELETE`.
        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let dir = TempDir::new().expect("creating a temporary directory");
        let table_path = dir.path().join(CratesTable::TABLE_NAME);
        fs::write(&table_path, b"not a real table").expect("writing the placeholder table file");

        let locked = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&table_path)
            .expect("opening the table file that was just written");

        let deleted_everything = delete_all_tables(dir.path()).expect("a locked file is reported, not turned into an error");

        assert!(!deleted_everything, "a locked table file must be reported as such");
        assert!(table_path.exists(), "the locked table file must still be on disk");

        drop(locked);
    }

    #[test]
    fn cleanup_tables_propagates_delete_errors() {
        let _lock = CLEANUP_HOOK_TEST_LOCK.lock().expect("cleanup hook test mutex is not poisoned");
        let _guard = install_cleanup_hooks();
        *DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") =
            Some(Box::new(|_| Err(ohno::app_err!("synthetic delete failure"))));

        let error = TableMgr::cleanup_tables(Path::new("unused")).expect_err("delete failures are fatal");

        assert!(format!("{error:#}").contains("synthetic delete failure"));
    }

    #[test]
    fn cleanup_tables_retries_locked_files_with_exponential_backoff() {
        let _lock = CLEANUP_HOOK_TEST_LOCK.lock().expect("cleanup hook test mutex is not poisoned");
        let _guard = install_cleanup_hooks();
        let outcomes = Arc::new(StdMutex::new(vec![false, false, true]));
        let sleep_durations = Arc::new(StdMutex::new(Vec::new()));

        {
            let outcomes = Arc::clone(&outcomes);
            *DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") = Some(Box::new(move |_| {
                let mut outcomes = outcomes.lock().expect("outcome mutex is not poisoned");
                Ok(outcomes.remove(0))
            }));
        }

        let elapsed_ms = Arc::new(StdMutex::new(vec![0, 0, 4_000]));
        {
            let elapsed_ms = Arc::clone(&elapsed_ms);
            *ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") = Some(Box::new(move || {
                let mut elapsed_ms = elapsed_ms.lock().expect("elapsed mutex is not poisoned");
                elapsed_ms.remove(0)
            }));
        }
        {
            let sleep_durations = Arc::clone(&sleep_durations);
            *SLEEP_HOOK.lock().expect("cleanup sleep hook mutex is not poisoned") = Some(Box::new(move |duration| {
                sleep_durations.lock().expect("sleep mutex is not poisoned").push(duration);
            }));
        }

        TableMgr::cleanup_tables(Path::new("unused")).expect("locked files eventually clear");

        assert_eq!(
            *sleep_durations.lock().expect("sleep mutex is not poisoned"),
            vec![Duration::from_millis(100), Duration::from_millis(200)]
        );
    }

    #[test]
    fn cleanup_tables_caps_exponential_backoff_at_one_second() {
        let _lock = CLEANUP_HOOK_TEST_LOCK.lock().expect("cleanup hook test mutex is not poisoned");
        let _guard = install_cleanup_hooks();
        let outcomes = Arc::new(StdMutex::new(vec![false, false, false, false, false, false, true]));
        let sleep_durations = Arc::new(StdMutex::new(Vec::new()));

        {
            let outcomes = Arc::clone(&outcomes);
            *DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") = Some(Box::new(move |_| {
                Ok(outcomes.lock().expect("outcome mutex is not poisoned").remove(0))
            }));
        }
        *ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") = Some(Box::new(|| 0));
        {
            let sleep_durations = Arc::clone(&sleep_durations);
            *SLEEP_HOOK.lock().expect("cleanup sleep hook mutex is not poisoned") = Some(Box::new(move |duration| {
                sleep_durations.lock().expect("sleep mutex is not poisoned").push(duration);
            }));
        }

        TableMgr::cleanup_tables(Path::new("unused")).expect("locked files eventually clear");

        assert_eq!(
            *sleep_durations.lock().expect("sleep mutex is not poisoned"),
            [100, 200, 400, 800, 1000, 1000].map(Duration::from_millis)
        );
    }

    #[test]
    fn cleanup_tables_caps_sleep_at_remaining_wait_time() {
        let _lock = CLEANUP_HOOK_TEST_LOCK.lock().expect("cleanup hook test mutex is not poisoned");
        let _guard = install_cleanup_hooks();
        let outcomes = Arc::new(StdMutex::new(vec![false, true]));
        let sleep_durations = Arc::new(StdMutex::new(Vec::new()));
        let sleep_seconds = Arc::new(StdMutex::new(Vec::new()));

        {
            let outcomes = Arc::clone(&outcomes);
            *DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") = Some(Box::new(move |_| {
                let mut outcomes = outcomes.lock().expect("outcome mutex is not poisoned");
                Ok(outcomes.remove(0))
            }));
        }
        let elapsed_ms = Arc::new(StdMutex::new(vec![3_990, 4_000]));
        {
            let elapsed_ms = Arc::clone(&elapsed_ms);
            *ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") = Some(Box::new(move || {
                let mut elapsed_ms = elapsed_ms.lock().expect("elapsed mutex is not poisoned");
                elapsed_ms.remove(0)
            }));
        }
        {
            let sleep_durations = Arc::clone(&sleep_durations);
            *SLEEP_HOOK.lock().expect("cleanup sleep hook mutex is not poisoned") = Some(Box::new(move |duration| {
                sleep_durations.lock().expect("sleep mutex is not poisoned").push(duration);
            }));
        }
        {
            let sleep_seconds = Arc::clone(&sleep_seconds);
            *SLEEP_SECONDS_HOOK.lock().expect("cleanup sleep seconds hook mutex is not poisoned") = Some(Box::new(move |seconds| {
                sleep_seconds.lock().expect("sleep seconds mutex is not poisoned").push(seconds);
            }));
        }

        TableMgr::cleanup_tables(Path::new("unused")).expect("locked files clear after capped sleep");

        assert_eq!(
            *sleep_durations.lock().expect("sleep mutex is not poisoned"),
            vec![Duration::from_millis(10)]
        );
        assert_eq!(*sleep_seconds.lock().expect("sleep seconds mutex is not poisoned"), vec![0.01]);
    }

    #[test]
    fn cleanup_tables_times_out_at_the_exact_limit() {
        let _lock = CLEANUP_HOOK_TEST_LOCK.lock().expect("cleanup hook test mutex is not poisoned");
        let _guard = install_cleanup_hooks();
        *DELETE_ALL_TABLES_HOOK.lock().expect("cleanup delete hook mutex is not poisoned") = Some(Box::new(|_| Ok(false)));
        *ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") = Some(Box::new(|| 4_000));

        let error =
            TableMgr::cleanup_tables(Path::new("locked-tables")).expect_err("the cleanup budget is exhausted at exactly four seconds");

        let message = format!("{error:#}");
        assert!(message.contains("locked-tables"));
        assert!(message.contains("4000ms"));
    }

    #[test]
    fn cleanup_timing_helpers_use_their_real_fallbacks_without_hooks() {
        let _lock = CLEANUP_HOOK_TEST_LOCK.lock().expect("cleanup hook test mutex is not poisoned");
        let _guard = install_cleanup_hooks();

        assert!(super::cleanup_elapsed_ms(std::time::Instant::now()) <= 100);
        super::cleanup_sleep(Duration::ZERO);
        super::record_retry_sleep_seconds(0.0);
    }

    #[test]
    fn download_progress_messages_report_mebibytes() {
        let mib = 1024 * 1024;

        assert_eq!(
            determinate_download_progress(5 * mib, 3 * mib),
            (5 * mib, 3 * mib, "3/5 MB: Downloading crates database".to_owned())
        );
        assert_eq!(
            indeterminate_download_progress(7 * mib),
            "7 MB: Downloading crates database".to_owned()
        );
        assert_eq!(
            indeterminate_download_progress(mib - 1),
            "0 MB: Downloading crates database".to_owned()
        );
        assert_eq!(
            determinate_download_progress(mib - 1, mib - 1),
            (mib - 1, mib - 1, "0/0 MB: Downloading crates database".to_owned())
        );
    }

    #[test]
    fn download_retry_policy_is_stable() {
        assert_eq!(CRATES_DB_DOWNLOAD_OPERATION, "crates_db_download");
        assert_eq!(CRATES_DB_DOWNLOAD_TIMEOUT, Some(Duration::from_mins(30)));
    }

    #[test]
    fn known_and_unknown_lengths_install_the_matching_progress_callback() {
        let progress = RecordingProgress::default();
        let downloaded = Arc::new(AtomicU64::new(17));
        set_download_progress(&progress, Some(100), Arc::clone(&downloaded));

        let determinate = progress
            .determinate
            .lock()
            .expect("determinate callback mutex is not poisoned")
            .take()
            .expect("known lengths install a determinate callback");
        assert_eq!(determinate(), (100, 17, "0/0 MB: Downloading crates database".to_owned()));
        assert!(
            progress
                .indeterminate
                .lock()
                .expect("indeterminate callback mutex is not poisoned")
                .is_none()
        );

        downloaded.store(2 * 1024 * 1024, Ordering::Relaxed);
        let progress = RecordingProgress::default();
        set_download_progress(&progress, None, downloaded);
        let indeterminate = progress
            .indeterminate
            .lock()
            .expect("indeterminate callback mutex is not poisoned")
            .take()
            .expect("unknown lengths install an indeterminate callback");
        assert_eq!(indeterminate(), "2 MB: Downloading crates database");
        assert!(
            progress
                .determinate
                .lock()
                .expect("determinate callback mutex is not poisoned")
                .is_none()
        );
    }

    #[tokio::test]
    async fn sending_to_a_closed_download_channel_reports_failure() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);

        assert!(!send_download_chunk(&tx, bytes::Bytes::from_static(b"chunk")).await);
    }

    #[tokio::test]
    async fn streaming_a_download_forwards_every_byte_and_tracks_the_exact_total() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bytes"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"abcdef"))
            .expect(1)
            .mount(&server)
            .await;
        let response = reqwest::get(format!("{}/bytes", server.uri())).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let downloaded = AtomicU64::new(0);

        super::stream_download(response, &tx, &downloaded, None).await;
        drop(tx);
        let mut received = Vec::new();
        while let Some(chunk) = rx.recv().await {
            received.extend_from_slice(&chunk.expect("the mock body is successful"));
        }

        assert_eq!(received, b"abcdef");
        assert_eq!(downloaded.load(Ordering::Relaxed), 6);
    }

    #[tokio::test]
    async fn deterministic_stream_forwards_multiple_chunks_and_stops_after_a_closed_receiver() {
        let chunks: [crate::Result<bytes::Bytes>; 2] = [Ok(bytes::Bytes::from_static(b"ab")), Ok(bytes::Bytes::from_static(b"cde"))];
        let chunks = futures_util::stream::iter(chunks);
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let downloaded = AtomicU64::new(0);
        forward_download_stream(chunks, &tx, &downloaded, Some(99)).await;
        drop(tx);

        assert_eq!(rx.recv().await.unwrap().unwrap(), bytes::Bytes::from_static(b"ab"));
        assert_eq!(rx.recv().await.unwrap().unwrap(), bytes::Bytes::from_static(b"cde"));
        assert_eq!(downloaded.load(Ordering::Relaxed), 99);

        let chunks: [crate::Result<bytes::Bytes>; 2] = [Ok(bytes::Bytes::from_static(b"first")), Ok(bytes::Bytes::from_static(b"second"))];
        let chunks = futures_util::stream::iter(chunks);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let downloaded = AtomicU64::new(0);
        forward_download_stream(chunks, &tx, &downloaded, None).await;
        assert_eq!(downloaded.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn download_channel_has_the_documented_capacity() {
        let (tx, _rx) = download_channel();
        assert_eq!(tx.capacity(), 64);
    }

    #[test]
    fn finishing_a_known_length_download_sets_the_exact_total() {
        let downloaded = AtomicU64::new(3);
        finish_download(&downloaded, Some(12));
        assert_eq!(downloaded.load(Ordering::Relaxed), 12);

        finish_download(&downloaded, None);
        assert_eq!(downloaded.load(Ordering::Relaxed), 12);
    }

    #[test]
    fn channel_reader_crosses_chunk_boundaries_without_overreading() {
        use std::io::Read as _;

        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.blocking_send(Ok(bytes::Bytes::from_static(b"ab"))).unwrap();
        tx.blocking_send(Ok(bytes::Bytes::from_static(b"cd"))).unwrap();
        drop(tx);
        let mut reader = ChannelReader::new(rx);
        let mut output = [0u8; 4];

        assert_eq!(reader.read(&mut output[..2]).unwrap(), 2);
        assert_eq!(reader.read(&mut output[2..]).unwrap(), 2);
        assert_eq!(output, *b"abcd");
        assert_eq!(reader.read(&mut output).unwrap(), 0);
    }

    #[test]
    fn channel_reader_returns_download_errors() {
        use std::io::Read as _;

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.blocking_send(Err(ohno::app_err!("synthetic download failure"))).unwrap();
        drop(tx);
        let mut reader = ChannelReader::new(rx);

        let error = reader.read(&mut [0u8; 1]).expect_err("channel errors must be returned");
        assert!(error.to_string().contains("synthetic download failure"));
    }

    #[test]
    fn processing_an_empty_archive_returns_missing_table_errors() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let error = process_bytes(compressed_tar(&[]), dir.path()).expect_err("an empty archive has no required tables");

        assert!(format!("{error:#}").contains("missing file for table"));
    }

    #[test]
    fn csv_processing_errors_are_returned_without_panicking() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let missing_root = dir.path().join("missing");
        let archive = compressed_tar(&[("snapshot/data/crates.csv", b"id,name\n1,demo\n")]);

        let error = process_bytes(archive, &missing_root).expect_err("table creation cannot use a missing root");

        assert!(format!("{error:#}").contains("creating table file"));
    }

    #[test]
    fn malformed_archive_entries_are_returned_without_panicking() {
        use std::io::Write as _;

        use flate2::Compression;
        use flate2::write::GzEncoder;

        let mut corrupt_tar = vec![0u8; 1024];
        corrupt_tar[0..4].copy_from_slice(b"name");
        corrupt_tar[124..135].copy_from_slice(b"00000000001");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&corrupt_tar).expect("compressing malformed tar bytes");
        let bytes = encoder.finish().expect("finishing malformed gzip stream");
        let dir = TempDir::new().expect("creating a temporary directory");

        process_bytes(bytes, dir.path()).expect_err("the malformed tar entry must be rejected");
    }

    #[test]
    fn archive_entry_path_errors_have_exact_context() {
        let error =
            archive_entry_path(Err(IoError::other("synthetic archive path failure"))).expect_err("archive path errors must be returned");
        assert!(format!("{error:#}").contains("reading crates database archive entry path"));
    }

    #[tokio::test]
    async fn processing_task_panics_are_returned_with_exact_context() {
        let handle: tokio::task::JoinHandle<crate::Result<TableMgr>> = tokio::spawn(async { panic!("synthetic processing panic") });
        let error = join_processing_task(handle)
            .await
            .expect_err("a panicking processing task must be returned as an error");
        assert!(format!("{error:#}").contains("joining crates database processing task"));
    }

    #[tokio::test]
    async fn download_transport_errors_are_returned_with_context() {
        let client = reqwest::Client::new();
        let source = url::Url::parse("http://127.0.0.1:0/db-dump.tar.gz").expect("fixed URL is valid");

        let error = download_response(&client, &source)
            .await
            .expect_err("port zero cannot accept an HTTP connection");

        assert!(format!("{error:#}").contains("starting crates database dump download"));
    }

    #[tokio::test]
    async fn table_preparation_adds_request_context_to_transport_errors() {
        let source = url::Url::parse("http://127.0.0.1:0/db-dump.tar.gz").expect("fixed URL is valid");
        let dir = TempDir::new().expect("creating a temporary directory");
        let error = prep_tables_core(
            &source,
            dir.path().to_path_buf(),
            Duration::from_hours(1),
            chrono::DateTime::UNIX_EPOCH,
            Arc::new(RecordingProgress::default()),
        )
        .await
        .expect_err("port zero cannot accept an HTTP connection");

        assert!(format!("{error:#}").contains("requesting crates database dump"));
    }

    #[tokio::test]
    async fn database_download_uses_the_cargo_aprz_user_agent() {
        let server = MockServer::start().await;
        let body = compressed_tar(&[]);
        Mock::given(method("GET"))
            .and(path("/db-dump.tar.gz"))
            .and(header("user-agent", "cargo-aprz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let source = url::Url::parse(&format!("{}/db-dump.tar.gz", server.uri())).expect("mock URL is valid");
        let dir = TempDir::new().expect("creating a temporary directory");
        let progress = Arc::new(RecordingProgress::default());
        prep_tables_core(
            &source,
            dir.path().to_path_buf(),
            Duration::from_hours(1),
            chrono::DateTime::UNIX_EPOCH,
            Arc::clone(&progress) as Arc<dyn Progress>,
        )
        .await
        .expect_err("the empty archive is missing required tables");

        let callback = progress
            .determinate
            .lock()
            .expect("determinate callback mutex is not poisoned")
            .take()
            .expect("the preparation path installs download progress");
        let (total, downloaded, _) = callback();
        assert!(total > 0);
        assert_eq!(downloaded, 0);
    }

    #[tokio::test]
    async fn table_manager_enriches_download_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/db-dump.tar.gz"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let source = url::Url::parse(&format!("{}/db-dump.tar.gz", server.uri())).unwrap();
        let dir = TempDir::new().unwrap();

        let error = TableMgr::new(
            &source,
            dir.path(),
            Duration::from_hours(1),
            chrono::DateTime::UNIX_EPOCH,
            true,
            Arc::new(RecordingProgress::default()),
        )
        .await
        .expect_err("HTTP 500 cannot prepare the tables");

        assert!(format!("{error:#}").contains("could not prepare crates.io tables"));
    }
}
