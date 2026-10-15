// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Delta synchronization of a scratch tree against the source workspace.
//!
//! Instead of deleting and recopying the entire tree on each campaign, this module compares the
//! existing scratch tree against the source and applies only the necessary changes: new files are
//! copied, changed files are replaced, stale files are removed, and unchanged files are left in
//! place with their original mtimes — preserving Cargo's fingerprint validity for inputs that did
//! not change.

use std::collections::HashSet;
use std::fs::{self, File, FileTimes};
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::sync::Mutex;
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::{WalkBuilder, WalkState};
use walkdir::WalkDir;

use crate::Result;
use crate::error::{Error, error};
use crate::exec::copy::{CopyOptions, Reflinks, copy_tree_with, is_pruned, tracked_files};

/// Outcome of a delta synchronization attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncOutcome {
    /// The existing tree was updated in place.
    Synchronized,
    /// The existing tree was unsuitable and a full copy was performed instead.
    FreshCopy,
}

const SYNC_SENTINEL: &str = ".gamma-sync-ok";

fn sentinel(root: &Utf8Path) -> Utf8PathBuf {
    root.parent().unwrap_or(root).join(SYNC_SENTINEL)
}

pub(super) fn mark_consistent(root: &Utf8Path) {
    let path = sentinel(root);
    let _written = fs::write(path.as_std_path(), "1");
}

fn is_consistent(root: &Utf8Path) -> bool {
    let path = sentinel(root);
    matches!(fs::read(path.as_std_path()), Ok(content) if content == b"1")
}

fn clear_sentinel(root: &Utf8Path) {
    let path = sentinel(root);
    let _removed = fs::remove_file(path.as_std_path());
}

/// Attempts to delta-synchronize `source` into `existing_root`.
///
/// If the existing tree is unsuitable (inconsistent from a prior interrupted run, or not a
/// directory), falls back to removing it and performing a fresh copy.
///
/// Returns which path was taken so the caller can emit appropriate diagnostics.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn sync_or_copy(source: &Utf8Path, root: &Utf8Path, skip: &Utf8Path, options: CopyOptions) -> Result<SyncOutcome> {
    sync_or_copy_with_delta(source, root, skip, options, delta_sync)
}

fn sync_or_copy_with_delta(
    source: &Utf8Path,
    root: &Utf8Path,
    skip: &Utf8Path,
    options: CopyOptions,
    synchronize: impl FnOnce(&Utf8Path, &Utf8Path, &Utf8Path, CopyOptions, &Reflinks) -> Result<()>,
) -> Result<SyncOutcome> {
    // Taken once for the whole operation and shared by both the delta path and the fresh copy it
    // may fall back to: they write to the same tree, so what one of them learns about cloning there
    // is exactly what the other needs to know.
    let reflinks = Reflinks::new();

    if !root.as_std_path().is_dir() {
        copy_tree_with(source, root, skip, options, &reflinks)?;
        mark_consistent(root);
        return Ok(SyncOutcome::FreshCopy);
    }

    if !is_consistent(root) {
        // Prior run was interrupted — cannot trust what is there. Remove and resync.
        fs::remove_dir_all(root.as_std_path())
            .map_err(|cause| error!("could not clear the inconsistent scratch tree at `{root}`").caused_by(cause))?;
        copy_tree_with(source, root, skip, options, &reflinks)?;
        mark_consistent(root);
        return Ok(SyncOutcome::FreshCopy);
    }

    // The tree looks consistent — attempt delta sync.
    clear_sentinel(root);

    match synchronize(source, root, skip, options, &reflinks) {
        Ok(()) => {
            mark_consistent(root);
            Ok(SyncOutcome::Synchronized)
        }
        Err(_cause) => {
            // Delta sync failed. Remove everything and do a clean sync to restore correctness.
            let _removed = fs::remove_dir_all(root.as_std_path());
            copy_tree_with(source, root, skip, options, &reflinks)?;
            mark_consistent(root);
            Ok(SyncOutcome::FreshCopy)
        }
    }
}

/// Performs the actual delta synchronization.
///
/// Walks the source tree (with the same ignore/selection semantics as the full copy) to discover
/// what should be in the scratch tree, then:
/// 1. Copies new entries and replaces changed entries.
/// 2. Removes stale entries that no longer exist in the source.
/// 3. Leaves unchanged entries untouched (preserving their mtimes for Cargo).
fn delta_sync(source: &Utf8Path, root: &Utf8Path, skip: &Utf8Path, options: CopyOptions, reflinks: &Reflinks) -> Result<()> {
    // Collect the set of relative paths the source tree produces.
    let expected = collect_source_entries(source, skip, options)?;

    // Synchronize parents before their descendants so type mismatches cannot redirect writes.
    let mut ordered: Vec<_> = expected.iter().collect();
    ordered.sort_unstable_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    for relative in ordered {
        let src = source.join(relative);
        let dst = root.join(relative);
        sync_entry(&src, &dst, reflinks)?;
    }

    // Remove stale entries from the scratch tree.
    remove_stale(root, &expected)?;

    Ok(())
}

/// Collects relative paths from the source tree using the same walk logic as `copy_tree_with`.
#[cfg_attr(coverage_nightly, coverage(off))]
fn collect_source_entries(source: &Utf8Path, skip: &Utf8Path, options: CopyOptions) -> Result<HashSet<Utf8PathBuf>> {
    collect_source_entries_with(source, skip, options, tracked_files)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkSetting {
    Hidden,
    Parents,
    RequireGit,
    GitIgnore,
    GitExclude,
    GitGlobal,
    Ignore,
    FollowLinks,
}

fn configure_source_walk(options: CopyOptions, mut configure: impl FnMut(WalkSetting, bool)) {
    for (setting, enabled) in [
        (WalkSetting::Hidden, false),
        (WalkSetting::Parents, false),
        (WalkSetting::RequireGit, true),
        (WalkSetting::GitIgnore, !options.copy_ignored),
        (WalkSetting::GitExclude, !options.copy_ignored),
        (WalkSetting::GitGlobal, false),
        (WalkSetting::Ignore, false),
        (WalkSetting::FollowLinks, false),
    ] {
        configure(setting, enabled);
    }
}

fn collect_source_entries_with(
    source: &Utf8Path,
    skip: &Utf8Path,
    options: CopyOptions,
    tracked: impl FnOnce(&Utf8Path) -> Result<Option<Vec<Utf8PathBuf>>>,
) -> Result<HashSet<Utf8PathBuf>> {
    let entries: Mutex<HashSet<Utf8PathBuf>> = Mutex::new(HashSet::new());
    let failure: Mutex<Option<Error>> = Mutex::new(None);

    let mut builder = WalkBuilder::new(source.as_std_path());
    configure_source_walk(options, |setting, enabled| {
        let _builder = match setting {
            WalkSetting::Hidden => builder.hidden(enabled),
            WalkSetting::Parents => builder.parents(enabled),
            WalkSetting::RequireGit => builder.require_git(enabled),
            WalkSetting::GitIgnore => builder.git_ignore(enabled),
            WalkSetting::GitExclude => builder.git_exclude(enabled),
            WalkSetting::GitGlobal => builder.git_global(enabled),
            WalkSetting::Ignore => builder.ignore(enabled),
            WalkSetting::FollowLinks => builder.follow_links(enabled),
        };
    });

    let root = source.to_owned();
    let excluded = skip.to_owned();

    builder.build_parallel().run(|| {
        let root = root.clone();
        let excluded = excluded.clone();
        let entries = &entries;
        let failure = &failure;

        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(cause) => {
                    record_walk_failure(failure, cause);
                    return WalkState::Quit;
                }
            };

            let Some(path) = Utf8Path::from_path(entry.path()) else {
                record_invalid_path(failure, entry.path());
                return WalkState::Quit;
            };

            let Ok(relative) = path.strip_prefix(&root) else {
                return WalkState::Continue;
            };

            if is_walk_root(relative) {
                return WalkState::Continue;
            }

            if pruned_entry(path, relative, &excluded) {
                return WalkState::Skip;
            }

            let mut set = entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _inserted = set.insert(relative.to_owned());

            WalkState::Continue
        })
    });

    if let Some(cause) = failure.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        return collection_failure(cause);
    }

    let mut result = entries.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);

    // Include tracked files that the ignore walk may have skipped, same as copy_tracked does.
    if let Some(tracked) = tracked(source)? {
        for relative in tracked {
            if tracked_path_allowed(source, &relative, skip) {
                let src = source.join(&relative);
                if source_entry_exists(&src) {
                    let _inserted = result.insert(relative);
                }
            }
        }
    }

    Ok(result)
}

/// Returns whether any directory on the way to `relative` is one the sync leaves out.
fn is_pruned_anywhere(root: &Utf8Path, relative: &Utf8Path, excluded: &Utf8Path) -> bool {
    let mut prefix = Utf8PathBuf::new();
    for component in relative.components() {
        prefix.push(component);
        if is_pruned(&root.join(&prefix), &prefix, excluded) {
            return true;
        }
    }
    false
}

/// Synchronizes one source entry to the scratch tree.
///
/// For files: copies if new or if length, permissions, or contents changed. Unchanged files are
/// left in place so their modification times continue to preserve Cargo fingerprints.
/// For directories: creates if missing.
/// For symlinks: recreates if target differs.
#[cfg_attr(coverage_nightly, coverage(off))]
fn sync_entry(source: &Utf8Path, destination: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    let src_meta = fs::symlink_metadata(source.as_std_path()).map_err(|cause| error!("could not read `{source}`").caused_by(cause))?;

    if src_meta.is_dir() {
        match fs::symlink_metadata(destination.as_std_path()) {
            Ok(dst_meta) if dst_meta.is_dir() => return Ok(()),
            Ok(_) => {
                // A symlink or other non-directory where a directory should be — replace it.
                remove_stale_entry(destination)
                    .map_err(|cause| error!("could not remove stale entry at `{destination}`").caused_by(cause))?;
            }
            Err(cause) if is_not_found(&cause) => {}
            Err(cause) => return Err(error!("could not inspect `{destination}`").caused_by(cause)),
        }
        fs::create_dir_all(destination.as_std_path()).map_err(|cause| error!("could not create `{destination}`").caused_by(cause))?;
        return Ok(());
    }

    if src_meta.is_symlink() {
        return sync_symlink(source, destination);
    }

    // Regular file.
    sync_file(source, destination, &src_meta, reflinks)
}

/// Synchronizes a regular file, preserving mtime for unchanged files.
#[cfg_attr(coverage_nightly, coverage(off))]
fn sync_file(source: &Utf8Path, destination: &Utf8Path, src_meta: &fs::Metadata, reflinks: &Reflinks) -> Result<()> {
    let needs_copy = match fs::symlink_metadata(destination.as_std_path()) {
        Err(_) => true, // Destination does not exist.
        Ok(dst_meta) => {
            if dst_meta.is_symlink() || dst_meta.is_dir() {
                // Type mismatch — remove and recopy.
                if metadata_is_directory(&dst_meta) {
                    fs::remove_dir_all(destination.as_std_path())
                        .map_err(|cause| error!("could not remove stale directory at `{destination}`").caused_by(cause))?;
                } else {
                    fs::remove_file(destination.as_std_path())
                        .map_err(|cause| error!("could not remove stale entry at `{destination}`").caused_by(cause))?;
                }
                true
            } else {
                file_differs(source, destination, src_meta, &dst_meta)?
            }
        }
    };

    if needs_copy {
        // Ensure parent exists.
        if let Some(parent) = destination.parent()
            && missing_directory(parent)
        {
            fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
        }

        // Remove existing destination before copying (reflink requires no existing file).
        let _removed = fs::remove_file(destination.as_std_path());
        copy_file_for_sync(source, destination, reflinks)?;
    }
    // Unchanged files are left in place — their mtime stays as it was, preserving Cargo
    // fingerprints.

    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn file_differs(source: &Utf8Path, destination: &Utf8Path, src: &fs::Metadata, dst: &fs::Metadata) -> Result<bool> {
    if different_lengths(src, dst) {
        return Ok(true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if src.mode() != dst.mode() {
            return Ok(true);
        }
    }

    if src.permissions().readonly() != dst.permissions().readonly() {
        return Ok(true);
    }

    same_contents(source, destination).map(|same| !same)
}

fn same_contents(left: &Utf8Path, right: &Utf8Path) -> Result<bool> {
    let left = File::open(left.as_std_path()).map_err(|cause| error!("could not read `{left}`").caused_by(cause))?;
    let right = File::open(right.as_std_path()).map_err(|cause| error!("could not read `{right}`").caused_by(cause))?;

    readers_have_same_contents(left, right).map_err(|cause| error!("could not compare scratch input").caused_by(cause))
}

fn readers_have_same_contents(left: impl Read, right: impl Read) -> std::io::Result<bool> {
    // #[gamma::skip(all, reason = "buffer capacity changes only how identical byte streams are chunked; fill_buf/consume explicitly aligns independent chunk boundaries")]
    let mut left = BufReader::with_capacity(16 * 1024, left);
    // #[gamma::skip(all, reason = "buffer capacity changes only how identical byte streams are chunked; fill_buf/consume explicitly aligns independent chunk boundaries")]
    let mut right = BufReader::with_capacity(16 * 1024, right);

    loop {
        let (compared, left_eof, right_eof, equal) = {
            let left_buffer = left.fill_buf()?;
            let right_buffer = right.fill_buf()?;
            let compared = left_buffer.len().min(right_buffer.len());
            (
                compared,
                left_buffer.is_empty(),
                right_buffer.is_empty(),
                left_buffer[..compared] == right_buffer[..compared],
            )
        };

        // #[gamma::skip(logical.or_remove_right, tag = "timeout", reason = "the campaign comparison stalled when unequal end-of-file states no longer ended the scan")]
        if !equal || left_eof != right_eof {
            return Ok(false);
        }
        // #[gamma::skip(cond.always_false, tag = "timeout", reason = "the comparison must stop once both files reach end of input")]
        if left_eof {
            return Ok(true);
        }

        left.consume(compared);
        right.consume(compared);
    }
}

/// Synchronizes a symlink. Recreates it if the target changed or if the destination is not a link.
#[cfg_attr(coverage_nightly, coverage(off))]
fn sync_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    sync_symlink_with(source, destination, |target, destination, directory| {
        #[cfg(unix)]
        {
            let _ = directory;
            std::os::unix::fs::symlink(target, destination)
        }
        #[cfg(windows)]
        {
            if directory {
                std::os::windows::fs::symlink_dir(target, destination)
            } else {
                std::os::windows::fs::symlink_file(target, destination)
            }
        }
    })
}

fn sync_symlink_with(
    source: &Utf8Path,
    destination: &Utf8Path,
    create: impl FnOnce(&std::path::Path, &std::path::Path, bool) -> std::io::Result<()>,
) -> Result<()> {
    let src_target = fs::read_link(source.as_std_path()).map_err(|cause| error!("could not read the link `{source}`").caused_by(cause))?;

    let needs_recreate = match fs::symlink_metadata(destination.as_std_path()) {
        Err(_) => true,
        Ok(dst_meta) => {
            if metadata_is_symlink(&dst_meta) {
                // Both are symlinks — compare targets.
                link_target_differs(destination, &src_target)
            } else {
                // Type mismatch — remove what is there.
                if metadata_is_directory(&dst_meta) {
                    fs::remove_dir_all(destination.as_std_path())
                        .map_err(|cause| error!("could not remove stale directory at `{destination}`").caused_by(cause))?;
                } else {
                    fs::remove_file(destination.as_std_path())
                        .map_err(|cause| error!("could not remove stale entry at `{destination}`").caused_by(cause))?;
                }
                true
            }
        }
    };

    if should_recreate_link(needs_recreate) {
        let _removed = fs::remove_file(destination.as_std_path());

        if let Some(parent) = destination.parent()
            && !parent.as_std_path().exists()
        {
            fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
        }

        #[cfg(windows)]
        let directory = source_link_targets_directory(source, &src_target);
        #[cfg(not(windows))]
        let directory = false;
        create(&src_target, destination.as_std_path(), directory)
            .map_err(|cause| error!("could not recreate the link `{destination}`").caused_by(cause))?;
    }

    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn copy_file_for_sync(source: &Utf8Path, destination: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    copy_file_for_sync_with(source, destination, reflinks, |source, destination| {
        reflink_copy::reflink(source, destination)
    })
}

fn copy_file_for_sync_with(
    source: &Utf8Path,
    destination: &Utf8Path,
    reflinks: &Reflinks,
    mut clone_file: impl FnMut(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> Result<()> {
    copy_file_for_sync_with_stamp(source, destination, reflinks, SystemTime::now(), &mut clone_file, stamp_mtime)
}

fn copy_file_for_sync_with_stamp(
    source: &Utf8Path,
    destination: &Utf8Path,
    reflinks: &Reflinks,
    copied_at: SystemTime,
    mut clone_file: impl FnMut(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
    mut stamp: impl FnMut(&Utf8Path, SystemTime) -> Result<()>,
) -> Result<()> {
    if should_try_reflink(reflinks) {
        match clone_file(source.as_std_path(), destination.as_std_path()) {
            Ok(()) => {
                stamp(destination, copied_at)?;
                return Ok(());
            }
            Err(cause) if is_not_found(&cause) => {
                return copy_failure(source, destination, cause);
            }
            Err(_unsupported) => {
                mark_reflinks_unsupported(reflinks);
                let _removed = fs::remove_file(destination.as_std_path());
            }
        }
    }

    let _bytes = fs::copy(source.as_std_path(), destination.as_std_path())
        .map_err(|cause| error!("could not copy `{source}` to `{destination}`").caused_by(cause))?;

    stamp(destination, copied_at)?;

    Ok(())
}

fn stamp_mtime(path: &Utf8Path, time: SystemTime) -> Result<()> {
    let file = File::options()
        .write(true)
        .open(path.as_std_path())
        .map_err(|cause| error!("could not open copied file `{path}`").caused_by(cause))?;
    file.set_times(FileTimes::new().set_modified(time))
        .map_err(|cause| error!("could not freshen copied file `{path}`").caused_by(cause))
}

/// Removes entries from the scratch tree that are not in the expected set.
///
/// Walks the scratch tree and removes anything not present in the source. Directories are handled
/// bottom-up: empty directories left after file removal are pruned.
#[cfg_attr(coverage_nightly, coverage(off))]
fn remove_stale(root: &Utf8Path, expected: &HashSet<Utf8PathBuf>) -> Result<()> {
    remove_stale_with_directory(root, expected, |path| fs::remove_dir(path.as_std_path()))
}

fn remove_stale_with_directory(
    root: &Utf8Path,
    expected: &HashSet<Utf8PathBuf>,
    mut remove_directory: impl FnMut(&Utf8Path) -> std::io::Result<()>,
) -> Result<()> {
    // Collect all entries in the scratch tree.
    let mut stale_files: Vec<Utf8PathBuf> = Vec::new();
    let mut stale_dirs: Vec<Utf8PathBuf> = Vec::new();

    'entries: for entry in WalkDir::new(root.as_std_path()).into_iter().filter_map(core::result::Result::ok) {
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue 'entries;
        };

        let Ok(relative) = path.strip_prefix(root) else {
            continue 'entries;
        };

        if is_walk_root(relative) {
            continue 'entries;
        }

        if !expected.contains(relative) {
            if walk_entry_is_directory(&entry) {
                stale_dirs.push(path.to_owned());
            } else {
                stale_files.push(path.to_owned());
            }
        }
    }

    // Remove stale files first.
    for file in &stale_files {
        fs::remove_file(file.as_std_path())
            .or_else(ignore_not_found)
            .map_err(|cause| error!("could not remove stale file `{file}`").caused_by(cause))?;
    }

    // Remove stale directories deepest-first so that parents are empty when reached.
    sort_stale_directories(&mut stale_dirs);
    for dir in &stale_dirs {
        // Only remove if truly empty (children may have been expected).
        match remove_directory(dir) {
            Ok(()) => {}
            Err(cause) if is_not_found(&cause) => {}
            // Not empty — some children were expected; leave it.
            Err(cause) if directory_has_expected_children(&cause) => {}
            Err(cause) => {
                return stale_directory_failure(dir, cause);
            }
        }
    }

    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn record_walk_failure(failure: &Mutex<Option<Error>>, cause: ignore::Error) {
    record(failure, error!("could not read the source tree").caused_by(cause));
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn record_invalid_path(failure: &Mutex<Option<Error>>, path: &std::path::Path) {
    record(
        failure,
        error!("`{}` is not valid UTF-8 and cannot be synchronized", path.display()),
    );
}

fn is_walk_root(relative: &Utf8Path) -> bool {
    relative.as_str().is_empty()
}

fn pruned_entry(path: &Utf8Path, relative: &Utf8Path, excluded: &Utf8Path) -> bool {
    is_pruned(path, relative, excluded)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn collection_failure<T>(cause: Error) -> Result<T> {
    Err(cause)
}

fn tracked_path_allowed(source: &Utf8Path, relative: &Utf8Path, skip: &Utf8Path) -> bool {
    !is_pruned_anywhere(source, relative, skip)
}

fn source_entry_exists(path: &Utf8Path) -> bool {
    fs::symlink_metadata(path.as_std_path()).is_ok()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn remove_stale_entry(path: &Utf8Path) -> std::io::Result<()> {
    match fs::remove_file(path.as_std_path()) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(_) => fs::remove_dir(path.as_std_path()),
        #[cfg(not(windows))]
        Err(cause) => Err(cause),
    }
}

fn metadata_is_directory(metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
}

fn metadata_is_symlink(metadata: &fs::Metadata) -> bool {
    metadata.is_symlink()
}

fn missing_directory(path: &Utf8Path) -> bool {
    !path.as_std_path().exists()
}

fn different_lengths(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() != right.len()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn link_target_differs(destination: &Utf8Path, source_target: &std::path::Path) -> bool {
    match fs::read_link(destination.as_std_path()) {
        Ok(target) => target != source_target,
        Err(_) => true,
    }
}

const fn should_recreate_link(needs_recreate: bool) -> bool {
    needs_recreate
}

#[cfg(any(windows, all(test, not(miri))))]
fn source_link_targets_directory(source: &Utf8Path, target: &std::path::Path) -> bool {
    source
        .parent()
        .map_or_else(|| target.is_dir(), |parent| parent.as_std_path().join(target).is_dir())
}

fn should_try_reflink(reflinks: &Reflinks) -> bool {
    reflinks.worth_trying()
}

fn is_not_found(cause: &std::io::Error) -> bool {
    cause.kind() == ErrorKind::NotFound
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn copy_failure<T>(source: &Utf8Path, destination: &Utf8Path, cause: std::io::Error) -> Result<T> {
    Err(error!("could not copy `{source}` to `{destination}`").caused_by(cause))
}

fn mark_reflinks_unsupported(reflinks: &Reflinks) {
    reflinks.unsupported();
}

fn ignore_not_found(cause: std::io::Error) -> std::io::Result<()> {
    if is_not_found(&cause) { Ok(()) } else { Err(cause) }
}

fn sort_stale_directories(paths: &mut [Utf8PathBuf]) {
    paths.sort_by_key(|path| core::cmp::Reverse(path.as_str().len()));
}

fn walk_entry_is_directory(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir()
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn stale_directory_failure<T>(dir: &Utf8Path, cause: std::io::Error) -> Result<T> {
    Err(error!("could not remove stale directory `{dir}`").caused_by(cause))
}

fn directory_has_expected_children(cause: &std::io::Error) -> bool {
    is_not_empty_error(cause)
}

/// Checks if an IO error indicates the directory is not empty.
fn is_not_empty_error(err: &std::io::Error) -> bool {
    // On Unix, ENOTEMPTY; on Windows, ERROR_DIR_NOT_EMPTY.
    err.kind() == ErrorKind::DirectoryNotEmpty
        || err.raw_os_error() == Some(39)  // ENOTEMPTY on Linux
        || err.raw_os_error() == Some(66) // ENOTEMPTY on macOS
}

/// Records the first failure.
fn record(failure: &Mutex<Option<Error>>, cause: Error) {
    if let Ok(mut held) = failure.lock()
        && held.is_none()
    {
        *held = Some(cause);
    }
}

#[cfg(all(test, windows))]
pub(super) fn junction_for_test(link: &Utf8Path, target: &Utf8Path) {
    let link = super::workspace::absolute(link).as_str().replace('/', "\\");
    let target = super::workspace::absolute(target).as_str().replace('/', "\\");
    let output = std::process::Command::new("cmd.exe")
        .args(["/c", "mklink", "/J", &link, &target])
        .output()
        .expect("the native Windows regression fixture requires the standard command interpreter");
    assert!(output.status.success(), "junction creation failed: {output:?}");
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use core::time::Duration;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::process::Command;
    use std::{io, thread};

    use super::*;

    #[test]
    fn link_inspection_rejects_a_regular_source_and_distinguishes_directory_and_file_targets() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("ordinary-file");
        let destination = root.join("untouched-destination");
        let target = root.join("target");
        fs::write(&source, "regular source").unwrap();
        let failure = sync_symlink(&source, &destination).expect_err("a native regular file has no link target");
        assert_eq!(failure.message(), format!("could not read the link `{source}`"));
        assert!(!destination.exists());
        assert!(link_target_differs(&source, std::path::Path::new("not-a-link")));
        fs::create_dir(&target).unwrap();
        assert!(source_link_targets_directory(&source, std::path::Path::new("target")));
        fs::remove_dir(&target).unwrap();
        fs::write(&target, "now a file").unwrap();
        assert!(!source_link_targets_directory(&source, std::path::Path::new("target")));
    }

    #[test]
    fn source_walk_disables_global_git_excludes_without_disabling_workspace_ignore_policy() {
        for copy_ignored in [false, true] {
            let mut settings = Vec::new();
            configure_source_walk(CopyOptions { copy_ignored }, |setting, enabled| settings.push((setting, enabled)));
            assert_eq!(
                settings,
                [
                    (WalkSetting::Hidden, false),
                    (WalkSetting::Parents, false),
                    (WalkSetting::RequireGit, true),
                    (WalkSetting::GitIgnore, !copy_ignored),
                    (WalkSetting::GitExclude, !copy_ignored),
                    (WalkSetting::GitGlobal, false),
                    (WalkSetting::Ignore, false),
                    (WalkSetting::FollowLinks, false),
                ]
            );
        }
    }

    #[test]
    fn source_collection_propagates_a_late_tracked_inventory_failure_after_the_native_walk() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        fs::write(root.join("walked"), "walked bytes").unwrap();
        let reached = std::cell::Cell::new(false);
        let failure = collect_source_entries_with(root, &root.join("excluded"), CopyOptions::default(), |source| {
            assert_eq!(source, root);
            assert_eq!(fs::read_to_string(source.join("walked")).unwrap(), "walked bytes");
            reached.set(true);
            Err(error!("late tracked inventory refusal"))
        })
        .expect_err("a successful native walk does not authorize hiding a later tracked-inventory failure");
        assert!(reached.get());
        assert_eq!(failure.message(), "late tracked inventory refusal");
        assert_eq!(fs::read_to_string(root.join("walked")).unwrap(), "walked bytes");
    }

    #[test]
    fn stale_directory_removal_tolerates_a_native_disappearance_after_files_have_already_been_removed() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let stale = root.join("stale");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("old-file"), "old bytes").unwrap();
        fs::write(root.join("kept"), "kept bytes").unwrap();
        let calls = std::cell::Cell::new(0);
        remove_stale_with_directory(root, &HashSet::from_iter([Utf8PathBuf::from("kept")]), |path| {
            assert_eq!(path, stale);
            assert!(
                !stale.join("old-file").exists(),
                "the real file-removal phase must precede directory removal"
            );
            assert!(path.is_dir());
            calls.set(calls.get() + 1);
            fs::remove_dir(path)?;
            fs::remove_dir(path)
        })
        .expect("an entry removed after the native walk is already absent, not a failed synchronization");
        assert_eq!(calls.get(), 1);
        assert!(!stale.exists());
        assert_eq!(fs::read_to_string(root.join("kept")).unwrap(), "kept bytes");
    }

    #[test]
    fn ordinary_copy_propagates_a_late_stamp_failure_after_placing_the_exact_source_bytes() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("source");
        let destination = root.join("destination");
        fs::write(&source, "native copied bytes").unwrap();
        let reflinks = Reflinks::new();
        reflinks.unsupported();
        let reached = std::cell::Cell::new(false);
        let failure = copy_file_for_sync_with_stamp(
            &source,
            &destination,
            &reflinks,
            SystemTime::UNIX_EPOCH,
            |_, _| panic!("a settled unsupported capability must select the actual fs::copy fallback"),
            |path, _time| {
                assert_eq!(path, destination);
                assert_eq!(fs::read_to_string(path).unwrap(), "native copied bytes");
                reached.set(true);
                Err(error!("late ordinary-copy stamp refusal"))
            },
        )
        .expect_err("the late fallback stamp result must propagate after genuine native copying");
        assert!(reached.get());
        assert_eq!(failure.message(), "late ordinary-copy stamp refusal");
        assert_eq!(fs::read_to_string(&source).unwrap(), "native copied bytes");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "native copied bytes");
    }

    #[test]
    #[cfg(windows)]
    fn the_file_sync_caller_propagates_native_copy_denial_instead_of_accepting_its_old_destination() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("source");
        let destination = root.join("destination");
        fs::write(&source, "new").unwrap();
        fs::write(&destination, "long old bytes").unwrap();
        let metadata = fs::metadata(&source).unwrap();
        let locked = File::options().read(true).share_mode(0).open(&destination).unwrap();
        let failure = sync_file(&source, &destination, &metadata, &Reflinks::new())
            .expect_err("different lengths require copying, and the native destination lock must refuse it");
        assert_eq!(failure.message(), format!("could not copy `{source}` to `{destination}`"));
        drop(locked);
        assert_eq!(fs::read_to_string(&source).unwrap(), "new");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "long old bytes");
    }

    #[test]
    #[cfg(windows)]
    fn file_sync_refuses_a_locked_native_link_before_following_or_overwriting_its_target() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("source");
        let target = root.join("external");
        let destination = root.join("destination-link");
        fs::write(&source, "new").unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), "external target bytes").unwrap();
        junction_for_test(&destination, &target);
        let metadata = fs::symlink_metadata(&destination).unwrap();
        assert!(metadata.is_symlink());
        assert!(!metadata_is_directory(&metadata));
        // Open the reparse point itself, not its target, with directory-handle support.
        let locked = File::options()
            .read(true)
            .share_mode(1)
            .custom_flags(0x0220_0000)
            .open(&destination)
            .unwrap();
        let failure = sync_file(&source, &destination, &fs::metadata(&source).unwrap(), &Reflinks::new())
            .expect_err("deletion-sharing denial must escape the link type-repair branch before native copying");
        assert_eq!(failure.message(), format!("could not remove stale entry at `{destination}`"));
        drop(locked);
        assert!(fs::symlink_metadata(&destination).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(target.join("sentinel")).unwrap(), "external target bytes");
    }

    #[test]
    #[cfg(windows)]
    fn native_junction_entries_are_retained_without_recursing_into_their_external_targets_or_inserting_the_walk_root() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("source");
        let target = root.join("external");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("external-child"), "external bytes").unwrap();
        junction_for_test(&source.join("link"), &target);
        let entries = collect_source_entries(&source, &source.join("excluded"), CopyOptions::default()).unwrap();
        assert_eq!(entries, HashSet::from_iter([Utf8PathBuf::from("link")]));
        assert_eq!(fs::read_to_string(target.join("external-child")).unwrap(), "external bytes");
    }

    #[test]
    #[cfg(windows)]
    fn matching_native_junctions_are_recognized_as_links_and_never_recreated_or_removed() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let target = root.join("target");
        let source = root.join("source-link");
        let destination = root.join("destination-link");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), "target bytes").unwrap();
        junction_for_test(&source, &target);
        junction_for_test(&destination, &target);
        assert!(metadata_is_symlink(&fs::symlink_metadata(&destination).unwrap()));
        let original = fs::read_link(&destination).unwrap();
        assert!(!link_target_differs(&destination, &original));
        sync_symlink_with(&source, &destination, |_, _, _| {
            panic!("matching junctions must never request link creation")
        })
        .unwrap();
        assert_eq!(fs::read_link(&destination).unwrap(), original);
        assert_eq!(fs::read_to_string(target.join("sentinel")).unwrap(), "target bytes");
    }

    #[test]
    #[cfg(windows)]
    fn actual_link_recreation_routes_both_target_kinds_and_propagates_a_late_factory_failure_after_native_type_repair() {
        for directory_target in [false, true] {
            for previous_directory in [false, true] {
                let directory = tempfile::tempdir_in(".").unwrap();
                let root = Utf8Path::from_path(directory.path()).unwrap();
                let target = root.join("target");
                let source = root.join("source-link");
                let destination = root.join("destination-link");
                fs::create_dir(&target).unwrap();
                junction_for_test(&source, &target);
                if !directory_target {
                    fs::remove_dir(&target).unwrap();
                    fs::write(&target, "file target bytes").unwrap();
                }
                if previous_directory {
                    fs::create_dir(&destination).unwrap();
                    fs::write(destination.join("old-child"), "stale bytes").unwrap();
                } else {
                    fs::write(&destination, "stale file").unwrap();
                }
                let attempted = std::cell::Cell::new(false);
                let failure = sync_symlink_with(&source, &destination, |observed, to, is_directory| {
                    assert_eq!(observed, fs::read_link(&source).unwrap());
                    assert_eq!(to, destination.as_std_path());
                    assert_eq!(is_directory, directory_target);
                    assert!(!destination.exists(), "native type repair must precede the creation callback");
                    attempted.set(true);
                    Err(io::Error::other("late link factory refusal"))
                })
                .expect_err("a late adapter failure must escape actual link recreation");
                assert!(attempted.get());
                assert_eq!(failure.message(), format!("could not recreate the link `{destination}`"));
                assert!(!destination.exists());
                assert_eq!(target.is_dir(), directory_target);
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn link_recreation_propagates_native_parent_and_stale_entry_removal_failures_before_calling_the_factory() {
        for shape in ["parent", "file", "directory"] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let target = root.join("target");
            let source = root.join("source-link");
            fs::create_dir(&target).unwrap();
            junction_for_test(&source, &target);
            let destination = if shape == "parent" {
                root.join("blocked/new/link")
            } else {
                root.join("destination")
            };
            let parent = root.join("blocked");
            let locked_path = if shape == "directory" {
                destination.join("locked")
            } else {
                destination.clone()
            };
            let locked = if shape == "parent" {
                fs::write(&parent, "blocking parent").unwrap();
                None
            } else {
                if shape == "directory" {
                    fs::create_dir(&destination).unwrap();
                }
                fs::write(&locked_path, "locked stale bytes").unwrap();
                Some(fs::File::options().read(true).share_mode(0).open(&locked_path).unwrap())
            };
            let failure = sync_symlink_with(&source, &destination, |_, _, _| {
                panic!("native repair refusal must precede link creation")
            })
            .expect_err("the exact native parent/removal failure must propagate");
            assert_eq!(
                failure.message(),
                match shape {
                    "parent" => format!("could not create `{}`", destination.parent().unwrap()),
                    "directory" => format!("could not remove stale directory at `{destination}`"),
                    _ => format!("could not remove stale entry at `{destination}`"),
                }
            );
            drop(locked);
            assert_eq!(
                fs::read_to_string(if shape == "parent" { &parent } else { &locked_path }).unwrap(),
                if shape == "parent" {
                    "blocking parent"
                } else {
                    "locked stale bytes"
                }
            );
        }
    }

    #[test]
    fn delta_failure_clears_the_old_sentinel_and_publishes_only_the_repaired_tree() {
        let (_temporary, from, to) = tree();
        fs::write(from.join("new"), "new").unwrap();
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("old"), "old").unwrap();
        mark_consistent(&to);
        let outcome = sync_or_copy_with_delta(
            &from,
            &to,
            &from.join("target"),
            CopyOptions::default(),
            |source, root, _, _, reflinks| {
                assert_eq!(source, from);
                assert_eq!(root, to);
                assert!(!is_consistent(root), "the previous sentinel must disappear before mutation begins");
                reflinks.unsupported();
                Err(error!("injected incomplete delta"))
            },
        )
        .unwrap();
        assert_eq!(outcome, SyncOutcome::FreshCopy);
        assert!(is_consistent(&to));
        assert!(!to.join("old").exists());
        assert_eq!(fs::read(to.join("new")).unwrap(), b"new");
    }

    #[test]
    fn copy_ignored_is_forwarded_through_fresh_and_delta_synchronization() {
        let (_temporary, from, to) = tree();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&from)
                .status()
                .unwrap()
                .success()
        );
        fs::write(from.join(".gitignore"), "ignored\n").unwrap();
        fs::write(from.join("ignored"), "first").unwrap();
        let skip = from.join("target");
        assert_eq!(
            sync_or_copy(&from, &to, &skip, CopyOptions { copy_ignored: true }).unwrap(),
            SyncOutcome::FreshCopy
        );
        assert_eq!(fs::read(to.join("ignored")).unwrap(), b"first");
        fs::write(from.join("ignored"), "next!").unwrap();
        assert_eq!(
            sync_or_copy(&from, &to, &skip, CopyOptions { copy_ignored: true }).unwrap(),
            SyncOutcome::Synchronized
        );
        assert_eq!(fs::read(to.join("ignored")).unwrap(), b"next!");
    }

    #[test]
    fn collection_and_each_copy_entry_path_propagate_missing_source_failures() {
        let (_temporary, from, to) = tree();
        let missing = from.join("missing");
        let failure =
            collect_source_entries(&missing, &from.join("target"), CopyOptions::default()).expect_err("the root cannot be walked");
        assert_eq!(failure.message(), "could not read the source tree");
        let failure = delta_sync(&missing, &to, &from.join("target"), CopyOptions::default(), &Reflinks::new())
            .expect_err("a failed source inventory cannot become a successful delta");
        assert_eq!(failure.message(), "could not read the source tree");
        let failure = sync_or_copy(&missing, &to, &from.join("target"), CopyOptions::default()).expect_err("a fresh copy needs a source");
        assert!(failure.message().contains("source tree"), "{failure}");
        assert!(!is_consistent(&to));
        let failure = sync_or_copy(&missing, &to, &from.join("target"), CopyOptions::default())
            .expect_err("an inconsistent tree still needs a source");
        assert!(failure.message().contains("source tree"), "{failure}");
        mark_consistent(&to);
        let failure = sync_or_copy_with_delta(&missing, &to, &from.join("target"), CopyOptions::default(), |_, _, _, _, _| {
            Err(error!("delta failed"))
        })
        .expect_err("failed repair must propagate the clean-copy error");
        assert!(failure.message().contains("source tree"), "{failure}");
        assert!(!is_consistent(&to));
    }

    #[test]
    fn clone_not_found_is_terminal_but_unsupported_clones_disable_only_the_shared_operation() {
        let (_temporary, from, to) = tree();
        fs::create_dir_all(&to).unwrap();
        let source = from.join("file");
        fs::write(&source, "complete source").unwrap();
        let destination = to.join("file");
        let reflinks = Reflinks::new();
        if cfg!(target_env = "musl") {
            return;
        }
        let failure = copy_file_for_sync_with(&source, &destination, &reflinks, |_, _| Err(io::Error::from(ErrorKind::NotFound)))
            .expect_err("a missing clone input must not fall through to a successful ordinary copy");
        assert_eq!(failure.message(), format!("could not copy `{source}` to `{destination}`"));
        assert!(!destination.exists());
        assert!(reflinks.worth_trying());
        let shared = reflinks.clone();
        let mut attempts = 0;
        copy_file_for_sync_with(&source, &destination, &reflinks, |left, right| {
            attempts += 1;
            assert_eq!(left, source.as_std_path());
            fs::write(right, "partial clone")?;
            Err(io::Error::from(ErrorKind::Unsupported))
        })
        .unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(fs::read(&destination).unwrap(), b"complete source");
        assert!(!reflinks.worth_trying());
        assert!(!shared.worth_trying());
        fs::remove_file(&destination).unwrap();
        copy_file_for_sync_with(&source, &destination, &shared, |_, _| {
            panic!("the failed clone capability is shared across the operation")
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"complete source");
        let missing = from.join("absent-after-cloning-is-disabled");
        let failure = copy_file_for_sync_with(&missing, &to.join("absent"), &shared, |_, _| panic!("cloning remains disabled"))
            .expect_err("ordinary copy failures still propagate after clone support was disabled");
        assert_eq!(failure.message(), format!("could not copy `{missing}` to `{}`", to.join("absent")));
    }

    #[test]
    fn successful_clones_are_timestamped_and_do_not_fall_through_to_another_copy() {
        if cfg!(target_env = "musl") {
            return;
        }
        let (_temporary, from, to) = tree();
        fs::create_dir_all(&to).unwrap();
        let destination = to.join("cloned");
        let copied_at = SystemTime::UNIX_EPOCH + core::time::Duration::from_hours(262_992);
        copy_file_for_sync_with_stamp(
            &from.join("source"),
            &destination,
            &Reflinks::new(),
            copied_at,
            |_, right| {
                fs::write(right, "cloned bytes")?;
                File::options()
                    .write(true)
                    .open(right)?
                    .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            },
            stamp_mtime,
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"cloned bytes");
        assert_eq!(fs::metadata(&destination).unwrap().modified().unwrap(), copied_at);
        let bad = to.join("not-a-file");
        let failure = copy_file_for_sync_with(&from.join("source"), &bad, &Reflinks::new(), |_, right| fs::create_dir(right))
            .expect_err("even a nominally successful clone must propagate a timestamp-open failure");
        assert_eq!(failure.message(), format!("could not open copied file `{bad}`"));
    }

    #[test]
    fn source_walk_ignores_parent_rules_and_search_only_ignores_but_honors_git_exclude() {
        let (_temporary, from, _to) = tree();
        let parent = from.parent().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(parent)
                .status()
                .unwrap()
                .success()
        );
        fs::write(parent.join(".gitignore"), "from/parent-hidden\n").unwrap();
        fs::write(from.join(".ignore"), "search-hidden\n").unwrap();
        for name in ["parent-hidden", "search-hidden", "git-excluded"] {
            fs::write(from.join(name), name).unwrap();
        }
        let files = collect_source_entries(&from, &from.join("target"), CopyOptions::default()).unwrap();
        assert!(files.contains(Utf8Path::new("parent-hidden")));
        assert!(files.contains(Utf8Path::new("search-hidden")));
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&from)
                .status()
                .unwrap()
                .success()
        );
        fs::write(from.join(".git/info/exclude"), "git-excluded\n").unwrap();
        let files = collect_source_entries(&from, &from.join("target"), CopyOptions::default()).unwrap();
        assert!(!files.contains(Utf8Path::new("git-excluded")));
        let (_outside, detached, _) = tree();
        fs::write(detached.join(".gitignore"), "kept\n").unwrap();
        fs::write(detached.join("kept"), "no repository marker").unwrap();
        assert!(
            collect_source_entries(&detached, &detached.join("target"), CopyOptions::default())
                .unwrap()
                .contains(Utf8Path::new("kept"))
        );
    }

    #[test]
    fn native_non_utf8_source_entries_stop_the_collection_instead_of_disappearing() {
        let (_temporary, from, _to) = tree();
        #[cfg(windows)]
        let name = { std::ffi::OsString::from_wide(&[0xd800]) };
        #[cfg(unix)]
        let name = { std::ffi::OsString::from_vec(vec![0xff]) };
        fs::write(from.as_std_path().join(name), "native name").unwrap();
        let failure = collect_source_entries(&from, &from.join("target"), CopyOptions::default())
            .expect_err("an unrepresentable input is not a successful partial tree");
        assert!(failure.message().contains("not valid UTF-8"), "{failure}");
        assert!(failure.message().contains("cannot be synchronized"), "{failure}");
    }

    #[test]
    fn tracked_overrides_restore_ignored_inputs_but_never_deleted_or_pruned_files() {
        let (_temporary, from, _to) = tree();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&from)
                .status()
                .unwrap()
                .success()
        );
        fs::write(from.join(".gitignore"), "forced\n").unwrap();
        for name in ["target", "scratch"] {
            fs::create_dir(from.join(name)).unwrap();
            fs::write(from.join(name).join("tracked"), "must stay pruned").unwrap();
        }
        fs::write(from.join("forced"), "tracked ignored input").unwrap();
        fs::write(from.join("deleted"), "removed after adding").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "--force", "--", "forced", "deleted", "target/tracked", "scratch/tracked"])
                .current_dir(&from)
                .status()
                .unwrap()
                .success()
        );
        fs::remove_file(from.join("deleted")).unwrap();
        let files = collect_source_entries(&from, &from.join("scratch"), CopyOptions::default()).unwrap();
        assert_eq!(files, HashSet::from([Utf8PathBuf::from(".gitignore"), Utf8PathBuf::from("forced")]));
        assert!(!tracked_path_allowed(&from, Utf8Path::new("target/tracked"), &from.join("scratch")));
        assert!(!tracked_path_allowed(
            &from,
            Utf8Path::new("scratch/tracked"),
            &from.join("scratch")
        ));
        assert!(tracked_path_allowed(&from, Utf8Path::new("forced"), &from.join("scratch")));
    }

    #[cfg(windows)]
    #[test]
    fn share_denials_preserve_inconsistent_trees_and_reject_stale_file_or_directory_removal() {
        let (_temporary, from, to) = tree();
        fs::create_dir_all(to.join("stale")).unwrap();
        let locked = to.join("stale").join("locked");
        fs::write(&locked, "retain on failure").unwrap();
        let held = File::options().read(true).share_mode(0).open(&locked).unwrap();
        let failure = sync_or_copy(&from, &to, &from.join("target"), CopyOptions::default())
            .expect_err("the inconsistent tree cannot be erased through an exclusive file handle");
        assert_eq!(
            failure.message(),
            format!("could not clear the inconsistent scratch tree at `{to}`")
        );
        let failure = remove_stale(&to, &HashSet::new()).expect_err("stale file deletion must propagate sharing violations");
        assert_eq!(failure.message(), format!("could not remove stale file `{locked}`"));
        let failure = delta_sync(&from, &to, &from.join("target"), CopyOptions::default(), &Reflinks::new())
            .expect_err("the delta adapter must propagate its stale-removal failure");
        assert_eq!(failure.message(), format!("could not remove stale file `{locked}`"));
        fs::write(from.join("stale"), "replacement file").unwrap();
        let failure = sync_entry(&from.join("stale"), &to.join("stale"), &Reflinks::new())
            .expect_err("a file cannot replace a directory with an exclusively held child");
        assert_eq!(
            failure.message(),
            format!("could not remove stale directory at `{}`", to.join("stale"))
        );
        drop(held);
        fs::remove_file(&locked).unwrap();
        let held = File::options()
            .read(true)
            .custom_flags(0x0200_0000)
            .share_mode(0)
            .open(to.join("stale"))
            .unwrap();
        let failure = remove_stale(&to, &HashSet::new()).expect_err("a locked empty stale directory is neither missing nor nonempty");
        assert_eq!(
            failure.message(),
            format!("could not remove stale directory `{}`", to.join("stale"))
        );
        drop(held);
        remove_stale(&to, &HashSet::new()).unwrap();
        assert!(to.is_dir());
    }

    #[test]
    fn invalid_destination_shapes_preserve_creation_and_delta_entry_errors() {
        let (_temporary, from, to) = tree();
        fs::write(from.join("file"), "source").unwrap();
        fs::write(&to, "destination is not a directory").unwrap();
        let failure = sync_or_copy(&from, &to, &from.join("target"), CopyOptions::default())
            .expect_err("an existing ordinary file cannot become a scratch root");
        assert_eq!(failure.message(), format!("could not create the scratch tree at `{to}`"));
        let nested = to.join("missing/file");
        let failure =
            sync_entry(&from.join("file"), &nested, &Reflinks::new()).expect_err("missing ancestors beneath a file cannot be created");
        assert_eq!(failure.message(), format!("could not create `{}`", nested.parent().unwrap()));
        let invalid = Utf8Path::new("invalid\0root");
        let failure = delta_sync(&from, invalid, &from.join("target"), CopyOptions::default(), &Reflinks::new())
            .expect_err("valid enumeration must still propagate a later entry-copy failure");
        assert!(failure.message().contains("invalid\0root"), "{failure}");
    }

    #[cfg(windows)]
    #[test]
    fn share_denials_propagate_directory_inspection_and_file_comparison_failures() {
        let (_temporary, from, to) = tree();
        fs::create_dir_all(from.join("directory")).unwrap();
        fs::create_dir_all(&to).unwrap();
        let destination = to.join("directory");
        fs::write(&destination, "locked type mismatch").unwrap();
        let held = File::options().read(true).share_mode(0).open(&destination).unwrap();
        let failure = sync_entry(&from.join("directory"), &destination, &Reflinks::new())
            .expect_err("a type mismatch must propagate removal failure");
        assert_eq!(failure.message(), format!("could not remove stale entry at `{destination}`"));
        drop(held);
        fs::remove_file(&destination).unwrap();
        fs::write(from.join("file"), "same").unwrap();
        fs::write(to.join("file"), "same").unwrap();
        let held = File::options().read(true).share_mode(0).open(to.join("file")).unwrap();
        let failure = sync_entry(&from.join("file"), &to.join("file"), &Reflinks::new())
            .expect_err("an inaccessible equal-length input is not unchanged");
        assert_eq!(failure.message(), format!("could not read `{}`", to.join("file")));
        drop(held);
        let invalid = Utf8Path::new("invalid\0destination");
        let failure = sync_entry(&from.join("directory"), invalid, &Reflinks::new())
            .expect_err("an invalid destination is not a nonexistent destination");
        assert_eq!(failure.message(), format!("could not inspect `{invalid}`"));
    }

    #[test]
    fn entry_sync_replaces_directories_and_distinct_file_contents_and_propagates_metadata_failures() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("source");
        fs::write(&source, "aaaa").unwrap();
        for (name, old) in [("same", "aaaa"), ("same-length", "bbbb"), ("shorter", "a")] {
            let destination = root.join(name);
            fs::write(&destination, old).unwrap();
            sync_entry(&source, &destination, &Reflinks::new()).unwrap();
            assert!(destination.is_file());
            assert_eq!(fs::read(&destination).unwrap(), b"aaaa");
        }
        let directory_destination = root.join("old-directory");
        fs::create_dir_all(directory_destination.join("nested")).unwrap();
        fs::write(directory_destination.join("nested/stale"), "old").unwrap();
        sync_entry(&source, &directory_destination, &Reflinks::new()).unwrap();
        assert!(directory_destination.is_file());
        assert_eq!(fs::read(&directory_destination).unwrap(), b"aaaa");
        let nested = root.join("new/parents/file");
        sync_entry(&source, &nested, &Reflinks::new()).unwrap();
        assert_eq!(fs::read(&nested).unwrap(), b"aaaa");
        let missing = root.join("missing");
        let failure =
            sync_entry(&missing, &root.join("unpublished"), &Reflinks::new()).expect_err("a missing source cannot be synchronized");
        assert_eq!(failure.message(), format!("could not read `{missing}`"));
        let failure = sync_entry(&source, &source.join("not-a-directory"), &Reflinks::new())
            .expect_err("a regular file cannot contain the synchronization destination");
        assert!(failure.message().contains(source.as_str()));
        assert!(!root.join("unpublished").exists());
    }

    #[test]
    fn clone_support_failure_is_shared_only_within_its_operation() {
        let operation = Reflinks::new();
        let shared = operation.clone();
        assert_eq!(should_try_reflink(&operation), !cfg!(target_env = "musl"));
        mark_reflinks_unsupported(&operation);
        assert!(!should_try_reflink(&operation));
        assert!(!should_try_reflink(&shared));
        assert_eq!(should_try_reflink(&Reflinks::new()), !cfg!(target_env = "musl"));
    }

    #[test]
    fn independent_reader_failures_propagate_and_short_reads_are_aligned() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(ErrorKind::PermissionDenied, "scripted reader failure"))
            }
        }
        struct Chunked<'a>(&'a [u8], usize);
        impl Read for Chunked<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let length = buf.len().min(self.0.len()).min(self.1);
                buf[..length].copy_from_slice(&self.0[..length]);
                self.0 = &self.0[length..];
                Ok(length)
            }
        }
        assert_eq!(
            readers_have_same_contents(Broken, io::empty()).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            readers_have_same_contents(io::empty(), Broken).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        assert!(readers_have_same_contents(Chunked(b"abcdefg", 1), Chunked(b"abcdefg", 3)).unwrap());
        assert!(!readers_have_same_contents(Chunked(b"abcdefg", 1), Chunked(b"abcXefg", 3)).unwrap());
    }

    #[test]
    fn missing_inputs_are_errors_and_do_not_publish_a_consistent_tree() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let base = Utf8Path::from_path(directory.path()).unwrap();
        let source = base.join("missing-source");
        let destination = base.join("destination");
        let reflinks = Reflinks::new();
        let failure = sync_entry(&source, &destination, &reflinks).expect_err("a missing source cannot be synchronized");
        assert_eq!(failure.message(), format!("could not read `{source}`"));
        let failure = stamp_mtime(&destination, SystemTime::UNIX_EPOCH).expect_err("a missing destination cannot be timestamped");
        assert_eq!(failure.message(), format!("could not open copied file `{destination}`"));
        let failure = copy_file_for_sync(&source, &destination, &reflinks).expect_err("a missing source cannot be copied");
        assert_eq!(failure.message(), format!("could not copy `{source}` to `{destination}`"));
        assert!(!destination.exists());
        assert!(!is_consistent(&destination));
        let failed = Mutex::new(None);
        record_invalid_path(&failed, std::path::Path::new("invalid-source"));
        let error = failed.lock().unwrap().take();
        let error = error.unwrap();
        assert_eq!(error.message(), "`invalid-source` is not valid UTF-8 and cannot be synchronized");
        record_walk_failure(&failed, ignore::Error::Io(io::Error::from(ErrorKind::PermissionDenied)));
        let error = failed.lock().unwrap().take();
        let error = error.unwrap();
        assert_eq!(error.message(), "could not read the source tree");
    }

    #[test]
    fn stale_removal_retains_exactly_expected_descendants() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        fs::create_dir_all(root.join("stale/deep")).unwrap();
        fs::write(root.join("stale/deep/gone"), "remove").unwrap();
        fs::create_dir_all(root.join("kept/deep")).unwrap();
        fs::write(root.join("kept/deep/present"), "retain").unwrap();
        fs::write(root.join("gone"), "remove").unwrap();
        let expected = HashSet::from([Utf8PathBuf::from("kept/deep/present")]);
        remove_stale(root, &expected).unwrap();
        assert!(!root.join("stale").exists());
        assert!(!root.join("gone").exists());
        assert_eq!(fs::read(root.join("kept/deep/present")).unwrap(), b"retain");
        remove_stale(root, &HashSet::new()).unwrap();
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
        assert!(root.is_dir());
    }

    #[test]
    fn directory_not_empty_classification_accepts_only_the_portable_and_platform_codes() {
        assert!(is_not_empty_error(&io::Error::from(ErrorKind::DirectoryNotEmpty)));
        for code in [39, 66] {
            assert!(is_not_empty_error(&io::Error::from_raw_os_error(code)), "code {code}");
        }
        for code in [0, 1, 38, 40, 65, 67] {
            assert!(!is_not_empty_error(&io::Error::from_raw_os_error(code)), "code {code}");
        }
        assert!(!is_not_empty_error(&io::Error::other("different failure")));
        assert!(directory_has_expected_children(&io::Error::from(ErrorKind::DirectoryNotEmpty)));
        assert!(!directory_has_expected_children(&io::Error::other("different failure")));
    }

    #[test]
    fn pruning_checks_every_prefix_and_not_only_the_leaf() {
        let root = Utf8Path::new("C:/workspace");
        let excluded = root.join("target");

        assert!(is_pruned_anywhere(root, Utf8Path::new("target/deep/file"), &excluded));
        assert!(!is_pruned_anywhere(root, Utf8Path::new("src/target/file"), &excluded));
        assert!(!is_pruned_anywhere(root, Utf8Path::new("src/lib.rs"), &excluded));
    }

    #[test]
    fn content_comparison_distinguishes_empty_prefix_and_boundary_differences() {
        assert!(readers_have_same_contents(io::Cursor::new([]), io::Cursor::new([])).unwrap());
        assert!(!readers_have_same_contents(io::Cursor::new([]), io::Cursor::new([1])).unwrap());
        assert!(!readers_have_same_contents(io::Cursor::new([1]), io::Cursor::new([])).unwrap());

        let mut left = vec![b'a'; 16 * 1024 + 1];
        let mut right = left.clone();
        right[16 * 1024] = b'b';
        assert!(!readers_have_same_contents(io::Cursor::new(&left), io::Cursor::new(&right)).unwrap());
        left[16 * 1024] = b'b';
        assert!(readers_have_same_contents(io::Cursor::new(&left), io::Cursor::new(&right)).unwrap());
    }

    #[test]
    fn every_successful_sync_path_restores_the_consistency_sentinel() {
        let (_temporary, from, to) = tree();
        let skip = from.join("target");
        fs::write(from.join("file"), "one").expect("source");

        assert_eq!(
            sync_or_copy(&from, &to, &skip, CopyOptions::default()).unwrap(),
            SyncOutcome::FreshCopy
        );
        assert!(is_consistent(&to));

        fs::write(from.join("file"), "two").expect("changed source");
        assert_eq!(
            sync_or_copy(&from, &to, &skip, CopyOptions::default()).unwrap(),
            SyncOutcome::Synchronized
        );
        assert!(is_consistent(&to));
        assert_eq!(fs::read_to_string(to.join("file")).unwrap(), "two");

        clear_sentinel(&to);
        assert!(!is_consistent(&to));
        assert_eq!(
            sync_or_copy(&from, &to, &skip, CopyOptions::default()).unwrap(),
            SyncOutcome::FreshCopy
        );
        assert!(is_consistent(&to));
    }

    #[test]
    fn source_collection_obeys_hidden_ignore_pruning_and_link_boundaries() {
        let (_temporary, from, _to) = tree();
        fs::write(from.join(".gitignore"), "ignored.txt\n").expect("ignore");
        fs::write(from.join(".hidden"), "hidden").expect("hidden");
        fs::write(from.join("ignored.txt"), "ignored").expect("ignored");
        fs::create_dir_all(from.join("real")).expect("real directory");
        fs::write(from.join("real/file"), "real").expect("real file");
        let skip = from.join("real");
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&from)
            .status()
            .expect("git runs");
        assert!(status.success());

        let ignored = collect_source_entries(&from, &skip, CopyOptions::default()).expect("default collection");
        assert!(ignored.contains(Utf8Path::new(".hidden")));
        assert!(!ignored.contains(Utf8Path::new("ignored.txt")));
        assert!(!ignored.iter().any(|path| path.starts_with("real")));

        let included = collect_source_entries(&from, &skip, CopyOptions { copy_ignored: true }).expect("ignored-file collection");
        assert!(included.contains(Utf8Path::new(".hidden")));
        assert!(included.contains(Utf8Path::new("ignored.txt")));
        assert!(!included.iter().any(|path| path.starts_with("real")));
    }

    #[test]
    fn synchronization_predicates_distinguish_every_file_state() {
        let (_temporary, from, to) = tree();
        fs::write(from.join("file"), "abc").expect("source");
        fs::create_dir_all(&to).expect("destination");
        fs::write(to.join("same"), "abc").expect("same");
        fs::write(to.join("longer"), "abcd").expect("longer");
        fs::create_dir_all(to.join("directory")).expect("directory");

        assert!(is_walk_root(Utf8Path::new("")));
        assert!(!is_walk_root(Utf8Path::new("file")));
        assert!(source_entry_exists(&from.join("file")));
        assert!(!source_entry_exists(&from.join("missing")));
        assert!(missing_directory(&to.join("missing")));
        assert!(!missing_directory(&to));

        let same = fs::metadata(to.join("same")).unwrap();
        let longer = fs::metadata(to.join("longer")).unwrap();
        let directory = fs::metadata(to.join("directory")).unwrap();
        assert!(!different_lengths(&same, &same));
        assert!(different_lengths(&same, &longer));
        assert!(metadata_is_directory(&directory));
        assert!(!metadata_is_directory(&same));
        assert!(!metadata_is_symlink(&same));
        assert!(should_recreate_link(true));
        assert!(!should_recreate_link(false));
        assert!(is_not_found(&io::Error::from(ErrorKind::NotFound)));
        assert!(!is_not_found(&io::Error::other("other")));
        ignore_not_found(io::Error::from(ErrorKind::NotFound)).unwrap();
        assert!(ignore_not_found(io::Error::other("other")).is_err());

        let mut paths = vec![Utf8PathBuf::from("a"), Utf8PathBuf::from("a/b/c"), Utf8PathBuf::from("a/b")];
        sort_stale_directories(&mut paths);
        assert_eq!(paths, ["a/b/c", "a/b", "a"]);
    }

    struct ScheduledReader<'a> {
        bytes: &'a [u8],
        schedule: &'a [usize],
        offset: usize,
        turn: usize,
    }

    impl Read for ScheduledReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.offset == self.bytes.len() {
                return Ok(0);
            }

            let scheduled = self.schedule[self.turn % self.schedule.len()];
            self.turn += 1;
            let count = scheduled.min(buf.len()).min(self.bytes.len() - self.offset);
            buf[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    fn scheduled_reader<'a>(bytes: &'a [u8], schedule: &'a [usize]) -> ScheduledReader<'a> {
        ScheduledReader {
            bytes,
            schedule,
            offset: 0,
            turn: 0,
        }
    }

    #[test]
    fn content_comparison_aligns_independent_short_reads() {
        let bytes = b"independent short reads must not change equality";

        assert!(readers_have_same_contents(scheduled_reader(bytes, &[1, 7, 2]), scheduled_reader(bytes, &[9, 3])).unwrap());
        assert!(
            !readers_have_same_contents(
                scheduled_reader(bytes, &[1, 7, 2]),
                scheduled_reader(b"independent short reads must not change equalitx", &[9, 3]),
            )
            .unwrap()
        );
        assert!(!readers_have_same_contents(scheduled_reader(bytes, &[8]), scheduled_reader(&bytes[..bytes.len() - 1], &[8])).unwrap());
    }

    fn tree() -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let from = Utf8PathBuf::from_path_buf(temporary.path().join("from")).unwrap();
        let to = Utf8PathBuf::from_path_buf(temporary.path().join("to")).unwrap();
        fs::create_dir_all(from.as_std_path()).unwrap();
        (temporary, from, to)
    }

    /// A one-file change in the source is reflected in the scratch tree without recopying
    /// unchanged files.
    #[test]
    fn a_one_file_change_is_synchronized() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        // Initial state: two files.
        fs::write(from.join("a.rs").as_std_path(), "fn a() {}").unwrap();
        fs::write(from.join("b.rs").as_std_path(), "fn b() {}").unwrap();

        let outcome = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(matches!(outcome, SyncOutcome::FreshCopy));
        assert_eq!(fs::read_to_string(to.join("a.rs").as_std_path()).unwrap(), "fn a() {}");

        // Record mtime of b.rs in scratch — it should be preserved.
        let b_mtime_before = fs::metadata(to.join("b.rs").as_std_path()).unwrap().modified().unwrap();

        // Change a.rs in source, leave b.rs unchanged (but update a.rs's mtime).
        thread::sleep(Duration::from_millis(50));
        fs::write(from.join("a.rs").as_std_path(), "fn a_new() {}").unwrap();

        let outcome = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(matches!(outcome, SyncOutcome::Synchronized));
        assert_eq!(fs::read_to_string(to.join("a.rs").as_std_path()).unwrap(), "fn a_new() {}");

        // b.rs should be unchanged — same mtime.
        let b_mtime_after = fs::metadata(to.join("b.rs").as_std_path()).unwrap().modified().unwrap();
        assert_eq!(b_mtime_before, b_mtime_after);
    }

    /// Deleted files are removed from the scratch tree.
    #[test]
    fn a_deletion_is_reflected_in_the_scratch_tree() {
        let (_tmp, from, to) = tree();
        let skip = to.parent().unwrap();

        fs::write(from.join("keep.rs").as_std_path(), "keep").unwrap();
        fs::write(from.join("gone.rs").as_std_path(), "gone").unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(to.join("gone.rs").as_std_path().exists());

        // Delete gone.rs from source.
        fs::remove_file(from.join("gone.rs").as_std_path()).unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(!to.join("gone.rs").as_std_path().exists());
        assert!(to.join("keep.rs").as_std_path().exists());
    }

    /// A renamed file shows up as a deletion + creation.
    #[test]
    fn a_rename_is_reflected_as_deletion_and_creation() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("old.rs").as_std_path(), "fn f() {}").unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(to.join("old.rs").as_std_path().exists());

        // Rename: delete old, create new.
        fs::remove_file(from.join("old.rs").as_std_path()).unwrap();
        fs::write(from.join("new.rs").as_std_path(), "fn f() {}").unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(!to.join("old.rs").as_std_path().exists());
        assert!(to.join("new.rs").as_std_path().exists());
    }

    /// An unchanged tree does not modify any file in the scratch tree.
    #[test]
    fn an_unchanged_tree_leaves_the_scratch_tree_untouched() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("stable.rs").as_std_path(), "fn stable() {}").unwrap();
        fs::create_dir_all(from.join("sub").as_std_path()).unwrap();
        fs::write(from.join("sub/mod.rs").as_std_path(), "mod sub;").unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();

        let mtime_before = fs::metadata(to.join("stable.rs").as_std_path()).unwrap().modified().unwrap();

        // Sync again with no changes.
        thread::sleep(Duration::from_millis(50));
        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();

        let mtime_after = fs::metadata(to.join("stable.rs").as_std_path()).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
    }

    /// Symlinks are synchronized correctly.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_synchronized() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("target.txt").as_std_path(), "real").unwrap();
        std::os::unix::fs::symlink("target.txt", from.join("link").as_std_path()).unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(fs::symlink_metadata(to.join("link").as_std_path()).unwrap().is_symlink());
        assert_eq!(
            fs::read_link(to.join("link").as_std_path()).unwrap().to_str().unwrap(),
            "target.txt"
        );

        // Change the link target.
        fs::remove_file(from.join("link").as_std_path()).unwrap();
        std::os::unix::fs::symlink("other.txt", from.join("link").as_std_path()).unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert_eq!(fs::read_link(to.join("link").as_std_path()).unwrap().to_str().unwrap(), "other.txt");
    }

    #[cfg(windows)]
    #[test]
    fn symlinks_are_synchronized() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new(r"C:\nowhere");
        fs::write(from.join("target.txt"), "real").expect("first target");
        fs::write(from.join("other.txt"), "other").expect("second target");
        std::os::windows::fs::symlink_file("target.txt", from.join("link")).expect("source link");

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).expect("initial copy");
        assert_eq!(
            fs::read_link(to.join("link")).expect("copied link"),
            std::path::PathBuf::from("target.txt")
        );

        fs::remove_file(from.join("link")).expect("old source link");
        std::os::windows::fs::symlink_file("other.txt", from.join("link")).expect("changed source link");
        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).expect("changed copy");

        assert_eq!(
            fs::read_link(to.join("link")).expect("updated link"),
            std::path::PathBuf::from("other.txt")
        );
    }

    #[cfg(windows)]
    #[test]
    fn symlinks_replace_stale_directories_and_create_missing_parents() {
        let (_tmp, from, to) = tree();
        let source = from.join("link");
        let destination = to.join("nested/link");
        fs::write(from.join("target.txt"), "real").expect("target");
        std::os::windows::fs::symlink_file("target.txt", &source).expect("source link");
        fs::create_dir_all(&destination).expect("stale directory");

        sync_symlink(&source, &destination).expect("replace directory with link");
        assert!(fs::symlink_metadata(&destination).expect("link metadata").is_symlink());

        fs::remove_file(&destination).expect("remove copied link");
        fs::remove_dir(destination.parent().expect("parent")).expect("remove empty parent");
        sync_symlink(&source, &destination).expect("create parent and link");
        assert_eq!(
            fs::read_link(&destination).expect("link target"),
            std::path::PathBuf::from("target.txt")
        );
    }

    /// An interrupted prior sync (no sentinel) triggers a fresh copy.
    #[test]
    fn an_interrupted_prior_sync_triggers_a_fresh_copy() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("a.rs").as_std_path(), "fn a() {}").unwrap();

        // Do an initial sync.
        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();

        // Simulate an interrupted sync by removing the sentinel and leaving extra junk.
        clear_sentinel(&to);
        fs::write(to.join("junk.rs").as_std_path(), "stale").unwrap();

        let outcome = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(matches!(outcome, SyncOutcome::FreshCopy));
        // Junk should be gone after fresh copy.
        assert!(!to.join("junk.rs").as_std_path().exists());
        assert!(to.join("a.rs").as_std_path().exists());
    }

    /// Extra files in the scratch tree (from a prior instrumentation or interrupted run) are
    /// removed during delta sync.
    #[test]
    fn extra_scratch_files_are_removed() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("real.rs").as_std_path(), "fn real() {}").unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();

        // Manually add extra files to scratch (simulating prior instrumentation leftovers).
        fs::write(to.join("instrumented.rs").as_std_path(), "stale").unwrap();
        fs::create_dir_all(to.join("ghost").as_std_path()).unwrap();
        fs::write(to.join("ghost/file.rs").as_std_path(), "stale").unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();

        assert!(to.join("real.rs").as_std_path().exists());
        assert!(!to.join("instrumented.rs").as_std_path().exists());
        assert!(!to.join("ghost").as_std_path().exists());
    }

    #[test]
    fn changed_bytes_with_identical_size_and_mtime_are_copied_and_freshened() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("f.rs").as_std_path(), "v1").unwrap();
        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        let prior_scratch_mtime = fs::metadata(to.join("f.rs").as_std_path()).unwrap().modified().unwrap();

        fs::write(from.join("f.rs").as_std_path(), "v2").unwrap();
        File::options()
            .write(true)
            .open(from.join("f.rs").as_std_path())
            .unwrap()
            .set_times(FileTimes::new().set_modified(prior_scratch_mtime))
            .unwrap();

        let _ = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();

        assert_eq!(fs::read_to_string(to.join("f.rs").as_std_path()).unwrap(), "v2");
        let dst_mtime = fs::metadata(to.join("f.rs").as_std_path()).unwrap().modified().unwrap();
        assert!(dst_mtime >= prior_scratch_mtime, "changed input must not look older to Cargo");
    }

    /// Sentinel presence/absence controls whether delta sync or fresh copy is chosen.
    #[test]
    fn sentinel_controls_sync_vs_fresh_copy() {
        let (_tmp, from, to) = tree();
        let skip = Utf8Path::new("/nowhere");

        fs::write(from.join("f.rs").as_std_path(), "fn f() {}").unwrap();

        // First call: no existing tree → fresh copy.
        let outcome = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(matches!(outcome, SyncOutcome::FreshCopy));
        assert!(is_consistent(&to));

        // Second call: consistent tree → delta sync.
        let outcome = sync_or_copy(&from, &to, skip, CopyOptions::default()).unwrap();
        assert!(matches!(outcome, SyncOutcome::Synchronized));
    }

    #[test]
    fn directories_are_created_and_replace_stale_files() {
        let (_tmp, from, to) = tree();
        let source_directory = from.join("nested");
        let destination = to.join("nested");
        fs::create_dir_all(&source_directory).expect("source directory");
        fs::create_dir_all(&to).expect("destination root");

        sync_entry(&source_directory, &destination, &Reflinks::new()).expect("new directory");
        assert!(destination.is_dir());

        fs::remove_dir(&destination).expect("empty destination directory");
        fs::write(&destination, "stale file").expect("stale file");
        sync_entry(&source_directory, &destination, &Reflinks::new()).expect("replace stale file");
        assert!(destination.is_dir());
    }

    #[test]
    fn directories_replace_symlinks_without_writing_through_them() {
        let (temporary, from, to) = tree();
        let skip = from.join("target");
        let source_directory = from.join("nested");
        let source_file = source_directory.join("file");
        let destination = to.join("nested");
        let external = Utf8PathBuf::from_path_buf(temporary.path().join("external")).expect("UTF-8 temporary path");

        fs::create_dir_all(&source_directory).expect("source directory");
        fs::write(&source_file, "source contents").expect("source file");
        sync_or_copy(&from, &to, &skip, CopyOptions::default()).expect("initial copy");

        fs::remove_dir_all(&destination).expect("old destination directory");
        fs::create_dir_all(&external).expect("external link target");
        fs::write(external.join("guard"), "untouched").expect("external guard");
        create_directory_symlink(&external, &destination);

        let outcome = sync_or_copy(&from, &to, &skip, CopyOptions::default()).expect("delta sync");

        assert_eq!(outcome, SyncOutcome::Synchronized);
        let metadata = fs::symlink_metadata(&destination).expect("destination metadata");
        assert!(metadata.is_dir());
        assert!(!metadata.is_symlink());
        assert_eq!(
            fs::read_to_string(destination.join("file")).expect("synchronized file"),
            "source contents"
        );
        assert!(!external.join("file").exists(), "sync must not write through the stale link");
        assert_eq!(fs::read_to_string(external.join("guard")).expect("external guard"), "untouched");
    }

    #[cfg(unix)]
    fn create_directory_symlink(target: &Utf8Path, link: &Utf8Path) {
        std::os::unix::fs::symlink(target, link).expect("directory symlink");
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Utf8Path, link: &Utf8Path) {
        std::os::windows::fs::symlink_dir(target, link).expect("directory symlink");
    }

    #[test]
    fn regular_files_replace_directories_and_create_missing_parents() {
        let (_tmp, from, to) = tree();
        let source = from.join("file");
        let destination = to.join("deep/file");
        fs::write(&source, "new contents").expect("source file");
        fs::create_dir_all(&destination).expect("stale directory");

        sync_entry(&source, &destination, &Reflinks::new()).expect("replace stale directory");

        assert_eq!(fs::read_to_string(&destination).expect("copied file"), "new contents");
        assert!(destination.parent().expect("parent").is_dir());
    }

    #[test]
    fn permission_changes_make_otherwise_identical_files_differ() {
        let (_tmp, from, to) = tree();
        let source = from.join("file");
        let destination = to.join("file");
        fs::create_dir_all(&to).expect("destination");
        fs::write(&source, "same").expect("source");
        fs::write(&destination, "same").expect("destination");

        let mut permissions = fs::metadata(&source).expect("source metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions).expect("readonly source");

        let differs = file_differs(
            &source,
            &destination,
            &fs::metadata(&source).expect("source metadata"),
            &fs::metadata(&destination).expect("destination metadata"),
        )
        .expect("comparison");

        assert!(differs);
    }

    #[test]
    fn content_comparison_reports_open_failures() {
        let (_tmp, from, _to) = tree();
        let existing = from.join("existing");
        let missing = from.join("missing");
        fs::write(&existing, "contents").expect("existing file");

        let left = same_contents(&missing, &existing).expect_err("missing left file");
        assert!(left.to_string().contains("could not read"), "{left}");

        let right = same_contents(&existing, &missing).expect_err("missing right file");
        assert!(right.to_string().contains("could not read"), "{right}");
    }

    #[test]
    fn stale_nonempty_directories_are_retained_for_expected_children() {
        let (_tmp, _from, to) = tree();
        let child = Utf8PathBuf::from("parent/kept");
        fs::create_dir_all(to.join("parent")).expect("parent");
        fs::write(to.join(&child), "kept").expect("expected child");
        let expected = HashSet::from([child.clone()]);

        remove_stale(&to, &expected).expect("stale removal");

        assert_eq!(fs::read_to_string(to.join(child)).expect("expected child remains"), "kept");
        assert!(to.join("parent").is_dir());
    }

    #[test]
    fn only_the_first_parallel_walk_failure_is_recorded() {
        let failure = Mutex::new(None);
        record(&failure, error!("first"));
        record(&failure, error!("second"));

        let recorded = failure.into_inner().expect("unpoisoned").expect("failure");
        assert_eq!(recorded.to_string(), "first");
    }

    #[test]
    fn tracked_ignored_files_are_included_but_pruned_paths_are_not() {
        let (_tmp, from, to) = tree();
        fs::write(from.join(".gitignore"), "ignored.txt\nexcluded/\n").expect("ignore file");
        fs::write(from.join("ignored.txt"), "tracked despite ignore").expect("tracked file");
        fs::create_dir_all(from.join("excluded")).expect("excluded directory");
        fs::write(from.join("excluded/file"), "skip").expect("excluded file");

        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&from)
            .status()
            .expect("git runs");
        assert!(status.success());
        let status = Command::new("git")
            .args(["add", "--force", "ignored.txt"])
            .current_dir(&from)
            .status()
            .expect("git add runs");
        assert!(status.success());

        let entries = collect_source_entries(&from, &from.join("excluded"), CopyOptions::default()).expect("source entries");

        assert!(entries.contains(Utf8Path::new("ignored.txt")));
        assert!(!entries.iter().any(|path| path.starts_with("excluded")));
        assert!(!to.exists(), "collection does not write the destination");
    }
}
