// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use ohno::IntoAppError;

use super::cache::Cache;
use super::cache_lock::{CacheLockGuard, acquire_cache_lock};
use super::crate_facts::CrateFacts;
use super::crate_spec::CrateSpec;
use super::progress::Progress;
use super::request_tracker::RequestTracker;
use super::{BugLabelMatcher, CrateRef, CratesData, Endpoints, ProviderResult};
use crate::{HashMap, HashSet, Result};

/// Collector for gathering crate information from different sources
pub struct Collector {
    crates_provider: super::crates::Provider,
    hosting_provider: super::hosting::Provider,
    advisories_provider: super::advisories::Provider,
    codebase_provider: super::codebase::Provider,
    coverage_provider: super::coverage::Provider,
    docs_provider: super::docs::Provider,
    progress: Arc<dyn Progress>,
    _cache_lock: CacheLockGuard,
}

impl core::fmt::Debug for Collector {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Collector")
            .field("crates_provider", &self.crates_provider)
            .field("hosting_provider", &self.hosting_provider)
            .field("advisories_provider", &self.advisories_provider)
            .field("codebase_provider", &self.codebase_provider)
            .field("coverage_provider", &self.coverage_provider)
            .field("docs_provider", &self.docs_provider)
            .field("progress", &"<dyn Progress>")
            .finish_non_exhaustive()
    }
}

impl Collector {
    #[expect(clippy::too_many_arguments, reason = "all cache TTL parameters are necessary for configuration")]
    pub async fn new(
        github_token: Option<&str>,
        codeberg_token: Option<&str>,
        cache_dir: impl AsRef<Path>,
        crates_cache_ttl: Duration,
        hosting_cache_ttl: Duration,
        codebase_cache_ttl: Duration,
        coverage_cache_ttl: Duration,
        advisories_cache_ttl: Duration,
        ignore_cached: bool,
        bug_labels: Arc<BugLabelMatcher>,
        progress: impl Progress + 'static,
        endpoints: &Endpoints,
    ) -> Result<Self> {
        let progress: Arc<dyn Progress> = Arc::new(progress);
        progress.set_phase("Preparing");

        let crates_cache_dir = create_cache_dir(&cache_dir, "crates")?;
        let hosting_cache_dir = create_cache_dir(&cache_dir, "hosting")?;
        let codebase_cache_dir = create_cache_dir(&cache_dir, "codebase")?;
        let coverage_cache_dir = create_cache_dir(&cache_dir, "coverage")?;
        let advisories_cache_dir = create_cache_dir(&cache_dir, "advisories")?;
        let docs_cache_dir = create_cache_dir(&cache_dir, "docs")?;

        // Acquire cache lock to prevent concurrent access
        let cache_lock = acquire_cache_lock(cache_dir.as_ref()).await?;

        let hosting_cache = Cache::new(hosting_cache_dir, hosting_cache_ttl, ignore_cached);
        let codebase_cache = Cache::new(codebase_cache_dir, codebase_cache_ttl, ignore_cached);
        let coverage_cache = Cache::new(coverage_cache_dir, coverage_cache_ttl, ignore_cached);
        let advisories_cache = Cache::new(advisories_cache_dir, advisories_cache_ttl, ignore_cached);
        let docs_cache = Cache::new(docs_cache_dir, Duration::MAX, ignore_cached);

        Ok(Self {
            crates_provider: super::crates::Provider::new(
                &crates_cache_dir,
                crates_cache_ttl,
                Arc::clone(&progress),
                Utc::now(),
                ignore_cached,
                Some(endpoints.dump_url()),
            )
            .await?,

            advisories_provider: super::advisories::Provider::new(&advisories_cache, Arc::clone(&progress), endpoints.advisory_url())
                .await?,

            hosting_provider: super::hosting::Provider::new(github_token, codeberg_token, hosting_cache, bug_labels, endpoints)?,
            codebase_provider: super::codebase::Provider::new(codebase_cache),
            coverage_provider: super::coverage::Provider::new(coverage_cache, Some(endpoints.coverage_url())),
            docs_provider: super::docs::Provider::new(docs_cache, Some(endpoints.docs_url())),
            progress,
            _cache_lock: cache_lock,
        })
    }

    /// Collect facts for multiple crates
    pub async fn collect(&self, crate_refs: &[CrateRef], suggestions: bool) -> Result<impl Iterator<Item = CrateFacts>> {
        if crate_refs.is_empty() {
            return Ok(Vec::new().into_iter());
        }

        // Deduplicate crate refs before processing, preserving insertion order
        let crate_refs: Vec<_> = {
            let mut seen = HashSet::default();
            crate_refs.iter().filter(|r| seen.insert(*r)).cloned().collect()
        };

        // Step 1: Start identification phase - query crates provider
        self.progress.set_phase("Identifying");
        let crate_data = self
            .crates_provider
            .get_crates_data(&crate_refs, self.progress.as_ref(), suggestions)
            .await;

        // Deduplicate CrateSpecs to prevent concurrent processing of the same crate
        let crate_data: Vec<_> = crate_data
            .fold(HashMap::default(), |mut map, (crate_spec, provider_result)| {
                let _ = map.entry(crate_spec).or_insert(provider_result);
                map
            })
            .into_iter()
            .collect();

        // Step 2: Query phase - parallel data gathering
        self.progress.set_phase("Querying");
        let collected_facts = self.query_providers(crate_data).await;

        self.progress.done();

        Ok(collected_facts.into_iter())
    }

    async fn query_providers(&self, crates_data: Vec<(CrateSpec, ProviderResult<CratesData>)>) -> Vec<CrateFacts> {
        let request_tracker = RequestTracker::new(&self.progress);

        let mut facts_map: HashMap<CrateSpec, CrateFacts> = crates_data
            .into_iter()
            .map(|(crate_spec, crates_result)| {
                let facts = CrateFacts {
                    crate_spec: crate_spec.clone(),
                    crates_data: crates_result,
                    hosting_data: ProviderResult::Unavailable("not queried".into()),
                    advisory_data: ProviderResult::Unavailable("not queried".into()),
                    codebase_data: ProviderResult::Unavailable("not queried".into()),
                    coverage_data: ProviderResult::Unavailable("not queried".into()),
                    docs_data: ProviderResult::Unavailable("not queried".into()),
                };
                (crate_spec, facts)
            })
            .collect();

        let all_queryable_specs: Arc<[CrateSpec]> = facts_map
            .iter()
            .filter(|(_, facts)| facts.crates_data.is_found())
            .map(|(crate_spec, _)| crate_spec.clone())
            .collect();

        if !all_queryable_specs.is_empty() {
            // Phase 1: Run advisory, hosting, codebase, and coverage providers in parallel.
            let (advisory_iter, hosting_iter, codebase_iter, coverage_iter) = tokio::join!(
                self.advisories_provider.get_advisory_data(Arc::clone(&all_queryable_specs)),
                self.hosting_provider
                    .get_hosting_data(Arc::clone(&all_queryable_specs), &request_tracker),
                self.codebase_provider
                    .get_codebase_data(Arc::clone(&all_queryable_specs), &request_tracker),
                self.coverage_provider
                    .get_coverage_data(Arc::clone(&all_queryable_specs), &request_tracker),
            );

            macro_rules! update_facts {
                ($iter:expr, $field:ident) => {
                    for (crate_spec, result) in $iter {
                        if let Some(facts) = facts_map.get_mut(&crate_spec) {
                            facts.$field = result;
                        }
                    }
                };
            }

            update_facts!(advisory_iter, advisory_data);
            update_facts!(hosting_iter, hosting_data);
            update_facts!(codebase_iter, codebase_data);
            update_facts!(coverage_iter, coverage_data);

            // Phase 2: Run docs provider separately so its memory-intensive rustdoc JSON
            // parsing doesn't overlap with the codebase provider's source analysis.
            let docs_iter = self.docs_provider.get_docs_data(all_queryable_specs, &request_tracker).await;
            update_facts!(docs_iter, docs_data);
        }

        facts_map.into_values().collect()
    }
}

/// Create a cache directory by joining a base path with a name
fn create_cache_dir(base_path: impl AsRef<Path>, name: impl AsRef<str>) -> Result<PathBuf> {
    let name_str = name.as_ref();
    let cache_path = base_path.as_ref().join(name_str);

    #[cfg(windows)]
    let needs_creation = !cache_path.exists();

    fs::create_dir_all(&cache_path).into_app_err_with(|| format!("creating `{name_str}` cache directory"))?;

    #[cfg(windows)]
    configure_windows_cache_directory(&cache_path, name_str, needs_creation);

    Ok(cache_path)
}

/// Disable NTFS compression for a newly created crates cache.
#[cfg(windows)]
#[mutants::skip] // Windows-only optimization; Linux mutation runners cannot compile or observe this code.
fn configure_windows_cache_directory(cache_path: &Path, name: &str, needs_creation: bool) {
    if needs_creation && name == "crates" {
        disable_directory_compression(cache_path);
    }
}

/// Disables NTFS compression on a directory to improve memory-mapped file performance.
///
/// This prevents the approximately 26 second "streaming" lag that occurs when Windows decompresses
/// data on the fly during memory mapping operations.
///
/// This function is completely opportunistic - if it fails for any reason, it fails silently.
#[cfg(windows)]
#[mutants::skip] // Windows-only optimization; Linux mutation runners cannot compile or observe this code.
fn disable_directory_compression(path: impl AsRef<Path>) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{addr_of, addr_of_mut};

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        COMPRESSION_FORMAT_NONE, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_COMPRESSION;
    use windows::core::HSTRING;

    /// RAII wrapper for Windows HANDLE that ensures it's closed when dropped
    struct HandleGuard(HANDLE);

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            // SAFETY: handle is valid and we're done using it
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    let path = path.as_ref();

    // Convert path to Windows HSTRING via wide string
    let wide_chars: Vec<_> = OsStr::new(path).encode_wide().collect();
    let path_wide = HSTRING::from_wide(&wide_chars);

    // Open the directory with FILE_WRITE_DATA access and FILE_FLAG_BACKUP_SEMANTICS
    // SAFETY: Calling Windows API with valid path
    let handle = unsafe {
        CreateFileW(
            &path_wide,
            FILE_WRITE_DATA.0,                  // Write access needed for DeviceIoControl
            FILE_SHARE_READ | FILE_SHARE_WRITE, // Allow concurrent access
            None,                               // No security attributes
            OPEN_EXISTING,                      // Directory must exist
            FILE_FLAG_BACKUP_SEMANTICS,         // Required to open directories
            None,                               // No template file
        )
    };

    let Ok(handle) = handle else {
        return; // Silently fail if we can't open the directory
    };

    let _guard = HandleGuard(handle); // Auto-closes handle on drop

    let compression_format = COMPRESSION_FORMAT_NONE;
    let mut bytes_returned: u32 = 0;

    #[expect(clippy::cast_possible_truncation, reason = "size_of::<u16>() is always 2, which fits in u32")]
    // SAFETY: Calling DeviceIoControl with valid handle and compression format
    let _ = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_COMPRESSION,
            Some(addr_of!(compression_format).cast()),
            size_of::<u16>() as u32,
            None,
            0,
            Some(addr_of_mut!(bytes_returned)),
            None,
        )
    };
}

#[cfg(all(test, windows))]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::TempDir;

    use super::disable_directory_compression;

    #[test]
    fn compression_is_disabled_for_an_existing_directory() {
        let dir = TempDir::new().expect("creating a temporary directory");

        disable_directory_compression(dir.path());

        assert!(dir.path().is_dir(), "clearing the compression flag must leave the directory alone");
    }

    #[test]
    fn a_directory_that_cannot_be_opened_is_ignored() {
        let dir = TempDir::new().expect("creating a temporary directory");
        let missing = dir.path().join("no-such-directory");

        // `CreateFileW` is called with `OPEN_EXISTING`, so a path that was never created makes it
        // fail and the function bails out silently.
        disable_directory_compression(&missing);

        assert!(!missing.exists(), "the directory must not be created as a side effect");
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod portable_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use chrono::Utc;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use semver::Version;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{Collector, create_cache_dir};
    use crate::facts::cache::Cache;
    use crate::facts::crates::{CrateOverallData, CrateVersionData};
    use crate::facts::progress::Progress;
    use crate::facts::{BugLabelMatcher, CrateSpec, CratesData, Endpoints, ProviderResult};

    #[derive(Debug)]
    struct NoOpProgress;

    impl Progress for NoOpProgress {
        fn set_phase(&self, _phase: &str) {}
        fn set_determinate(&self, _callback: Box<dyn Fn() -> (u64, u64, String) + Send + Sync + 'static>) {}
        fn set_indeterminate(&self, _callback: Box<dyn Fn() -> String + Send + Sync + 'static>) {}
        fn println(&self, _msg: &str) {}
        fn done(&self) {}
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn create_cache_dir_creates_and_returns_named_child() {
        let dir = tempfile::tempdir().expect("creating a temporary directory");

        let created = create_cache_dir(dir.path(), "crates").expect("creating the cache child succeeds");

        assert_eq!(created, dir.path().join("crates"));
        assert!(created.is_dir());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    fn create_cache_dir_reports_directory_creation_errors() {
        let dir = tempfile::tempdir().expect("creating a temporary directory");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("creating blocker file");

        let err = create_cache_dir(&blocker, "nested").expect_err("a file cannot contain a cache directory");

        assert!(err.to_string().contains("creating `nested` cache directory"), "{err}");
    }

    fn csv(headers: &[&str], rows: Vec<Vec<String>>) -> String {
        let mut writer = csv::Writer::from_writer(Vec::new());
        writer
            .write_record(headers)
            .expect("writing headers to an in-memory CSV must not fail");
        for row in rows {
            writer.write_record(row).expect("writing a row to an in-memory CSV must not fail");
        }

        String::from_utf8(writer.into_inner().expect("flushing an in-memory CSV must not fail"))
            .expect("CSV data written by the test is valid UTF-8")
    }

    fn tar_gz(files: &[(&str, String)]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);

        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();

            builder
                .append_data(&mut header, format!("2026-01-15-020017/data/{name}"), contents.as_bytes())
                .expect("writing to an in-memory tar must not fail");
        }

        builder
            .into_inner()
            .expect("finishing an in-memory tar must not fail")
            .finish()
            .expect("finishing an in-memory gzip stream must not fail")
    }

    fn minimal_dump() -> Vec<u8> {
        tar_gz(&[
            (
                "crates.csv",
                csv(
                    &[
                        "id",
                        "name",
                        "created_at",
                        "updated_at",
                        "description",
                        "homepage",
                        "documentation",
                        "repository",
                        "readme",
                        "max_upload_size",
                    ],
                    Vec::new(),
                ),
            ),
            (
                "versions.csv",
                csv(
                    &[
                        "id",
                        "crate_id",
                        "num",
                        "created_at",
                        "updated_at",
                        "downloads",
                        "features",
                        "yanked",
                        "license",
                        "crate_size",
                        "checksum",
                        "links",
                        "rust_version",
                        "has_lib",
                        "bin_names",
                        "edition",
                        "description",
                        "homepage",
                        "documentation",
                    ],
                    Vec::new(),
                ),
            ),
            ("version_downloads.csv", csv(&["version_id", "downloads", "date"], Vec::new())),
            (
                "dependencies.csv",
                csv(
                    &[
                        "id",
                        "version_id",
                        "crate_id",
                        "req",
                        "optional",
                        "default_features",
                        "features",
                        "target",
                        "kind",
                        "explicit_name",
                    ],
                    Vec::new(),
                ),
            ),
            ("crate_downloads.csv", csv(&["crate_id", "downloads"], Vec::new())),
            ("crates_categories.csv", csv(&["crate_id", "category_id"], Vec::new())),
            ("crates_keywords.csv", csv(&["crate_id", "keyword_id"], Vec::new())),
            (
                "categories.csv",
                csv(
                    &["id", "category", "slug", "description", "crates_cnt", "created_at", "path"],
                    Vec::new(),
                ),
            ),
            ("keywords.csv", csv(&["id", "keyword", "crates_cnt", "created_at"], Vec::new())),
            (
                "teams.csv",
                csv(&["id", "login", "github_id", "name", "avatar", "org_id"], Vec::new()),
            ),
            ("users.csv", csv(&["id", "gh_login", "name", "gh_avatar", "gh_id"], Vec::new())),
            (
                "crate_owners.csv",
                csv(&["crate_id", "owner_id", "created_at", "created_by", "owner_kind"], Vec::new()),
            ),
        ])
    }

    fn write_advisory(root: &std::path::Path, package: &str, id: &str) {
        let dir = root.join("crates").join(package);
        std::fs::create_dir_all(&dir).expect("creating advisory fixture directory");
        std::fs::write(
            dir.join(format!("{id}.md")),
            format!(
                "```toml\n\
                 [advisory]\n\
                 id = \"{id}\"\n\
                 package = \"{package}\"\n\
                 date = \"2020-01-01\"\n\
                 informational = \"unmaintained\"\n\
                 \n\
                 [versions]\n\
                 patched = []\n\
                 ```\n\
                 \n\
                 # advisory\n\
                 \n\
                 Body.\n"
            ),
        )
        .expect("writing advisory fixture");
    }

    fn found_crates_data() -> CratesData {
        let now = Utc::now();
        CratesData::new(
            CrateVersionData {
                description: "fixture".into(),
                homepage: None,
                documentation: None,
                license: "MIT".into(),
                rust_version: "1.70.0".into(),
                edition: None,
                features: BTreeMap::new(),
                created_at: now,
                updated_at: now,
                yanked: false,
                downloads: 1,
                monthly_downloads: Vec::new(),
            },
            CrateOverallData {
                created_at: now,
                updated_at: now,
                repository: None,
                categories: Vec::new(),
                keywords: Vec::new(),
                owners: Vec::new(),
                monthly_downloads: Vec::new(),
                downloads: 1,
                dependents: 0,
                versions_last_90_days: 0,
                versions_last_180_days: 0,
                versions_last_365_days: 0,
            },
        )
    }

    fn is_not_queried<T>(result: &ProviderResult<T>) -> bool {
        matches!(result, ProviderResult::Unavailable(reason) if reason == "not queried")
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot run a Tokio reactor")]
    async fn query_providers_runs_for_found_crates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/db-dump.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(minimal_dump()))
            .mount(&server)
            .await;

        let cache_dir = tempfile::tempdir().expect("creating collector cache directory");
        let advisories_dir = cache_dir.path().join("advisories");
        write_advisory(&advisories_dir.join("repo"), "queried-crate", "RUSTSEC-2020-0200");
        Cache::new(&advisories_dir, core::time::Duration::from_hours(1), false)
            .save("last_synced.bin", &())
            .expect("writing advisory sync marker");

        let server_uri = server.uri();
        let dump_url = format!("{server_uri}/db-dump.tar.gz");
        let endpoints = Endpoints::default()
            .with_dump_url(&dump_url)
            .with_docs_url(&server_uri)
            .with_coverage_url(&server_uri)
            .with_github_url(&server_uri)
            .with_codeberg_url(&server_uri)
            .with_advisory_url("https://example.invalid/advisories.git");
        assert_eq!(endpoints.dump_url(), dump_url);
        assert_eq!(endpoints.docs_url(), server_uri);
        assert_eq!(endpoints.coverage_url(), server_uri);
        assert_eq!(endpoints.host_url("github.com"), Some(server_uri.as_str()));
        assert_eq!(endpoints.host_url("codeberg.org"), Some(server_uri.as_str()));
        assert_eq!(endpoints.advisory_url(), "https://example.invalid/advisories.git");
        let collector = Collector::new(
            None,
            None,
            cache_dir.path(),
            core::time::Duration::from_hours(1),
            core::time::Duration::from_hours(1),
            core::time::Duration::from_hours(1),
            core::time::Duration::from_hours(1),
            core::time::Duration::from_hours(1),
            false,
            Arc::new(BugLabelMatcher::default()),
            NoOpProgress,
            &endpoints,
        )
        .await
        .expect("collector should initialize from local fixtures");

        let spec = CrateSpec::from_arcs(Arc::from("queried-crate"), Arc::new(Version::new(1, 0, 0)));
        let facts = collector
            .query_providers(vec![(spec, ProviderResult::Found(found_crates_data()))])
            .await
            .pop()
            .expect("one fact is returned for the one input crate");

        assert!(!is_not_queried(&facts.advisory_data), "advisory provider must run for found crates");
        assert!(!is_not_queried(&facts.docs_data), "docs provider must run for found crates");
        assert!(
            is_not_queried(&facts.hosting_data),
            "without a repository, hosting has no result to update"
        );
        assert!(
            is_not_queried(&facts.codebase_data),
            "without a repository, codebase has no result to update"
        );
        assert!(
            is_not_queried(&facts.coverage_data),
            "without a repository, coverage has no result to update"
        );
    }
}
