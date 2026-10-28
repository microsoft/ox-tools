// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

#[cfg(feature = "internals")]
pub fn retain_prepared_locks(workspace: Workspace, workspace_lock: File, cache_lock: Option<File>) -> Workspace {
    Workspace::retain_prepared_locks(workspace, workspace_lock, cache_lock)
}

#[cfg(feature = "internals")]
pub fn retained_cache_locks(workspace: &Workspace) -> (&Option<File>, &Option<File>) {
    let Workspace {
        _workspace_lock: workspace_lock,
        _cache_lock: cache_lock,
        ..
    } = workspace;
    (workspace_lock, cache_lock)
}

#[cfg(feature = "internals")]
pub fn retain_cache_locks(workspace: &mut Workspace, workspace_lock: File, cache_lock: Option<File>) {
    workspace.retain_cache_locks(workspace_lock, cache_lock);
}

#[cfg(feature = "internals")]
pub fn publish_cache_owner(
    source: &Utf8Path,
    base: &Utf8Path,
    inspect: impl FnOnce(&File) -> io::Result<fs::Metadata>,
    inventory: impl FnOnce(&Utf8Path) -> Result<bool>,
    write_owner: impl FnOnce(&mut File, &Utf8Path, &Utf8Path) -> Result<()>,
) -> Result<()> {
    validate_cache_owner_with_writer(source, base, CacheKind::Redirected, inspect, inventory, write_owner)
}

#[cfg(feature = "internals")]
pub fn anchor_native_manifest_paths(
    source: &Utf8Path,
    root: &Utf8Path,
    runtime: &Utf8Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    anchor_manifest_paths(source, root, runtime, paths)
}

#[cfg(feature = "internals")]
pub fn anchor_native_manifests(source: &Utf8Path, root: &Utf8Path, runtime: &Utf8Path) -> Result<()> {
    anchor_manifests(source, root, runtime)
}

#[cfg(feature = "internals")]
pub fn anchor_native_cargo_config(root: &Utf8Path, source: &Utf8Path) -> Result<()> {
    anchor_cargo_config(root, source)
}

#[cfg(feature = "internals")]
pub fn with_workspace_resolver<T>(resolve: impl FnMut(&Utf8Path) -> Utf8PathBuf + 'static, body: impl FnOnce() -> T) -> T {
    struct Reset(Option<WorkspaceResolver>);
    impl Drop for Reset {
        fn drop(&mut self) {
            WORKSPACE_RESOLVER.with_borrow_mut(|slot| *slot = self.0.take());
        }
    }
    let _reset = Reset(WORKSPACE_RESOLVER.with_borrow_mut(|slot| slot.replace(Box::new(resolve))));
    body()
}

#[cfg(feature = "internals")]
pub fn with_cache_metadata<T>(inspect: impl FnMut(&Utf8Path) -> io::Result<fs::Metadata> + 'static, body: impl FnOnce() -> T) -> T {
    struct Reset(Option<CacheMetadataProvider>);
    impl Drop for Reset {
        fn drop(&mut self) {
            CACHE_METADATA.with_borrow_mut(|slot| *slot = self.0.take());
        }
    }
    let _reset = Reset(CACHE_METADATA.with_borrow_mut(|slot| slot.replace(Box::new(inspect))));
    body()
}

#[cfg(feature = "internals")]
pub fn claim_native_redirected_cache(
    source: &Utf8Path,
    base: &Utf8Path,
    inspect: impl FnOnce(&Utf8Path) -> io::Result<fs::Metadata>,
    inventory: impl FnOnce(&Utf8Path) -> Result<bool>,
) -> Result<File> {
    claim_redirected_cache_with(source, base, inspect, inventory)
}

#[cfg(feature = "internals")]
pub fn claim_native_state_with(
    testing: bool,
    selected: &Utf8Path,
    cache: Option<&Utf8Path>,
    resolve: impl FnMut(&Utf8Path, Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf)>,
) -> Result<(Utf8PathBuf, Utf8PathBuf, File)> {
    if testing {
        claim_testing_state_with(selected, cache, resolve)
    } else {
        claim_campaign_state_with(selected, cache, resolve)
    }
}

#[cfg(feature = "internals")]
pub fn reject_native_linked_cache(path: &Utf8Path) -> Result<()> {
    reject_linked_cache(path)
}

#[cfg(feature = "internals")]
pub fn prepare_native_workspace(
    source: &Utf8Path,
    target: &Utf8Path,
    config: &Config,
    events: &mut impl Events,
    locks: Option<(File, Option<File>, usize)>,
) -> Result<Workspace> {
    Workspace::prepare_with_locks(
        source,
        target,
        config,
        events,
        locks.map(|(workspace, redirected, identity)| CacheLocks {
            workspace,
            redirected,
            identity,
        }),
    )
}

#[cfg(feature = "internals")]
pub fn physical_workspace(path: &Utf8Path) -> Utf8PathBuf {
    physical(path)
}

#[cfg(feature = "internals")]
pub fn native_gamma_base(root: &Utf8Path) -> Utf8PathBuf {
    gamma_base(root, None)
}

#[cfg(feature = "internals")]
pub fn workspace_digest(root: &Utf8Path) -> String {
    digest_workspace_path(root)
}

#[cfg(feature = "internals")]
pub fn require_native_manifest_within(path: &Utf8Path, root: &Utf8Path) -> Result<Utf8PathBuf> {
    crate::paths::require_within(path, root, "a scratch manifest")
}

pub(crate) fn mark_cache_owned_for_test(root: &Utf8Path, base: &Utf8Path) {
    fs::write(base.join(CACHE_OWNER), physical(root).as_str()).expect("test cache owner");
    assert!(cache_owner_root(base).is_some(), "test cache owner must resolve");
}

pub(crate) fn create_private_dir_all_for_test(path: &Utf8Path) {
    create_private_dir_all(path).expect("the private test directory should be creatable");
}

impl Workspace {
    #[cfg(feature = "internals")]
    #[doc(hidden)]
    pub fn test_set_environment(&mut self, package: &str, name: &str, value: OsString) {
        self.test_environment = Some(
            serde_json::from_value(serde_json::json!({
                "common": [],
                "packages": {
                    package: {
                        "variables": [(OsString::from(name), value)],
                        "binary_executables": []
                    }
                },
                "integration_tests": []
            }))
            .expect("the fixture encodes the complete TestEnvironment schema with lossless native-string pairs"),
        );
    }

    #[cfg(feature = "internals")]
    #[doc(hidden)]
    pub fn test_snapshot_with_native_values(&mut self, loader: OsString, stack: String) -> Result<serde_json::Value> {
        self.launch = OnceLock::from(Launch {
            loader: Some(loader),
            stack,
        });
        self.testing_snapshot()
    }

    pub(in crate::exec) fn set_rustc_captures(&mut self, captures: Option<Utf8PathBuf>) {
        // Exclusive access prevents readers; this store does not publish the following assignment.
        self.rustc_captures_ready.store(captures.is_some(), Ordering::Release);
        self.rustc_captures = captures;
    }
}
