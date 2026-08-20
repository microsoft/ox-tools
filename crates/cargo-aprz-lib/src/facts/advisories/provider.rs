// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::path::Path;
use std::sync::Arc;

use compact_str::CompactString;
use ohno::IntoAppError;
use rustsec::database::Database;
use rustsec::repository::git::Repository;

use super::AdvisoryData;
use crate::facts::ProviderResult;
use crate::facts::cache::{Cache, CacheResult};
use crate::facts::crate_spec::CrateSpec;
use crate::facts::progress::Progress;
use crate::{HashMap, Result};

/// Log target for advisories provider
const LOG_TARGET: &str = "advisories";

#[derive(Debug)]
pub struct Provider {
    database: Arc<Database>,
}

const DATABASE_FETCH_TIMEOUT: Duration = Duration::from_mins(1);

/// How the advisory database is brought into the cache directory.
///
/// This is a seam: production clones the `RustSec` git repository, while tests substitute a
/// local database. `rustsec` refuses any database URL that does not start with `https://`,
/// so without this seam the successful-fetch path could not be exercised offline.
///
/// Implementations run on a blocking thread.
trait DbFetcher: Send + Sync + 'static {
    fn fetch(&self, repo_path: &Path, database_url: &str) -> Result<()>;
}

/// The production fetcher: clones the advisory database over git.
struct GitFetcher;

impl DbFetcher for GitFetcher {
    fn fetch(&self, repo_path: &Path, database_url: &str) -> Result<()> {
        Repository::fetch(database_url, repo_path, true, DATABASE_FETCH_TIMEOUT)
            .map(|_| ())
            .map_err(Into::into)
    }
}

impl Provider {
    pub async fn new(cache: &Cache, progress: Arc<dyn Progress>, database_url: &str) -> Result<Self> {
        Self::with_fetcher(cache, progress, database_url, Arc::new(GitFetcher)).await
    }

    async fn with_fetcher(cache: &Cache, progress: Arc<dyn Progress>, database_url: &str, fetcher: Arc<dyn DbFetcher>) -> Result<Self> {
        let cache_dir = cache.dir();
        let sync_filename = "last_synced.bin";
        let repo_path = cache_dir.join("repo");

        let needs_fetch = matches!(cache.load::<()>(sync_filename), CacheResult::Miss);

        if needs_fetch {
            download_db(&repo_path, database_url, progress.as_ref(), fetcher)
                .await
                .into_app_err("downloading the advisory database")?;
            cache.save(sync_filename, &())?;
        }

        Ok(Self {
            database: Arc::new(
                open_db(&repo_path, progress.as_ref())
                    .await
                    .into_app_err("opening the advisory database")?,
            ),
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

async fn open_db(cache_dir: impl AsRef<Path>, progress: &dyn Progress) -> Result<Database> {
    let cache_path = cache_dir.as_ref().to_path_buf();

    run_blocking_with_progress(progress, "Opening the advisory database", "opening", move || {
        Database::open(&cache_path).map_err(Into::into)
    })
    .await
}

async fn download_db(cache_dir: impl AsRef<Path>, database_url: &str, progress: &dyn Progress, fetcher: Arc<dyn DbFetcher>) -> Result<()> {
    let cache_path = cache_dir.as_ref().to_path_buf();
    let database_url = database_url.to_owned();

    run_blocking_with_progress(progress, "Downloading the advisory database", "downloading", move || {
        fetcher.fetch(&cache_path, &database_url)
    })
    .await
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
    use std::fs;
    use std::process::Command;
    use std::sync::Mutex;

    use super::*;

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

    /// Test fetcher: writes a miniature advisory database where the real one would be cloned,
    /// and records how many times it was asked to.
    #[derive(Debug, Default)]
    struct FixtureFetcher {
        calls: AtomicUsize,
    }

    impl DbFetcher for FixtureFetcher {
        fn fetch(&self, repo_path: &Path, _database_url: &str) -> Result<()> {
            let _ = self.calls.fetch_add(1, Ordering::SeqCst);
            write_advisory(repo_path, "fetched-crate", "RUSTSEC-2020-0003", None);
            Ok(())
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
            Arc::clone(&fetcher) as Arc<dyn DbFetcher>,
        )
        .await
        .expect("the fixture fetcher always produces a valid database");

        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "the database must be fetched exactly once");
        assert!(
            matches!(cache.load::<()>("last_synced.bin"), CacheResult::Data(())),
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
        cache.save("last_synced.bin", &()).unwrap();

        let provider = Provider::with_fetcher(
            &cache,
            Arc::new(RecordingProgress::default()),
            "https://example.invalid/db.git",
            Arc::clone(&fetcher) as Arc<dyn DbFetcher>,
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
    #[cfg_attr(miri, ignore = "spawns the capturing helper as a subprocess, which Miri cannot execute")]
    fn scan_summary_log_reports_checked_and_matched_counts() {
        let logs = run_ignored_helper("helper_capture_scan_summary_log");

        assert!(
            logs.contains("Completed scan of advisory database: checked 3 advisories, found 4 matches for 1 crates"),
            "unexpected logs:\n{logs}"
        );
    }

    #[test]
    #[ignore = "spawned by scan_summary_log_reports_checked_and_matched_counts"]
    fn helper_capture_scan_summary_log() {
        if std::env::var_os("CARGO_APRZ_CAPTURE_LOGS").is_none() {
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

    #[test]
    fn advisory_database_fetch_timeout_is_one_minute() {
        assert_eq!(DATABASE_FETCH_TIMEOUT, Duration::from_mins(1));
    }
}
