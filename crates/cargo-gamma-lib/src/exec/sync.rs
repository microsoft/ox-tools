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
use std::io::{ErrorKind, Read};
use std::sync::Mutex;
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::{WalkBuilder, WalkState};
use walkdir::WalkDir;

use crate::Result;
use crate::error::{Error, error};
use crate::exec::copy::{CopyOptions, Reflinks, copy_tree_with, is_pruned, tracked_files};

/// Outcome of a delta synchronization attempt.
#[derive(Debug)]
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

    // Synchronize: copy new/changed, leave unchanged.
    for relative in &expected {
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
                    record(failure, error!("could not read the source tree").caused_by(cause));
                    return WalkState::Quit;
                }
            };

            let Some(path) = Utf8Path::from_path(entry.path()) else {
                record(
                    failure,
                    error!("`{}` is not valid UTF-8 and cannot be synchronized", entry.path().display()),
                );
                return WalkState::Quit;
            };

            let Ok(relative) = path.strip_prefix(&root) else {
                return WalkState::Continue;
            };

            if relative.as_str().is_empty() {
                return WalkState::Continue;
            }

            if is_pruned(path, relative, &excluded) {
                return WalkState::Skip;
            }

            let mut set = entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _inserted = set.insert(relative.to_owned());

            WalkState::Continue
        })
    });

    if let Some(cause) = failure.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner) {
        return Err(cause);
    }

    let mut result = entries.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);

    // Include tracked files that the ignore walk may have skipped, same as copy_tracked does.
    if let Some(tracked) = tracked_files(source)? {
        for relative in tracked {
            if !is_pruned_anywhere(source, &relative, skip) {
                let src = source.join(&relative);
                if fs::symlink_metadata(src.as_std_path()).is_ok() {
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
        if !destination.as_std_path().exists() {
            fs::create_dir_all(destination.as_std_path()).map_err(|cause| error!("could not create `{destination}`").caused_by(cause))?;
        } else if !destination.as_std_path().is_dir() {
            // A non-directory where a directory should be — replace it.
            fs::remove_file(destination.as_std_path())
                .map_err(|cause| error!("could not remove stale entry at `{destination}`").caused_by(cause))?;
            fs::create_dir_all(destination.as_std_path()).map_err(|cause| error!("could not create `{destination}`").caused_by(cause))?;
        }
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
                if dst_meta.is_dir() {
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
            && !parent.as_std_path().exists()
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
    if src.len() != dst.len() {
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
    let mut left = File::open(left.as_std_path()).map_err(|cause| error!("could not read `{left}`").caused_by(cause))?;
    let mut right = File::open(right.as_std_path()).map_err(|cause| error!("could not read `{right}`").caused_by(cause))?;
    let mut left_buffer = [0_u8; 16 * 1024];
    let mut right_buffer = [0_u8; 16 * 1024];

    loop {
        let left_read = left
            .read(&mut left_buffer)
            .map_err(|cause| error!("could not compare scratch input").caused_by(cause))?;
        let right_read = right
            .read(&mut right_buffer)
            .map_err(|cause| error!("could not compare scratch input").caused_by(cause))?;

        if left_read != right_read {
            return Ok(false);
        }
        if left_buffer[..left_read] != right_buffer[..left_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

/// Synchronizes a symlink. Recreates it if the target changed or if the destination is not a link.
fn sync_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let src_target = fs::read_link(source.as_std_path()).map_err(|cause| error!("could not read the link `{source}`").caused_by(cause))?;

    let needs_recreate = match fs::symlink_metadata(destination.as_std_path()) {
        Err(_) => true,
        Ok(dst_meta) => {
            if dst_meta.is_symlink() {
                // Both are symlinks — compare targets.
                fs::read_link(destination.as_std_path()).map_or(true, |dst_target| dst_target != src_target)
            } else {
                // Type mismatch — remove what is there.
                if dst_meta.is_dir() {
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

    if needs_recreate {
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
            let linked = if source
                .parent()
                .map_or_else(|| src_target.is_dir(), |parent| parent.as_std_path().join(&src_target).is_dir())
            {
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

    if reflinks.worth_trying() {
        match reflink_copy::reflink(source.as_std_path(), destination.as_std_path()) {
            Ok(()) => {
                stamp_mtime(destination, copied_at)?;
                return Ok(());
            }
            Err(cause) if cause.kind() == ErrorKind::NotFound => {
                return Err(error!("could not copy `{source}` to `{destination}`").caused_by(cause));
            }
            Err(_unsupported) => {
                reflinks.unsupported();
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

    for entry in WalkDir::new(root.as_std_path()).into_iter().filter_map(core::result::Result::ok) {
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };

        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };

        if relative.as_str().is_empty() {
            continue;
        }

        if !expected.contains(relative) {
            if entry.file_type().is_dir() {
                stale_dirs.push(path.to_owned());
            } else {
                stale_files.push(path.to_owned());
            }
        }
    }

    // Remove stale files first.
    for file in &stale_files {
        fs::remove_file(file.as_std_path())
            .or_else(|cause| if cause.kind() == ErrorKind::NotFound { Ok(()) } else { Err(cause) })
            .map_err(|cause| error!("could not remove stale file `{file}`").caused_by(cause))?;
    }

    // Remove stale directories deepest-first so that parents are empty when reached.
    stale_dirs.sort_by_key(|path| core::cmp::Reverse(path.as_str().len()));
    for dir in &stale_dirs {
        // Only remove if truly empty (children may have been expected).
        match fs::remove_dir(dir.as_std_path()) {
            Ok(()) => {}
            Err(cause) if cause.kind() == ErrorKind::NotFound => {}
            // Not empty — some children were expected; leave it.
            Err(cause) if is_not_empty_error(&cause) => {}
            Err(cause) => {
                return Err(error!("could not remove stale directory `{dir}`").caused_by(cause));
            }
        }
    }

    Ok(())
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
    use std::thread;

    use super::*;

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
}
