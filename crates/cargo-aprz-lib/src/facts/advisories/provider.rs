// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::path::{Component, Path};
use std::sync::Arc;

use compact_str::CompactString;
use ohno::IntoAppError;
use rustsec::database::Database;
use tick::FutureExt;
#[cfg(any(not(test), miri))]
use tokio::fs::OpenOptions as CacheLockOptions;

#[cfg(all(test, not(miri)))]
use self::tests::CacheLockOptions;
#[cfg(all(test, not(miri)))]
use self::tests::SnapshotStage;
use super::AdvisoryData;
#[cfg(any(not(test), miri))]
type SnapshotStage = tempfile::TempDir;
use crate::facts::ProviderResult;
use crate::facts::cache::{Cache, CacheResult};
use crate::facts::crate_spec::CrateSpec;
use crate::facts::progress::Progress;
use crate::{HashMap, Result};

mod publication;

use publication::{SnapshotIo, publish_snapshot};

/// Log target for advisories provider
const LOG_TARGET: &str = "advisories";

#[derive(Debug)]
pub struct Provider {
    database: Arc<Database>,
}

const DATABASE_FETCH_TIMEOUT: Duration = Duration::from_mins(1);

/// How an advisory database is installed into a new staging directory.
trait DbFetcher: Send + Sync + 'static {
    fn fetch(&self, repo_path: &Path, database_url: &str) -> impl Future<Output = Result<()>> + Send;
}

struct ArchiveFetcher;

impl DbFetcher for ArchiveFetcher {
    async fn fetch(&self, repo_path: &Path, database_url: &str) -> Result<()> {
        super::archive::download(
            repo_path,
            database_url,
            DATABASE_FETCH_TIMEOUT,
            crate::facts::resilient_http::new_client(),
        )
        .await
    }
}

impl Provider {
    pub async fn new(cache: &Cache, progress: Arc<dyn Progress>, database_url: &str) -> Result<Self> {
        Self::with_fetcher(cache, progress, database_url, Arc::new(ArchiveFetcher)).await
    }

    async fn with_fetcher<F: DbFetcher>(cache: &Cache, progress: Arc<dyn Progress>, database_url: &str, fetcher: Arc<F>) -> Result<Self> {
        let lock = Arc::new(lock_cache(cache.dir()).await?);
        if let CacheResult::Data((identity, directory)) = snapshot_marker(cache)
            && identity == archive_identity(database_url)
        {
            let path = snapshot_path(cache.dir(), &directory)?;
            return Ok(Self {
                database: Arc::new(open_db(path, progress.as_ref(), lock).await?),
            });
        }

        let root = cache.dir().to_path_buf();
        #[cfg(all(test, not(miri)))]
        let stage_host = tests::NativeScope::capture();
        let stage = tokio::task::spawn_blocking(move || {
            #[cfg(all(test, not(miri)))]
            let _scope = tests::NativeScope::install(stage_host);
            create_snapshot_stage(root)
        })
        .await??;
        download_db(stage.path(), database_url, progress.as_ref(), fetcher)
            .await
            .into_app_err(
                "downloading the advisory archive; --advisory-archive-url must point to a .tar.gz archive, not a Git repository",
            )?;
        let database = open_db(stage.path(), progress.as_ref(), Arc::clone(&lock))
            .await
            .into_app_err("validating the downloaded advisory database")?;
        let cache = cache.clone();
        let database_url = database_url.to_owned();
        #[cfg(all(test, not(miri)))]
        let publication_host = tests::NativeScope::capture();
        tokio::task::spawn_blocking(move || {
            #[cfg(all(test, not(miri)))]
            let _scope = tests::NativeScope::install(publication_host);
            with_cache_lock(lock, || publish_snapshot(SnapshotIo::new(cache, stage), &database_url))
        })
        .await??;
        Ok(Self {
            database: Arc::new(database),
        })
    }

    pub async fn get_advisory_data(&self, crates: Arc<[CrateSpec]>) -> impl Iterator<Item = (CrateSpec, ProviderResult<AdvisoryData>)> {
        let database = Arc::clone(&self.database);

        tokio::task::spawn_blocking(move || scan_advisories(&database, crates.iter().cloned()).collect::<Vec<_>>().into_iter())
            .await
            .expect("tasks must not panic")
    }
}

fn scan_advisories<I>(database: &Database, crates: I) -> impl Iterator<Item = (CrateSpec, ProviderResult<AdvisoryData>)> + use<I>
where
    I: IntoIterator<Item = CrateSpec>,
{
    let start_time = std::time::Instant::now();

    let mut crate_map: HashMap<CompactString, Vec<(CrateSpec, AdvisoryData)>> = HashMap::default();

    for crate_spec in crates {
        crate_map
            .entry(crate_spec.name().into())
            .or_default()
            .push((crate_spec, AdvisoryData::default()));
    }

    let crate_count = crate_map.len();
    let mut advisories_checked = 0;
    let mut advisories_matched = 0;

    log::info!(target: LOG_TARGET, "Querying the advisory database for {crate_count} crate(s)");

    for advisory in database.iter() {
        // Withdrawn advisories were retracted by RustSec and no longer describe a real
        // problem, so counting them would penalize crates that have since been cleared.
        if advisory.metadata.withdrawn.is_some() {
            continue;
        }

        advisories_checked += 1;

        if let Some(crate_entries) = crate_map.get_mut(advisory.metadata.package.as_str()) {
            for (crate_spec, data) in crate_entries.iter_mut() {
                advisories_matched += 1;

                data.count_advisory_historical(advisory);
                if advisory.versions.is_vulnerable(crate_spec.version()) {
                    data.count_advisory_for_version(advisory);
                }
            }
        }
    }

    log::debug!(
        target: LOG_TARGET,
        "Completed scan of advisory database: checked {} advisories, found {} matches for {} crates in {:.3}s",
        advisories_checked,
        advisories_matched,
        crate_count,
        start_time.elapsed().as_secs_f64()
    );

    crate_map
        .into_values()
        .flatten()
        .map(|(crate_spec, data)| (crate_spec, ProviderResult::Found(data)))
}

async fn open_db(cache_dir: impl AsRef<Path>, progress: &dyn Progress, lock: Arc<std::fs::File>) -> Result<Database> {
    let cache_path = cache_dir.as_ref().to_path_buf();

    run_blocking_with_progress(progress, "Opening the advisory database", "opening", move || {
        with_cache_lock(lock, || Database::open(&cache_path).map_err(Into::into))
    })
    .await
    .into_app_err("opening the advisory database")
}

fn with_cache_lock<L, T>(lock: Arc<L>, operation: impl FnOnce() -> T) -> T {
    // Blocking work outlives a cancelled initializer, so it must own its lock guard.
    let _lock = lock;
    operation()
}

async fn download_db<F: DbFetcher>(
    cache_dir: impl AsRef<Path>,
    database_url: &str,
    progress: &dyn Progress,
    fetcher: Arc<F>,
) -> Result<()> {
    let msg = "Downloading the advisory database";
    log::info!(target: LOG_TARGET, "{msg}");
    progress.set_indeterminate(Box::new(move || msg.to_owned()));
    let start = std::time::Instant::now();
    fetcher.fetch(cache_dir.as_ref(), database_url).await?;
    log::debug!(target: LOG_TARGET, "Finished downloading the advisory database in {:.3}s", start.elapsed().as_secs_f64());
    Ok(())
}

async fn lock_cache(root: &Path) -> Result<std::fs::File> {
    let lock = open_cache_lock(root).await?;
    wait_for_cache_lock(&lock, DATABASE_FETCH_TIMEOUT, &tick::Clock::new_tokio()).await?;
    Ok(lock)
}

async fn open_cache_lock(root: &Path) -> Result<std::fs::File> {
    tokio::fs::create_dir_all(root)
        .await
        .into_app_err("creating directory for the advisory cache")?;
    Ok(CacheLockOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("archive.lock"))
        .await?
        .into_std()
        .await)
}

async fn wait_for_cache_lock(lock: &std::fs::File, timeout: Duration, clock: &tick::Clock) -> Result<()> {
    wait_for_cache_lock_with(timeout, clock, || try_cache_lock(lock)).await
}

async fn wait_for_cache_lock_with(
    timeout: Duration,
    clock: &tick::Clock,
    mut try_lock: impl FnMut() -> std::result::Result<(), std::fs::TryLockError>,
) -> Result<()> {
    async {
        loop {
            match try_lock() {
                Ok(()) => return Ok(()),
                Err(std::fs::TryLockError::WouldBlock) => clock.delay(Duration::from_millis(50)).await,
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
        }
    }
    .timeout(clock, timeout)
    .await
    .into_app_err("timed out waiting for another advisory cache refresh to finish")??;
    Ok(())
}

fn try_cache_lock(lock: &std::fs::File) -> std::result::Result<(), std::fs::TryLockError> {
    #[cfg(all(test, not(miri)))]
    if let Some(error) = tests::LOCK_ATTEMPT_ERROR.take() {
        return Err(std::fs::TryLockError::Error(error));
    }
    lock.try_lock()
}

fn snapshot_path(root: &Path, directory: &str) -> Result<std::path::PathBuf> {
    let mut components = Path::new(directory).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(ohno::app_err!(
            "invalid advisory cache snapshot path; clear the advisory cache and retry"
        ));
    }
    Ok(root.join(directory))
}

fn archive_identity(url: &str) -> String {
    blake3::hash(url.as_bytes()).to_hex().to_string()
}

fn snapshot_marker(cache: &Cache) -> CacheResult<(String, String)> {
    #[cfg(all(test, not(miri)))]
    if let Some(host) = tests::NativeScope::capture() {
        return host.marker(cache);
    }
    cache.load("last_synced.bin")
}

fn create_snapshot_stage(root: std::path::PathBuf) -> std::io::Result<SnapshotStage> {
    #[cfg(all(test, not(miri)))]
    if let Some(host) = tests::NativeScope::capture() {
        return host.create_stage(&root);
    }
    let stage = tempfile::Builder::new().prefix(".advisories-").tempdir_in(root);
    #[cfg(all(test, not(miri)))]
    let stage = stage.map(SnapshotStage::Native);
    stage
}

fn create_snapshot_marker(root: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    #[cfg(all(test, not(miri)))]
    if let Some(host) = tests::NativeScope::capture() {
        return host.create_marker(root);
    }
    tempfile::NamedTempFile::new_in(root)
}

fn save_snapshot_marker(cache: &Cache, filename: &str, data: &(String, &str)) -> Result<()> {
    #[cfg(all(test, not(miri)))]
    if let Some(host) = tests::NativeScope::capture() {
        return host.save_marker(cache, filename, data);
    }
    cache.save(filename, data)
}

fn persist_snapshot_marker(marker: tempfile::NamedTempFile, destination: &Path) -> std::io::Result<()> {
    #[cfg(all(test, not(miri)))]
    if let Some(host) = tests::NativeScope::capture() {
        host.persist_marker(&marker, destination);
        return Ok(());
    }
    marker.persist(destination).map(|_| ()).map_err(|error| error.error)
}

fn remove_snapshot(path: &Path) -> std::io::Result<()> {
    #[cfg(all(test, not(miri)))]
    if let Some(host) = tests::NativeScope::capture() {
        return host.remove(path);
    }
    std::fs::remove_dir_all(path)
}

async fn run_blocking_with_progress<T, F>(progress: &dyn Progress, msg: &str, success_verb: &str, blocking_fn: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    log::info!(target: LOG_TARGET, "{msg}");

    let progress_msg = msg.to_string();
    let start_time = std::time::Instant::now();
    progress.set_indeterminate(Box::new(move || progress_msg.clone()));

    let result = tokio::task::spawn_blocking(blocking_fn).await??;

    let elapsed = start_time.elapsed();
    log::debug!(target: LOG_TARGET, "Finished {success_verb} the advisory database in {:.3}s", elapsed.as_secs_f64());
    Ok(result)
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::env::var_os;
    use std::fs;
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::process::Command;
    use std::sync::Mutex;
    use std::task::Poll;

    use futures_util::StreamExt;
    #[cfg(windows)]
    use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    use super::*;

    fn publish_snapshot(cache: &Cache, stage: impl Into<SnapshotStage>, url: &str) -> Result<()> {
        super::publish_snapshot(SnapshotIo::new(cache.clone(), stage.into()), url)
    }

    const NATIVE_HOST_URL: &str = "https://example.test/native-advisory-archive";

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum NativeScenario {
        InvalidCachedPath,
        StageError,
        StagePanic,
        InvalidDatabase,
        MarkerError,
        MarkerPanic,
        SaveError,
        ForeignPrevious,
        InvalidPrevious,
        OwnedPrevious,
        LegacyMissing,
        LegacyDenied,
    }

    /// Scenario dispatcher and event recorder shared explicitly with blocking workers.
    pub(super) struct NativeHost {
        scenario: NativeScenario,
        events: Mutex<Vec<String>>,
    }

    impl NativeHost {
        fn new(scenario: NativeScenario) -> Arc<Self> {
            Arc::new(Self {
                scenario,
                events: Mutex::new(Vec::new()),
            })
        }

        pub(super) fn record(&self, event: &str) {
            // Only append/snapshot plain strings under this lock. Scripted callbacks
            // and panic probes run after unlocking so their trace remains readable.
            self.events.lock().unwrap().push(event.to_owned());
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        pub(super) fn marker(&self, cache: &Cache) -> CacheResult<(String, String)> {
            self.record("load");
            let directory = match self.scenario {
                NativeScenario::InvalidCachedPath => "../escape",
                NativeScenario::ForeignPrevious => "user-owned",
                NativeScenario::InvalidPrevious => ".advisories-../escape",
                NativeScenario::OwnedPrevious => ".advisories-old",
                _ => return CacheResult::Miss,
            };
            cache.load_data_for_test("last_synced.bin", (archive_identity(NATIVE_HOST_URL), directory.to_owned()))
        }

        pub(super) fn create_stage(&self, root: &Path) -> std::io::Result<SnapshotStage> {
            assert_eq!(root, Path::new(env!("CARGO_MANIFEST_DIR")));
            self.record("create-stage");
            assert_ne!(
                self.scenario,
                NativeScenario::StagePanic,
                "native advisory stage factory panic probe"
            );
            if self.scenario == NativeScenario::StageError {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "native advisory stage error probe",
                ));
            }
            let fixture = if self.scenario == NativeScenario::InvalidDatabase {
                "partial-write"
            } else {
                "database"
            };
            Ok(SnapshotStage::Borrowed(root.join("tests/fixtures/advisories").join(fixture)))
        }

        pub(super) fn create_marker(&self, root: &Path) -> std::io::Result<tempfile::NamedTempFile> {
            assert_eq!(root, Path::new(env!("CARGO_MANIFEST_DIR")));
            self.record("create-marker");
            assert_ne!(
                self.scenario,
                NativeScenario::MarkerPanic,
                "native advisory marker factory panic probe"
            );
            if self.scenario == NativeScenario::MarkerError {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "native advisory marker error probe",
                ));
            }
            let path = root.join("tests/fixtures/advisories/survivor-no-created-marker");
            assert!(!path.exists());
            let mut path = tempfile::TempPath::try_from_path(path)?;
            path.disable_cleanup(true);
            Ok(tempfile::NamedTempFile::from_parts(fs::File::open(root.join("Cargo.toml"))?, path))
        }

        pub(super) fn save_marker(&self, cache: &Cache, filename: &str, data: &(String, &str)) -> Result<()> {
            assert_eq!(cache.dir(), Path::new(env!("CARGO_MANIFEST_DIR")));
            assert_eq!(filename, "survivor-no-created-marker");
            assert_eq!(data, &(archive_identity(NATIVE_HOST_URL), "database"));
            self.record("save-marker");
            if self.scenario == NativeScenario::SaveError {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "native advisory marker save probe",
                ))
                .into_app_err("saving marker");
            }
            Ok(())
        }

        pub(super) fn persist_marker(&self, marker: &tempfile::NamedTempFile, destination: &Path) {
            assert!(!marker.path().exists());
            assert_eq!(destination, Path::new(env!("CARGO_MANIFEST_DIR")).join("last_synced.bin"));
            self.record("persist-marker");
        }

        pub(super) fn remove(&self, path: &Path) -> std::io::Result<()> {
            let name = path.file_name().unwrap().to_str().unwrap();
            self.record(&format!("remove:{name}"));
            match self.scenario {
                NativeScenario::LegacyMissing => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
                NativeScenario::LegacyDenied => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                _ => Ok(()),
            }
        }
    }

    /// Owned stages clean up on drop; borrowed checked-in fixtures never acquire ownership.
    pub(crate) enum SnapshotStage {
        Native(tempfile::TempDir),
        Borrowed(std::path::PathBuf),
    }

    impl From<tempfile::TempDir> for SnapshotStage {
        fn from(stage: tempfile::TempDir) -> Self {
            Self::Native(stage)
        }
    }

    impl SnapshotStage {
        pub(super) fn path(&self) -> &Path {
            match self {
                Self::Native(stage) => stage.path(),
                Self::Borrowed(path) => path,
            }
        }

        pub(super) fn keep(self) -> std::path::PathBuf {
            match self {
                Self::Native(stage) => stage.keep(),
                Self::Borrowed(path) => {
                    NativeScope::capture().unwrap().record("keep-stage");
                    path
                }
            }
        }
    }

    thread_local! {
        static NATIVE_HOST: std::cell::RefCell<Option<Arc<NativeHost>>> = const { std::cell::RefCell::new(None) };
    }

    /// Restores this thread's previous adapter on drop. Blocking workers must
    /// explicitly capture and install it; thread-local state does not follow tasks.
    pub(super) struct NativeScope(Option<Arc<NativeHost>>);

    impl NativeScope {
        pub(super) fn capture() -> Option<Arc<NativeHost>> {
            NATIVE_HOST.with_borrow(Clone::clone)
        }

        pub(super) fn install(host: Option<Arc<NativeHost>>) -> Self {
            Self(NATIVE_HOST.replace(host))
        }
    }

    impl Drop for NativeScope {
        fn drop(&mut self) {
            NATIVE_HOST.with_borrow_mut(|host| *host = self.0.take());
        }
    }

    type LockOpenHook = Box<dyn FnMut([bool; 4], &Path) -> std::io::Result<std::fs::File> + Send>;

    thread_local! {
        static LOCK_OPEN: std::cell::RefCell<Option<LockOpenHook>> = const { std::cell::RefCell::new(None) };
        pub(super) static LOCK_ATTEMPT_ERROR: std::cell::Cell<Option<std::io::Error>> = const { std::cell::Cell::new(None) };
    }

    pub(super) struct CacheLockOptions {
        native: tokio::fs::OpenOptions,
        flags: [bool; 4],
    }

    impl CacheLockOptions {
        pub(super) fn new() -> Self {
            Self {
                native: tokio::fs::OpenOptions::new(),
                flags: [false; 4],
            }
        }

        pub(super) fn read(&mut self, value: bool) -> &mut Self {
            self.native.read(value);
            self.flags[0] = value;
            self
        }

        pub(super) fn write(&mut self, value: bool) -> &mut Self {
            self.native.write(value);
            self.flags[1] = value;
            self
        }

        pub(super) fn create(&mut self, value: bool) -> &mut Self {
            self.native.create(value);
            self.flags[2] = value;
            self
        }

        pub(super) fn truncate(&mut self, value: bool) -> &mut Self {
            self.native.truncate(value);
            self.flags[3] = value;
            self
        }

        pub(super) async fn open(&self, path: impl AsRef<Path>) -> std::io::Result<tokio::fs::File> {
            let path = path.as_ref();
            let intercepted = LOCK_OPEN.with_borrow_mut(|hook| hook.as_mut().map(|hook| hook(self.flags, path)));
            match intercepted {
                Some(result) => result.map(tokio::fs::File::from_std),
                None => self.native.open(path).await,
            }
        }
    }

    fn native_cache() -> Cache {
        Cache::new(Path::new(env!("CARGO_MANIFEST_DIR")), Duration::MAX, false)
    }

    fn borrowed_database_stage() -> SnapshotStage {
        SnapshotStage::Borrowed(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories/database"))
    }

    fn inject_readonly_lock() -> impl Drop {
        struct Restore(Option<LockOpenHook>);
        impl Drop for Restore {
            fn drop(&mut self) {
                LOCK_OPEN.with_borrow_mut(|hook| *hook = self.0.take());
            }
        }
        Restore(LOCK_OPEN.replace(Some(Box::new(|flags, path| {
            assert_eq!(flags, [true, true, true, false]);
            assert_eq!(path, Path::new(env!("CARGO_MANIFEST_DIR")).join("archive.lock"));
            #[cfg(windows)]
            let path = Path::new("NUL");
            #[cfg(not(windows))]
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
            fs::File::open(path)
        }))))
    }

    async fn native_initialization_error(scenario: NativeScenario) -> (ohno::AppError, Vec<String>) {
        let host = NativeHost::new(scenario);
        let _scope = NativeScope::install(Some(Arc::clone(&host)));
        let _lock = inject_readonly_lock();
        let error = Provider::with_fetcher(
            &native_cache(),
            Arc::new(RecordingProgress::default()),
            NATIVE_HOST_URL,
            Arc::new(NoOpFetcher),
        )
        .await
        .unwrap_err();
        (error, host.events())
    }

    #[tokio::test]
    async fn survivor_native_initialization_preserves_cached_path_and_stage_failures() {
        let (error, events) = native_initialization_error(NativeScenario::InvalidCachedPath).await;
        assert!(error.to_string().contains("invalid advisory cache snapshot path"), "{error}");
        assert_eq!(events, ["load"]);

        let (error, events) = native_initialization_error(NativeScenario::StageError).await;
        assert_eq!(
            error.find_source::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(error.find_source::<tokio::task::JoinError>().is_none());
        assert_eq!(events, ["load", "create-stage"]);

        let (error, events) = native_initialization_error(NativeScenario::StagePanic).await;
        assert!(error.find_source::<tokio::task::JoinError>().unwrap().is_panic());
        assert_eq!(events, ["load", "create-stage"]);
    }

    #[tokio::test]
    async fn survivor_download_validation_error_keeps_the_exact_outer_context() {
        let (error, events) = native_initialization_error(NativeScenario::InvalidDatabase).await;
        assert!(error.to_string().contains("validating the downloaded advisory database"), "{error}");
        assert_eq!(events, ["load", "create-stage"]);
    }

    #[tokio::test]
    async fn survivor_initialization_publication_worker_and_io_errors_are_distinct() {
        for scenario in [NativeScenario::MarkerError, NativeScenario::MarkerPanic] {
            let (error, events) = native_initialization_error(scenario).await;
            assert_eq!(events, ["load", "create-stage", "load", "create-marker"]);
            if scenario == NativeScenario::MarkerPanic {
                assert!(error.find_source::<tokio::task::JoinError>().unwrap().is_panic());
            } else {
                assert!(error.find_source::<tokio::task::JoinError>().is_none());
                assert_eq!(
                    error.find_source::<std::io::Error>().unwrap().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
            }
        }
    }

    #[test]
    fn survivor_publication_ignores_foreign_snapshots_and_rejects_invalid_prefixed_paths() {
        for scenario in [NativeScenario::ForeignPrevious, NativeScenario::InvalidPrevious] {
            let host = NativeHost::new(scenario);
            let _scope = NativeScope::install(Some(Arc::clone(&host)));
            let result = publish_snapshot(&native_cache(), borrowed_database_stage(), NATIVE_HOST_URL);
            if scenario == NativeScenario::InvalidPrevious {
                assert!(result.unwrap_err().to_string().contains("invalid advisory cache snapshot path"));
                assert_eq!(host.events(), ["load"]);
            } else {
                result.unwrap();
                assert_eq!(
                    host.events(),
                    [
                        "load",
                        "create-marker",
                        "save-marker",
                        "persist-marker",
                        "keep-stage",
                        "remove:repo"
                    ]
                );
            }
        }
    }

    #[test]
    fn survivor_publication_native_marker_failures_do_not_publish_or_keep_the_stage() {
        for scenario in [NativeScenario::MarkerError, NativeScenario::SaveError] {
            let host = NativeHost::new(scenario);
            let _scope = NativeScope::install(Some(Arc::clone(&host)));
            let error = publish_snapshot(&native_cache(), borrowed_database_stage(), NATIVE_HOST_URL).unwrap_err();
            assert_eq!(
                error.find_source::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let expected = if scenario == NativeScenario::MarkerError {
                vec!["load", "create-marker"]
            } else {
                vec!["load", "create-marker", "save-marker"]
            };
            assert_eq!(host.events(), expected);
        }
    }

    #[test]
    fn publication_discovers_previous_snapshots_when_cache_reads_are_disabled() {
        let host = NativeHost::new(NativeScenario::OwnedPrevious);
        let _scope = NativeScope::install(Some(Arc::clone(&host)));
        let cache = Cache::new(Path::new(env!("CARGO_MANIFEST_DIR")), Duration::ZERO, true);
        let io = publication::SnapshotIo::new(cache, borrowed_database_stage());
        assert_eq!(
            io.previous_snapshot().unwrap(),
            Some(Path::new(env!("CARGO_MANIFEST_DIR")).join(".advisories-old"))
        );
        assert_eq!(host.events(), ["load"]);
    }

    #[test]
    fn survivor_legacy_cleanup_warns_only_for_errors_other_than_missing_directories() {
        for scenario in [NativeScenario::LegacyMissing, NativeScenario::LegacyDenied] {
            let host = NativeHost::new(scenario);
            let _scope = NativeScope::install(Some(Arc::clone(&host)));
            let logs = crate::facts::test_logging::capture_logs(|| {
                publish_snapshot(&native_cache(), borrowed_database_stage(), NATIVE_HOST_URL).unwrap();
            });
            let expected = vec![
                "load",
                "create-marker",
                "save-marker",
                "persist-marker",
                "keep-stage",
                "remove:repo",
            ];
            assert_eq!(host.events(), expected);
            let expected_logs = if scenario == NativeScenario::LegacyDenied {
                vec![(
                    log::Level::Warn,
                    LOG_TARGET.to_owned(),
                    format!(
                        "Could not remove old advisory snapshot '{}': {}",
                        native_cache().dir().join("repo").display(),
                        std::io::Error::from(std::io::ErrorKind::PermissionDenied)
                    ),
                )]
            } else {
                Vec::new()
            };
            assert_eq!(logs, expected_logs);
        }
    }

    #[tokio::test]
    async fn survivor_cache_lock_forwards_exact_native_open_options_and_errors() {
        struct Restore(Option<LockOpenHook>);
        impl Drop for Restore {
            fn drop(&mut self) {
                LOCK_OPEN.with_borrow_mut(|hook| *hook = self.0.take());
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // The awaited open completes the callback; this counter publishes no other state.
        let _restore = Restore(LOCK_OPEN.replace(Some(Box::new({
            let calls = Arc::clone(&calls);
            move |flags, path| {
                assert_eq!(flags, [true, true, true, false]);
                assert_eq!(path, root.join("archive.lock"));
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "native lock-open probe"))
            }
        }))));
        let error = open_cache_lock(root).await.unwrap_err();
        assert!(error.to_string().contains("native lock-open probe"), "{error}");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn survivor_cache_lock_directory_errors_return_before_creating_a_lock_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let error = lock_cache(&root).await.unwrap_err();
        assert!(error.to_string().contains("creating directory for the advisory cache"), "{error}");
        assert!(!root.join("archive.lock").exists());
    }

    #[tokio::test]
    async fn cache_lock_returns_acquisition_errors_after_successfully_opening_the_handle() {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                LOCK_ATTEMPT_ERROR.set(None);
            }
        }
        let _restore = Restore;
        let _open = inject_readonly_lock();
        LOCK_ATTEMPT_ERROR.set(Some(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "scripted cache-lock acquisition failure",
        )));
        let error = lock_cache(Path::new(env!("CARGO_MANIFEST_DIR"))).await.unwrap_err();
        let cause = error.find_source::<std::io::Error>().unwrap();
        assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(cause.to_string(), "scripted cache-lock acquisition failure");
        assert!(LOCK_ATTEMPT_ERROR.take().is_none());
    }

    #[tokio::test]
    async fn survivor_cache_lock_retries_at_exactly_fifty_virtual_milliseconds() {
        let attempts = std::cell::Cell::new(0);
        let control = tick::ClockControl::new();
        let clock = control.to_clock();
        let wait = wait_for_cache_lock_with(Duration::from_secs(1), &clock, || match attempts.replace(attempts.get() + 1) {
            0 => Err(std::fs::TryLockError::WouldBlock),
            1 => Ok(()),
            _ => panic!("the exact-delay lock test allows only the initial attempt and one retry"),
        });
        let mut wait = std::pin::pin!(wait);
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(attempts.get(), 1);
        control.advance(Duration::from_millis(49));
        assert!(futures_util::poll!(&mut wait).is_pending());
        control.advance(Duration::from_millis(1));
        assert!(matches!(futures_util::poll!(&mut wait), Poll::Ready(Ok(()))));
        assert_eq!(attempts.get(), 2);
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn survivor_cache_lock_preserves_native_directory_lock_errors_without_spinning() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/advisories/database");
        let lock = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(directory)
            .unwrap();
        let std::fs::TryLockError::Error(expected) = lock.try_lock().unwrap_err() else {
            panic!("a directory handle must refuse byte-range locking rather than contend with another owner");
        };
        let error = wait_for_cache_lock(&lock, Duration::from_secs(1), &tick::Clock::new_tokio())
            .await
            .unwrap_err();
        assert!(
            std::iter::successors(error.source(), |cause| cause.source())
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|cause| cause.kind() == expected.kind() && cause.raw_os_error() == expected.raw_os_error()),
            "{error}"
        );
    }

    fn write_advisory(root: &Path, package: &str, id: &str, withdrawn: Option<&str>) {
        let dir = root.join("crates").join(package);
        fs::create_dir_all(&dir).unwrap();

        let withdrawn_line = withdrawn.map_or_else(String::new, |date| format!("withdrawn = \"{date}\"\n"));
        let content = format!(
            "```toml\n\
             [advisory]\n\
             id = \"{id}\"\n\
             package = \"{package}\"\n\
             date = \"2020-01-01\"\n\
             informational = \"unmaintained\"\n\
             {withdrawn_line}\
             \n\
             [versions]\n\
             patched = []\n\
             ```\n\
             \n\
             # {package} is unmaintained\n\
             \n\
             Body text.\n"
        );

        fs::write(dir.join(format!("{id}.md")), content).unwrap();
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("advisories")
            .join(name)
    }

    /// Copies the checked-in one-advisory database into a mutable cache location under test.
    fn seed_database(destination: &Path) {
        let target = destination.join("crates").join("fixture-crate");
        fs::create_dir_all(&target).unwrap();
        for entry in fs::read_dir(fixture("database").join("crates").join("fixture-crate")).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }

    /// Test fetcher: writes a miniature advisory database where the real one would be cloned,
    /// and records how many times it was asked to.
    #[derive(Debug, Default)]
    struct FixtureFetcher {
        calls: AtomicUsize,
    }

    impl DbFetcher for FixtureFetcher {
        async fn fetch(&self, repo_path: &Path, _database_url: &str) -> Result<()> {
            let _ = self.calls.fetch_add(1, Ordering::SeqCst);
            write_advisory(repo_path, "fetched-crate", "RUSTSEC-2020-0003", None);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct NoOpFetcher;

    impl DbFetcher for NoOpFetcher {
        async fn fetch(&self, _repo_path: &Path, _database_url: &str) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ScriptedArchiveFetcher {
        failures: usize,
        stall: bool,
        // Counters only record handler calls and completed partial writes. Assertions read
        // them after the download finishes, so they publish no other memory.
        calls: Arc<AtomicUsize>,
        written: Arc<AtomicUsize>,
    }

    impl ScriptedArchiveFetcher {
        fn new(failures: usize, stall: bool) -> Self {
            Self {
                failures,
                stall,
                calls: Arc::new(AtomicUsize::new(0)),
                written: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    async fn wait_for_partial_write(destination: &Path, prefix: &[u8]) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let file = fs::read_dir(destination)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.is_file())
                    .expect("the downloader creates its attempt file before polling the body");
                if tokio::fs::read(file).await.unwrap() == prefix {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .into_app_err("timed out waiting for the expected partial archive write")
    }

    #[tokio::test(start_paused = true)]
    async fn partial_write_observation_is_bounded_for_missing_or_incorrect_bytes() {
        for attempt in ["partial-write/empty", "partial-write/wrong-prefix"] {
            let attempt = fixture(attempt);
            let observation = wait_for_partial_write(&attempt, b"partial archive data");
            let mut observation = std::pin::pin!(observation);
            assert!(futures_util::poll!(&mut observation).is_pending());

            tokio::time::advance(Duration::from_secs(10)).await;
            let error = observation.await.expect_err("absent or incorrect bytes must not wait indefinitely");

            assert!(
                error
                    .to_string()
                    .contains("timed out waiting for the expected partial archive write"),
                "{error}"
            );
        }
    }

    impl DbFetcher for ScriptedArchiveFetcher {
        async fn fetch(&self, repo_path: &Path, database_url: &str) -> Result<()> {
            const BODY_FAILED: usize = 1;
            const BODY_STALLED: usize = 2;

            let destination = repo_path.to_path_buf();
            let failures = self.failures;
            let stall = self.stall;
            let calls = Arc::clone(&self.calls);
            let written = Arc::clone(&self.written);
            // The body stream stores the event after its partial write is observed on disk; the
            // poll loop below swaps it out to decide when to advance the clock. The filesystem
            // write is awaited before the store, so the event carries no other data and SeqCst
            // is used only to keep this single-flag handshake obviously correct.
            let event = Arc::new(AtomicUsize::new(0));
            let handler = fetch::fake::FakeHandler::from_fn({
                let event = Arc::clone(&event);
                move |_| {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    if attempt >= failures {
                        return fetch::HttpResponseBuilder::new_fake()
                            .bytes(bytes::Bytes::from_static(include_bytes!(
                                "../../../tests/fixtures/advisories/root.tar.gz"
                            )))
                            .build();
                    }
                    let destination = destination.clone();
                    let written = Arc::clone(&written);
                    let event = Arc::clone(&event);
                    let prefix = b"partial archive data";
                    let stream = futures_util::stream::once(async move { Ok(bytes::Bytes::from_static(prefix).into()) }).chain(
                        futures_util::stream::once(async move {
                            // The stream is polled again only after write_all. Wait for Tokio's
                            // buffered filesystem write to reach disk before failing or stalling.
                            wait_for_partial_write(&destination, prefix)
                                .await
                                .expect("each attempt must write the expected prefix before the scripted body fails or stalls");
                            written.fetch_add(1, Ordering::SeqCst);
                            if stall {
                                event.store(BODY_STALLED, Ordering::SeqCst);
                                return std::future::pending().await;
                            }
                            event.store(BODY_FAILED, Ordering::SeqCst);
                            Err(fetch::HttpError::from(std::io::Error::other("scripted body failure")))
                        }),
                    );
                    fetch::HttpResponseBuilder::new_fake().stream(stream).build()
                }
            });
            let control = tick::ClockControl::new();
            let clock = control.to_clock();
            let client = fetch::HttpClient::builder_fake(handler, &clock).minimal_pipeline().build();

            let download = super::super::archive::download_with_clock(repo_path, database_url, DATABASE_FETCH_TIMEOUT, client, clock);
            let mut download = std::pin::pin!(download);
            let mut timed_out = false;
            std::future::poll_fn(|cx| {
                let result = download.as_mut().poll(cx);
                if result.is_pending() {
                    // Advance only after the partial write is observed, never during filesystem I/O.
                    match event.swap(0, Ordering::SeqCst) {
                        BODY_STALLED => {
                            control.advance(DATABASE_FETCH_TIMEOUT + Duration::from_secs(1));
                            timed_out = true;
                        }
                        BODY_FAILED => control.advance(Duration::from_secs(10)),
                        _ if timed_out => {
                            control.advance(Duration::from_secs(10));
                            timed_out = false;
                        }
                        _ => return Poll::Pending,
                    }
                    cx.waker().wake_by_ref();
                }
                result
            })
            .await
        }
    }

    /// Progress reporter that records what the provider reported.
    #[derive(Debug, Default)]
    struct RecordingProgress {
        messages: Mutex<Vec<String>>,
    }

    impl Progress for RecordingProgress {
        fn set_phase(&self, phase: &str) {
            self.messages
                .lock()
                .expect("no test panics while holding this lock")
                .push(phase.to_owned());
        }

        fn set_determinate(&self, callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {
            self.messages
                .lock()
                .expect("no test panics while holding this lock")
                .push(callback().2);
        }

        fn set_indeterminate(&self, callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {
            self.messages
                .lock()
                .expect("no test panics while holding this lock")
                .push(callback());
        }

        fn println(&self, msg: &str) {
            self.messages
                .lock()
                .expect("no test panics while holding this lock")
                .push(msg.to_owned());
        }

        fn done(&self) {
            self.messages
                .lock()
                .expect("no test panics while holding this lock")
                .push("done".to_owned());
        }
    }

    impl RecordingProgress {
        fn messages(&self) -> Vec<String> {
            self.messages.lock().expect("no test panics while holding this lock").clone()
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

    /// The provider only ever reports indeterminate progress, so this pins down what the
    /// recorder captures for the callbacks the provider leaves untouched.
    #[test]
    fn the_recording_progress_reporter_captures_every_callback() {
        let progress = RecordingProgress::default();

        progress.set_phase("Analyzing");
        progress.set_determinate(Box::new(|| (1, 2, "half done".to_owned())));
        progress.set_indeterminate(Box::new(|| "working".to_owned()));
        progress.println("a line");
        progress.done();

        assert_eq!(progress.messages(), vec!["Analyzing", "half done", "working", "a line", "done"]);
        assert!(!progress.use_colors(), "the recorder inherits the default, which is no color");
    }

    #[tokio::test]
    async fn cache_lock_waits_on_the_injected_clock_until_released() {
        let root = tempfile::tempdir().unwrap();
        let owner = lock_cache(root.path()).await.expect("the first cache user acquires the lock");
        let control = tick::ClockControl::new();
        let clock = control.to_clock();
        let lock = open_cache_lock(root.path()).await.unwrap();
        let wait = wait_for_cache_lock(&lock, DATABASE_FETCH_TIMEOUT, &clock);
        let mut wait = std::pin::pin!(wait);

        assert!(futures_util::poll!(&mut wait).is_pending());
        control.advance(Duration::from_millis(50));
        assert!(
            futures_util::poll!(&mut wait).is_pending(),
            "a held lock keeps the contender waiting"
        );
        drop(owner);
        assert!(
            futures_util::poll!(&mut wait).is_pending(),
            "the contender retries only after its poll interval"
        );
        control.advance(Duration::from_millis(50));

        assert!(matches!(futures_util::poll!(&mut wait), Poll::Ready(Ok(()))));
    }

    #[tokio::test]
    async fn cache_lock_contention_has_a_bounded_actionable_failure() {
        let root = tempfile::tempdir().unwrap();
        let _owner = lock_cache(root.path()).await.expect("the first cache user acquires the lock");
        let control = tick::ClockControl::new();
        let clock = control.to_clock();
        let lock = open_cache_lock(root.path()).await.unwrap();
        let wait = wait_for_cache_lock(&lock, DATABASE_FETCH_TIMEOUT, &clock);
        let mut wait = std::pin::pin!(wait);

        assert!(futures_util::poll!(&mut wait).is_pending());
        control.advance(DATABASE_FETCH_TIMEOUT);

        let Poll::Ready(Err(error)) = futures_util::poll!(&mut wait) else {
            panic!("a second owner must not wait beyond the deadline");
        };
        assert!(error.to_string().contains("waiting for another advisory cache refresh"), "{error}");
    }

    #[test]
    fn cache_snapshot_paths_cannot_escape_the_cache_root() {
        let error = snapshot_path(Path::new("cache"), "../outside").expect_err("the marker cannot reference a parent directory");

        assert!(error.to_string().contains("invalid advisory cache snapshot path"), "{error}");
    }

    #[test]
    fn survivor_snapshot_paths_require_exactly_one_normal_component() {
        for path in ["", ".", "..", "../escape", "nested/snapshot", "nested/../snapshot"] {
            assert!(snapshot_path(Path::new("cache"), path).is_err(), "{path:?}");
        }
        assert_eq!(
            snapshot_path(Path::new("cache"), ".advisories-valid").unwrap(),
            Path::new("cache").join(".advisories-valid")
        );
    }

    #[test]
    fn marker_publication_failure_removes_the_unpublished_snapshot() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("last_synced.bin")).unwrap();
        seed_database(&root.path().join("repo"));
        let cache = Cache::new(root.path(), Duration::MAX, false);
        let stage = tempfile::Builder::new().prefix(".advisories-").tempdir_in(root.path()).unwrap();
        let snapshot = stage.path().to_path_buf();

        let error = publish_snapshot(&cache, stage, "https://example.invalid/archive.tar.gz")
            .expect_err("a directory cannot be replaced by the marker file");

        assert!(error.to_string().contains("publishing the advisory cache"), "{error}");
        assert_eq!((snapshot.exists(), root.path().join("last_synced.bin").is_dir()), (false, true));
        assert_eq!(Database::open(&root.path().join("repo")).unwrap().iter().count(), 1);
    }

    #[tokio::test]
    async fn successful_archive_migration_removes_the_legacy_git_checkout() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path(), Duration::MAX, false);
        seed_database(&root.path().join("repo"));
        cache.save("last_synced.bin", &()).unwrap();

        let provider = Provider::with_fetcher(
            &cache,
            Arc::new(RecordingProgress::default()),
            "https://example.test/database.tar.gz",
            Arc::new(FixtureFetcher::default()),
        )
        .await
        .expect("a legacy marker must be replaced by a validated archive snapshot");

        assert!(!root.path().join("repo").exists());
        assert_eq!(provider.database.iter().next().unwrap().metadata.package.as_str(), "fetched-crate");
        assert!(matches!(
            cache.load::<(String, String)>("last_synced.bin"),
            CacheResult::Data((_, directory)) if directory.starts_with(".advisories-")
        ));
    }

    #[tokio::test]
    async fn failed_archive_migration_preserves_the_legacy_git_checkout_and_marker() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path(), Duration::MAX, false);
        seed_database(&root.path().join("repo"));
        cache.save("last_synced.bin", &()).unwrap();
        let marker = fs::read(root.path().join("last_synced.bin")).unwrap();

        Provider::with_fetcher(
            &cache,
            Arc::new(RecordingProgress::default()),
            "https://example.test/database.tar.gz",
            Arc::new(NoOpFetcher),
        )
        .await
        .expect_err("a missing staged database cannot replace the legacy checkout");

        assert_eq!(fs::read(root.path().join("last_synced.bin")).unwrap(), marker);
        assert_eq!(Database::open(&root.path().join("repo")).unwrap().iter().count(), 1);
    }

    #[test]
    fn legacy_checkout_cleanup_failure_does_not_undo_publication() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path(), Duration::MAX, false);
        fs::write(root.path().join("repo"), "not a directory").unwrap();
        cache.save("last_synced.bin", &()).unwrap();
        let stage = tempfile::Builder::new().prefix(".advisories-").tempdir_in(root.path()).unwrap();
        let name = stage.path().file_name().unwrap().to_str().unwrap().to_owned();

        publish_snapshot(&cache, stage, "new-url").expect("legacy cleanup is best-effort after publication");

        let CacheResult::Data(marker) = cache.load::<(String, String)>("last_synced.bin") else {
            panic!("cleanup failures must not discard the new marker");
        };
        assert_eq!(marker, (archive_identity("new-url"), name));
    }

    #[tokio::test]
    async fn archive_body_error_retries_from_a_clean_file_before_publication() {
        assert_archive_body_retry(false).await;
    }

    #[tokio::test]
    async fn archive_body_timeout_retries_from_a_clean_file_before_publication() {
        assert_archive_body_retry(true).await;
    }

    async fn assert_archive_body_retry(stall: bool) {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path(), Duration::MAX, false);
        let fetcher = Arc::new(ScriptedArchiveFetcher::new(1, stall));

        let provider = Provider::with_fetcher(
            &cache,
            Arc::new(RecordingProgress::default()),
            "https://example.test/database.tar.gz",
            Arc::clone(&fetcher),
        )
        .await
        .expect("a complete retry must replace the partial attempt");

        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fetcher.written.load(Ordering::SeqCst), 1);
        assert_eq!(provider.database.iter().next().unwrap().metadata.package.as_str(), "archive-crate");
        let CacheResult::Data((_, directory)) = cache.load::<(String, String)>("last_synced.bin") else {
            panic!("the validated retry must publish a marker");
        };
        let snapshot = root.path().join(directory);
        assert_eq!(Database::open(&snapshot).unwrap().iter().count(), 1);
        assert_eq!(
            fs::read_dir(snapshot)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            ["crates"]
        );
    }

    #[tokio::test]
    async fn exhausted_archive_body_retries_preserve_the_previous_database_and_marker() {
        for stall in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cache = Cache::new(root.path(), Duration::MAX, false);
            let old_name = ".advisories-old";
            let previous = root.path().join(old_name);
            seed_database(&previous);
            cache.save("last_synced.bin", &("old-url", old_name)).unwrap();
            let marker = fs::read(root.path().join("last_synced.bin")).unwrap();
            let advisory_path = previous.join("crates").join("fixture-crate").join("RUSTSEC-2020-0001.md");
            let advisory = fs::read(&advisory_path).unwrap();
            let fetcher = Arc::new(ScriptedArchiveFetcher::new(usize::MAX, stall));

            let error = Provider::with_fetcher(
                &cache,
                Arc::new(RecordingProgress::default()),
                "https://example.test/database.tar.gz",
                Arc::clone(&fetcher),
            )
            .await
            .expect_err("four incomplete transfers must exhaust body retries");

            let message = if stall { "download timed out" } else { "scripted body failure" };
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert_eq!(fetcher.calls.load(Ordering::SeqCst), 4);
            assert_eq!(fetcher.written.load(Ordering::SeqCst), 4);
            assert_eq!(fs::read(root.path().join("last_synced.bin")).unwrap(), marker);
            assert_eq!(fs::read(advisory_path).unwrap(), advisory);
            assert_eq!(Database::open(&previous).unwrap().iter().count(), 1);
            assert_eq!(
                fs::read_dir(root.path())
                    .unwrap()
                    .filter(|entry| entry.as_ref().unwrap().file_type().unwrap().is_dir())
                    .count(),
                1,
                "failed attempts must leave no staging directories"
            );
        }
    }

    #[test]
    fn failure_to_clean_an_old_snapshot_does_not_undo_a_successful_publication() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path(), Duration::MAX, false);
        let old_name = ".advisories-old";
        fs::write(root.path().join(old_name), "not a directory").unwrap();
        cache.save("last_synced.bin", &("old-url", old_name)).unwrap();
        let stage = tempfile::Builder::new().prefix(".advisories-").tempdir_in(root.path()).unwrap();
        let name = stage.path().file_name().unwrap().to_str().unwrap().to_owned();

        publish_snapshot(&cache, stage, "https://example.invalid/archive.tar.gz")
            .expect("old snapshot cleanup is optional once the new snapshot is published");

        let CacheResult::Data(marker) = cache.load::<(String, String)>("last_synced.bin") else {
            panic!("the successful refresh must publish a marker");
        };
        assert_eq!(marker, (archive_identity("https://example.invalid/archive.tar.gz"), name));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
    async fn a_successful_fetch_marks_the_database_as_synchronized() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path(), Duration::from_hours(8760), false);
        let fetcher = Arc::new(FixtureFetcher::default());
        let progress = Arc::new(RecordingProgress::default());

        let provider = Provider::with_fetcher(
            &cache,
            Arc::clone(&progress) as Arc<dyn Progress>,
            "https://example.invalid/db.git",
            Arc::clone(&fetcher),
        )
        .await
        .expect("the fixture fetcher always produces a valid database");

        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "the database must be fetched exactly once");
        assert!(
            matches!(cache.load::<(String, String)>("last_synced.bin"), CacheResult::Data((identity, _)) if identity == archive_identity("https://example.invalid/db.git")),
            "a successful fetch must record the synchronization marker"
        );
        assert_eq!(
            progress.messages(),
            vec![
                "Downloading the advisory database".to_owned(),
                "Opening the advisory database".to_owned()
            ]
        );

        let crates = vec![CrateSpec::from_arcs("fetched-crate".into(), Arc::new("1.0.0".parse().unwrap()))];
        let results: Vec<_> = provider.get_advisory_data(crates.into()).await.collect();
        assert_eq!(results.len(), 1);
        let data = results[0].1.as_ref().expect("every crate yields Found");
        assert_eq!(data.total.unmaintained_warning_count, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call mkdir")]
    async fn an_existing_marker_skips_the_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path(), Duration::from_hours(8760), false);
        let fetcher = Arc::new(FixtureFetcher::default());

        // Seed a database and its synchronization marker, so no fetch is needed.
        write_advisory(&tmp.path().join("repo"), "seeded-crate", "RUSTSEC-2020-0004", None);
        cache
            .save("last_synced.bin", &(archive_identity("https://example.invalid/db.git"), "repo"))
            .unwrap();

        let provider = Provider::with_fetcher(
            &cache,
            Arc::new(RecordingProgress::default()),
            "https://example.invalid/db.git",
            Arc::clone(&fetcher),
        )
        .await
        .expect("a seeded database opens successfully");

        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            0,
            "an already-synchronized database must not be fetched again"
        );

        let crates = vec![CrateSpec::from_arcs("seeded-crate".into(), Arc::new("1.0.0".parse().unwrap()))];
        assert_eq!(provider.get_advisory_data(crates.into()).await.count(), 1);
    }

    #[tokio::test]
    async fn signed_archive_urls_are_fingerprinted_without_losing_cache_identity() {
        let root = tempfile::tempdir().unwrap();
        let cache = Cache::new(root.path(), Duration::MAX, false);
        let fetcher = Arc::new(FixtureFetcher::default());
        let first = "https://test-user:synthetic-password@mirror.invalid/db.tar.gz?token=synthetic-token-one";
        let second = "https://test-user:synthetic-password@mirror.invalid/db.tar.gz?token=synthetic-token-two";

        for (url, expected_fetches) in [(first, 1), (first, 1), (second, 2)] {
            Provider::with_fetcher(&cache, Arc::new(RecordingProgress::default()), url, Arc::clone(&fetcher))
                .await
                .expect("the fixture fetcher produces a valid database for each signed URL");

            assert_eq!(fetcher.calls.load(Ordering::SeqCst), expected_fetches);
            let CacheResult::Data((identity, _)) = cache.load::<(String, String)>("last_synced.bin") else {
                panic!("a successful refresh must publish its archive fingerprint");
            };
            assert_eq!(identity, archive_identity(url));
            let marker = fs::read(root.path().join("last_synced.bin")).unwrap();
            for secret in ["test-user", "synthetic-password", "synthetic-token-one", "synthetic-token-two"] {
                assert!(
                    !marker.windows(secret.len()).any(|window| window == secret.as_bytes()),
                    "the cache marker must not contain URL credentials"
                );
            }
        }
    }

    #[tokio::test]
    async fn synchronization_marker_write_failures_are_returned_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("not-a-directory");
        fs::write(&cache_root, b"blocks cache writes").unwrap();
        let cache = Cache::new(&cache_root, Duration::from_hours(8760), false);

        let error = Provider::with_fetcher(
            &cache,
            Arc::new(RecordingProgress::default()),
            "https://example.invalid/db.git",
            Arc::new(NoOpFetcher),
        )
        .await
        .expect_err("an unwritable synchronization marker must be returned as an error");

        assert!(error.to_string().contains("creating directory"), "{error}");
    }

    #[tokio::test]
    async fn a_panicking_blocking_operation_is_returned_without_panicking_the_caller() {
        let progress = RecordingProgress::default();

        let error = run_blocking_with_progress(&progress, "Panicking operation", "panicking", || -> Result<()> {
            panic!("deliberate blocking-task panic");
        })
        .await
        .expect_err("a panicking blocking task must be returned as an error");

        assert!(error.to_string().contains("task"), "{error}");
    }

    #[test]
    fn scan_advisories_ignores_withdrawn_advisories() {
        // The scan's summary is logged at debug level; evaluate its arguments too.
        crate::facts::test_logging::enable_log_argument_evaluation();

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        write_advisory(root, "active-crate", "RUSTSEC-2020-0001", None);
        write_advisory(root, "cleared-crate", "RUSTSEC-2020-0002", Some("2021-01-01"));

        let database = Database::open(root).unwrap();

        let crates = vec![
            CrateSpec::from_arcs("active-crate".into(), Arc::new("1.0.0".parse().unwrap())),
            CrateSpec::from_arcs("cleared-crate".into(), Arc::new("1.0.0".parse().unwrap())),
        ];

        let results: HashMap<String, AdvisoryData> = scan_advisories(&database, crates)
            .map(|(spec, result)| {
                let data = result
                    .as_ref()
                    .expect("scan_advisories always yields Found for every crate it was given");
                (spec.name().to_string(), data.clone())
            })
            .collect();

        assert_eq!(results["active-crate"].per_version.unmaintained_warning_count, 1);
        assert_eq!(results["active-crate"].total.unmaintained_warning_count, 1);
        assert_eq!(results["cleared-crate"].per_version.unmaintained_warning_count, 0);
        assert_eq!(results["cleared-crate"].total.unmaintained_warning_count, 0);
    }

    #[test]
    fn a_withdrawn_advisory_does_not_stop_the_remaining_scan() {
        let tmp = tempfile::tempdir().unwrap();
        // RustSec iterates these identifiers in order. The low withdrawn ID deliberately precedes
        // the high active ID so this fixture proves that scanning continues after a withdrawal.
        write_advisory(tmp.path(), "ordered-crate", "RUSTSEC-2020-0001", Some("2021-01-01"));
        write_advisory(tmp.path(), "ordered-crate", "RUSTSEC-2020-9999", None);
        let database = Database::open(tmp.path()).unwrap();
        let crates = vec![CrateSpec::from_arcs("ordered-crate".into(), Arc::new("1.0.0".parse().unwrap()))];

        let results: Vec<_> = scan_advisories(&database, crates).collect();
        let data = results[0].1.as_ref().expect("the crate is always returned");

        assert_eq!(data.total.unmaintained_warning_count, 1);
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn scan_summary_log_reports_checked_and_matched_counts() {
        let logs = run_ignored_helper("helper_capture_scan_summary_log");

        assert!(
            logs.contains("Completed scan of advisory database: checked 3 advisories, found 4 matches for 1 crates"),
            "unexpected logs:\n{logs}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn database_operations_log_their_exact_success_verbs() {
        let logs = run_ignored_helper("helper_capture_database_operation_logs");

        assert!(logs.contains("Finished opening the advisory database"), "{logs}");
        assert!(logs.contains("Finished downloading the advisory database"), "{logs}");
    }

    #[tokio::test]
    #[ignore = "spawned by database_operations_log_their_exact_success_verbs"]
    async fn helper_capture_database_operation_logs() {
        if var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
            return;
        }

        install_capturing_logger();
        let tmp = tempfile::tempdir().unwrap();
        let seeded = tmp.path().join("seeded");
        write_advisory(&seeded, "seeded-crate", "RUSTSEC-2020-0100", None);
        let progress = RecordingProgress::default();
        let lock = Arc::new(lock_cache(tmp.path()).await.unwrap());
        let _database = open_db(&seeded, &progress, lock).await.unwrap();

        let downloaded = tmp.path().join("downloaded");
        download_db(
            &downloaded,
            "https://example.invalid/db.git",
            &progress,
            Arc::new(FixtureFetcher::default()),
        )
        .await
        .unwrap();

        println!("{}", captured_logs());
    }

    #[test]
    #[ignore = "spawned by scan_summary_log_reports_checked_and_matched_counts"]
    fn helper_capture_scan_summary_log() {
        if var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
            return;
        }

        install_capturing_logger();

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        write_advisory(root, "matched-crate", "RUSTSEC-2020-0010", None);
        write_advisory(root, "matched-crate", "RUSTSEC-2020-0011", None);
        write_advisory(root, "other-crate", "RUSTSEC-2020-0012", None);
        write_advisory(root, "withdrawn-crate", "RUSTSEC-2020-0013", Some("2021-01-01"));

        let database = Database::open(root).unwrap();
        let crates = vec![
            CrateSpec::from_arcs("matched-crate".into(), Arc::new("1.0.0".parse().unwrap())),
            CrateSpec::from_arcs("matched-crate".into(), Arc::new("2.0.0".parse().unwrap())),
        ];

        assert_eq!(scan_advisories(&database, crates).count(), 2);

        println!("{}", captured_logs());
    }

    #[test]
    fn scan_advisories_returns_zero_counts_for_unmatched_crates() {
        let tmp = tempfile::tempdir().unwrap();
        write_advisory(tmp.path(), "other-crate", "RUSTSEC-2020-0005", None);
        let database = Database::open(tmp.path()).unwrap();

        let crates = vec![CrateSpec::from_arcs("wanted-crate".into(), Arc::new("1.0.0".parse().unwrap()))];

        let results: Vec<_> = scan_advisories(&database, crates).collect();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name(), "wanted-crate");
        let data = results[0].1.as_ref().expect("scan_advisories always yields Found");
        assert_eq!(data.per_version.unmaintained_warning_count, 0);
        assert_eq!(data.total.unmaintained_warning_count, 0);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod miri_tests {
    use core::time::Duration;

    use super::DATABASE_FETCH_TIMEOUT;

    #[test]
    fn advisory_database_fetch_policy_is_explicit() {
        assert_eq!(DATABASE_FETCH_TIMEOUT, Duration::from_mins(1));
    }
}
