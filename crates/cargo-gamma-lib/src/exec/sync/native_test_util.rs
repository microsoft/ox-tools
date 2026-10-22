// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use camino::Utf8Path;

use crate::Result;

pub fn collect_source_entries(
    source: &Utf8Path,
    skip: &Utf8Path,
    options: CopyOptions,
) -> Result<std::collections::HashSet<camino::Utf8PathBuf>> {
    with_untracked_source(source, || super::collect_source_entries(source, skip, options))
}

pub fn sync_or_copy(source: &Utf8Path, root: &Utf8Path, skip: &Utf8Path, options: CopyOptions) -> Result<SyncOutcome> {
    with_untracked_source(source, || super::sync_or_copy(source, root, skip, options))
}

pub fn delta_sync(source: &Utf8Path, root: &Utf8Path, skip: &Utf8Path, options: CopyOptions, reflinks: &Reflinks) -> Result<()> {
    with_untracked_source(source, || super::delta_sync(source, root, skip, options, reflinks))
}

pub fn sync_or_copy_with_delta(
    source: &Utf8Path,
    root: &Utf8Path,
    skip: &Utf8Path,
    options: CopyOptions,
    synchronize: impl FnOnce(&Utf8Path, &Utf8Path, &Utf8Path, CopyOptions, &Reflinks) -> Result<()>,
) -> Result<SyncOutcome> {
    with_untracked_source(source, || super::sync_or_copy_with_delta(source, root, skip, options, synchronize))
}

pub fn sync_or_copy_with_cleanup(
    source: &Utf8Path,
    root: &Utf8Path,
    skip: &Utf8Path,
    options: CopyOptions,
    synchronize: impl FnOnce(&Utf8Path, &Utf8Path, &Utf8Path, CopyOptions, &Reflinks) -> Result<()>,
    clear: impl FnOnce(&Utf8Path) -> std::io::Result<()>,
) -> Result<SyncOutcome> {
    with_untracked_source(source, || {
        super::sync_or_copy_with_cleanup(source, root, skip, options, synchronize, clear)
    })
}
use camino::Utf8PathBuf;

#[doc(hidden)]
pub use super::super::copy::{CopyOptions, Reflinks};
#[doc(hidden)]
pub use super::{
    SyncOutcome, collect_source_entries_with, copy_file_for_sync, copy_file_for_sync_with, copy_file_for_sync_with_stamp, file_differs,
    is_consistent, link_target_differs, mark_consistent, metadata_is_directory, metadata_is_symlink, record_invalid_path,
    record_walk_failure, remove_stale, remove_stale_with_directory, sentinel, source_link_targets_directory, stamp_mtime, sync_entry,
    sync_file, sync_or_copy_using, sync_symlink, sync_symlink_with, tracked_path_allowed,
};

type TrackedReader = Box<dyn FnMut(&Utf8Path) -> Result<Option<Vec<Utf8PathBuf>>>>;

std::thread_local! {
    pub(in crate::exec) static SCRIPTED_TRACKED: std::cell::RefCell<Option<TrackedReader>> = const { std::cell::RefCell::new(None) };
}

#[cfg_attr(coverage_nightly, coverage(off))]
pub(in crate::exec) fn with_untracked_source<T>(source: &Utf8Path, operation: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCRIPTED_TRACKED.with_borrow_mut(|reader| *reader = None);
        }
    }
    let source = source.to_owned();
    SCRIPTED_TRACKED.with_borrow_mut(|reader| {
        assert!(reader.is_none(), "tracked inventory scripts must not nest");
        *reader = Some(Box::new(move |root| {
            assert_eq!(root, source, "only the scenario's source tree may request an inventory");
            Ok(None)
        }));
    });
    let _reset = Reset;
    operation()
}

#[cfg(windows)]
pub fn junction_for_test(link: &Utf8Path, target: &Utf8Path) {
    let link = super::super::workspace::absolute(link).as_str().replace('/', "\\");
    let target = super::super::workspace::absolute(target).as_str().replace('/', "\\");
    let output = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J", &link, &target])
        .output()
        .expect("the native Windows regression fixture requires the standard command interpreter");
    assert!(output.status.success(), "junction creation failed: {output:?}");
}
