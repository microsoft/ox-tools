// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::sync::atomic::Ordering;
use core::time::Duration;
use std::fs::{self, File};
use std::io::{BufRead, Error as IoError, Read};
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
#[cfg(not(miri))]
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use flate2::bufread::GzDecoder;
use futures_util::StreamExt;
use mmap_rs::{MmapFlags, MmapOptions};
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
                    // Get file size for mapping
                    let metadata = file.metadata()
                        .into_app_err_with(|| format!("getting metadata for {}", <$type>::TABLE_NAME))?;
                    $(#[$meta])*
                    #[expect(clippy::cast_possible_truncation, reason = "Table files won't exceed usize::MAX on any supported platform")]
                    let file_size = metadata.len() as usize;

                    $(#[$meta])*
                    // SAFETY: We have read-only access to the file for the duration of the mmap.
                    // The file is controlled by this application and won't be modified externally.
                    let mmap = unsafe {
                        MmapOptions::new(file_size)
                            .into_app_err_with(|| format!("creating mmap options for {}", <$type>::TABLE_NAME))?
                            .with_flags(MmapFlags::TRANSPARENT_HUGE_PAGES | MmapFlags::SEQUENTIAL)
                            .with_file(file, 0)
                            .map()
                            .into_app_err_with(|| format!("memory-mapping {}", <$type>::TABLE_NAME))?
                    };

                    $(#[$meta])*
                    log::debug!(target: LOG_TARGET, "Finished mapping '{}' in {:.3}s", <$type>::TABLE_NAME, mmap_start.elapsed().as_secs_f64());

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
/// Windows surfaces a memory-mapped table file that the kernel has not finished releasing as
/// [`SHARING_VIOLATION`], which the caller retries rather than treats as fatal; every other
/// failure is a genuine error. This is deliberately not `#[cfg(windows)]`: `remove_file` cannot
/// produce raw OS error 32 on Unix, so the classification is correct everywhere while staying
/// testable on every platform.
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

        // On Windows, memory-mapped files might not be immediately released after dropping.
        // This is a documented OS limitation where kernel cleanup is asynchronous.
        // Retry with exponential backoff up to 4 seconds total wait time.

        let start = Instant::now();
        let mut delay_ms = INITIAL_DELAY_MS;

        loop {
            if cleanup_delete_all_tables(tables_root)? {
                return Ok(());
            }

            let elapsed_ms = cleanup_elapsed_ms(start);

            // If we've already waited MAX_WAIT_MS, give up
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

#[expect(
    clippy::cast_possible_truncation,
    reason = "Elapsed time won't exceed u64::MAX in practice (would require ~584 million years)"
)]
fn cleanup_elapsed_ms(start: Instant) -> u64 {
    #[cfg(test)]
    #[cfg(not(miri))]
    {
        if let Some(hook) = &*ELAPSED_MS_HOOK.lock().expect("cleanup elapsed hook mutex is not poisoned") {
            return hook();
        }
    }

    start.elapsed().as_millis() as u64
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

    thread::sleep(duration);
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
        "crates_db_download",
        (source, tables_root, max_ttl, now, progress),
        Some(Duration::from_mins(30)),
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

    let response = crate::facts::resilient_http::resilient_get(&client, source.as_str())
        .await
        .into_app_err("starting crates database dump download")?;

    if !response.status().is_success() {
        bail!("unable to download crates database dump: HTTP {}", response.status());
    }

    let content_length = response.content_length();

    // Set up progress callback for download
    let downloaded_bytes = Arc::new(core::sync::atomic::AtomicU64::new(0));
    let downloaded_bytes_clone = Arc::clone(&downloaded_bytes);

    if let Some(total) = content_length {
        // Determinate: we know the total size
        progress.set_determinate(Box::new(move || {
            let downloaded_bytes = downloaded_bytes_clone.load(Ordering::Relaxed);
            determinate_download_progress(total, downloaded_bytes)
        }));
    } else {
        // Indeterminate: we don't know the total size
        progress.set_indeterminate(Box::new(move || {
            let downloaded_bytes = downloaded_bytes_clone.load(Ordering::Relaxed);
            indeterminate_download_progress(downloaded_bytes)
        }));
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes>>(NUM_CHANNEL_BUFFERS);
    let processing_progress = Arc::clone(&progress);
    let processing_handle =
        tokio::task::spawn_blocking(move || process_download(rx, &tables_root, max_ttl, now, processing_progress.as_ref()));
    stream_download(response, &tx, &downloaded_bytes).await;

    if let Some(total) = content_length {
        downloaded_bytes.store(total, Ordering::Relaxed);
    }

    drop(tx);
    let table_mgr = processing_handle.await??;

    Ok(table_mgr)
}

#[cfg_attr(coverage_nightly, coverage(off))]
async fn stream_download(response: reqwest::Response, tx: &mpsc::Sender<Result<Bytes>>, downloaded_bytes: &core::sync::atomic::AtomicU64) {
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let _ = downloaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                if tx.send(Ok(bytes)).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error.into())).await;
                break;
            }
        }
    }
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
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

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
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{
        CratesTable, DELETE_ALL_TABLES_HOOK, ELAPSED_MS_HOOK, IoError, Path, SHARING_VIOLATION, SLEEP_HOOK, SLEEP_SECONDS_HOOK, Table,
        TableMgr, delete_all_tables, determinate_download_progress, fs, indeterminate_download_progress, removal_left_file_locked,
    };

    static CLEANUP_HOOK_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    struct HookGuard;

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
    }
}
