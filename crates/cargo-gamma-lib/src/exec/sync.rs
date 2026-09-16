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
pub(super) fn sync_or_copy(source: &Utf8Path, root: &Utf8Path, skip: &Utf8Path, options: CopyOptions) -> Result<SyncOutcome> {
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

    match delta_sync(source, root, skip, options, &reflinks) {
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
fn collect_source_entries(source: &Utf8Path, skip: &Utf8Path, options: CopyOptions) -> Result<HashSet<Utf8PathBuf>> {
    let entries: Mutex<HashSet<Utf8PathBuf>> = Mutex::new(HashSet::new());
    let failure: Mutex<Option<Error>> = Mutex::new(None);

    let mut builder = WalkBuilder::new(source.as_std_path());
    let _builder = builder
        .hidden(false)
        .parents(false)
        .require_git(true)
        .git_ignore(!options.copy_ignored)
        .git_exclude(!options.copy_ignored)
        .git_global(false)
        .ignore(false)
        .follow_links(false);

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
    if let Some(tracked) = tracked_files(source)? {
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

        if !equal || left_eof != right_eof {
            return Ok(false);
        }
        if left_eof {
            return Ok(true);
        }

        left.consume(compared);
        right.consume(compared);
    }
}

/// Synchronizes a symlink. Recreates it if the target changed or if the destination is not a link.
fn sync_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
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

        #[cfg(unix)]
        std::os::unix::fs::symlink(&src_target, destination.as_std_path())
            .map_err(|cause| error!("could not recreate the link `{destination}`").caused_by(cause))?;

        #[cfg(windows)]
        {
            let linked = if source_link_targets_directory(source, &src_target) {
                std::os::windows::fs::symlink_dir(&src_target, destination.as_std_path())
            } else {
                std::os::windows::fs::symlink_file(&src_target, destination.as_std_path())
            };
            linked.map_err(|cause| error!("could not recreate the link `{destination}`").caused_by(cause))?;
        }
    }

    Ok(())
}

fn copy_file_for_sync(source: &Utf8Path, destination: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    let copied_at = SystemTime::now();

    if should_try_reflink(reflinks) {
        match reflink_copy::reflink(source.as_std_path(), destination.as_std_path()) {
            Ok(()) => {
                stamp_mtime(destination, copied_at)?;
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

    stamp_mtime(destination, copied_at)?;

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
fn remove_stale(root: &Utf8Path, expected: &HashSet<Utf8PathBuf>) -> Result<()> {
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
        match fs::remove_dir(dir.as_std_path()) {
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

fn record_walk_failure(failure: &Mutex<Option<Error>>, cause: ignore::Error) {
    record(failure, error!("could not read the source tree").caused_by(cause));
}

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

fn collection_failure<T>(cause: Error) -> Result<T> {
    Err(cause)
}

fn tracked_path_allowed(source: &Utf8Path, relative: &Utf8Path, skip: &Utf8Path) -> bool {
    !is_pruned_anywhere(source, relative, skip)
}

fn source_entry_exists(path: &Utf8Path) -> bool {
    fs::symlink_metadata(path.as_std_path()).is_ok()
}

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

fn link_target_differs(destination: &Utf8Path, source_target: &std::path::Path) -> bool {
    match fs::read_link(destination.as_std_path()) {
        Ok(target) => target != source_target,
        Err(_) => true,
    }
}

const fn should_recreate_link(needs_recreate: bool) -> bool {
    needs_recreate
}

#[cfg(windows)]
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

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use core::time::Duration;
    use std::process::Command;
    use std::{io, thread};

    use super::*;

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
