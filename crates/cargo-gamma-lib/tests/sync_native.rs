// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

use std::collections::HashSet;
use std::fs::{self, File, FileTimes};
use std::io::{self, ErrorKind};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
use std::sync::Mutex;
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::error::Error;
use cargo_gamma_lib::internals::exec::sync_test_util::*;

macro_rules! error {
    ($($args:tt)*) => { Error::new(format!($($args)*)) };
}

fn tree() -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
    let temporary = tempfile::tempdir_in(".").expect("the repository permits private synchronization output directories");
    let from = fixture();
    let to = Utf8PathBuf::from_path_buf(temporary.path().join("to")).expect("the private synchronization output path is UTF-8");
    (temporary, from, to)
}
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
fn source_collection_propagates_a_late_tracked_inventory_failure_after_the_native_walk() {
    let source = fixture();
    let root = source.as_path();
    let reached = std::cell::Cell::new(false);
    let failure = collect_source_entries_with(root, &root.join("excluded"), CopyOptions::default(), |source| {
        assert_eq!(source, root);
        assert_eq!(
            fs::read_to_string(source.join("lib.rs")).unwrap(),
            include_str!("fixtures/completed-recovery/lib.rs")
        );
        reached.set(true);
        Err(error!("late tracked inventory refusal"))
    })
    .expect_err("a successful native walk does not authorize hiding a later tracked-inventory failure");
    assert!(reached.get());
    assert_eq!(failure.message(), "late tracked inventory refusal");
    assert_eq!(
        fs::read_to_string(root.join("lib.rs")).unwrap(),
        include_str!("fixtures/completed-recovery/lib.rs")
    );
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
    remove_stale_with_directory(root, &HashSet::from([Utf8PathBuf::from("kept")]), |path| {
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
    let source = fixture().join("lib.rs");
    let destination = root.join("destination");
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
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                include_str!("fixtures/completed-recovery/lib.rs")
            );
            reached.set(true);
            Err(error!("late ordinary-copy stamp refusal"))
        },
    )
    .expect_err("the late fallback stamp result must propagate after genuine native copying");
    assert!(reached.get());
    assert_eq!(failure.message(), "late ordinary-copy stamp refusal");
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        include_str!("fixtures/completed-recovery/lib.rs")
    );
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        include_str!("fixtures/completed-recovery/lib.rs")
    );
}

#[test]
#[cfg(windows)]
fn the_file_sync_caller_propagates_native_copy_denial_instead_of_accepting_its_old_destination() {
    let directory = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(directory.path()).unwrap();
    let source = fixture().join("lib.rs");
    let destination = root.join("destination");
    fs::write(&destination, "long old bytes").unwrap();
    let metadata = fs::metadata(&source).unwrap();
    let locked = File::options().read(true).share_mode(0).open(&destination).unwrap();
    let failure = sync_file(&source, &destination, &metadata, &Reflinks::new())
        .expect_err("different lengths require copying, and the native destination lock must refuse it");
    assert_eq!(failure.message(), format!("could not copy `{source}` to `{destination}`"));
    drop(locked);
    assert_eq!(
        fs::read_to_string(&source).unwrap(),
        include_str!("fixtures/completed-recovery/lib.rs")
    );
    assert_eq!(fs::read_to_string(&destination).unwrap(), "long old bytes");
}

#[test]
fn delta_failure_clears_the_old_sentinel_and_publishes_only_the_repaired_tree() {
    let (_temporary, from, to) = tree();
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
    assert_eq!(
        fs::read(to.join("lib.rs")).unwrap(),
        include_bytes!("fixtures/completed-recovery/lib.rs")
    );
}

#[test]
fn failed_scratch_cleanup_never_overlays_or_marks_a_mixed_tree_consistent() {
    for consistent in [false, true] {
        let (_temporary, from, to) = tree();
        fs::create_dir_all(&to).unwrap();
        fs::write(to.join("old"), "stale").unwrap();
        if consistent {
            mark_consistent(&to);
        }
        let failure = sync_or_copy_with_cleanup(
            &from,
            &to,
            &from.join("target"),
            CopyOptions::default(),
            |_, root, _, _, _| {
                assert!(consistent);
                assert!(!is_consistent(root));
                Err(error!("injected incomplete delta"))
            },
            |root| {
                assert_eq!(root, to);
                Err(std::io::Error::from(ErrorKind::PermissionDenied))
            },
        )
        .expect_err("a failed cleanup cannot authorize copying over stale files");
        assert!(failure.message().contains("could not clear"));
        assert_eq!(fs::read(to.join("old")).unwrap(), b"stale");
        assert!(!to.join("lib.rs").exists());
        assert!(!is_consistent(&to));
    }
}

#[test]
#[cfg(windows)]
fn a_locked_consistency_marker_prevents_delta_mutation() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let (_temporary, from, to) = tree();
    fs::create_dir_all(&to).unwrap();
    fs::write(to.join("old"), "unchanged").unwrap();
    mark_consistent(&to);
    let lock = File::options().read(true).share_mode(1).open(sentinel(&to)).unwrap();
    let failure = sync_or_copy_with_delta(&from, &to, &from.join("target"), CopyOptions::default(), |_, _, _, _, _| {
        panic!("mutation must not begin while the consistency marker cannot be invalidated")
    })
    .expect_err("a locked consistency marker cannot remain valid across mutation");
    assert!(failure.message().contains("could not invalidate scratch consistency"));
    assert_eq!(fs::read(to.join("old")).unwrap(), b"unchanged");
    assert!(is_consistent(&to));
    drop(lock);
}

#[test]
fn copy_ignored_is_forwarded_through_fresh_and_delta_synchronization() {
    let directory = tempfile::tempdir_in(".").unwrap();
    let to = Utf8PathBuf::from_path_buf(directory.path().join("output")).unwrap();
    let from = fixture();
    let skip = to.join("target");
    let copy = |source: &Utf8Path, root: &Utf8Path, excluded: &Utf8Path, options: CopyOptions, _: &Reflinks| {
        assert_eq!(source, from);
        assert_eq!(excluded, skip);
        assert!(options.copy_ignored);
        fs::create_dir_all(root)?;
        fs::copy(source.join("lib.rs"), root.join("lib.rs"))?;
        Ok(())
    };
    assert_eq!(
        sync_or_copy_using(
            &from,
            &to,
            &skip,
            CopyOptions { copy_ignored: true },
            |_, _, _, _, _| panic!("a fresh output cannot use delta synchronization"),
            |path| fs::remove_dir_all(path),
            copy
        )
        .unwrap(),
        SyncOutcome::FreshCopy
    );
    assert_eq!(
        fs::read(to.join("lib.rs")).unwrap(),
        include_bytes!("fixtures/completed-recovery/lib.rs")
    );
    assert_eq!(
        sync_or_copy_using(
            &from,
            &to,
            &skip,
            CopyOptions { copy_ignored: true },
            |source, root, excluded, options, _| {
                assert_eq!(source, from);
                assert_eq!(excluded, skip);
                assert!(options.copy_ignored);
                fs::copy(source.join("changed.rs"), root.join("lib.rs"))?;
                Ok(())
            },
            |path| fs::remove_dir_all(path),
            |_, _, _, _, _| panic!("a successful delta cannot use the fresh-copy collaborator")
        )
        .unwrap(),
        SyncOutcome::Synchronized
    );
    assert_eq!(
        fs::read(to.join("lib.rs")).unwrap(),
        include_bytes!("fixtures/completed-recovery/changed.rs")
    );
}

#[test]
fn collection_and_each_copy_entry_path_propagate_missing_source_failures() {
    let (_temporary, from, to) = tree();
    let missing = from.join("missing");
    let failure = collect_source_entries(&missing, &from.join("target"), CopyOptions::default()).expect_err("the root cannot be walked");
    assert_eq!(failure.message(), "could not read the source tree");
    let failure = delta_sync(&missing, &to, &from.join("target"), CopyOptions::default(), &Reflinks::new())
        .expect_err("a failed source inventory cannot become a successful delta");
    assert_eq!(failure.message(), "could not read the source tree");
    let failure = sync_or_copy(&missing, &to, &from.join("target"), CopyOptions::default()).expect_err("a fresh copy needs a source");
    assert!(failure.message().contains("source tree"), "{failure}");
    assert!(!is_consistent(&to));
    let failure =
        sync_or_copy(&missing, &to, &from.join("target"), CopyOptions::default()).expect_err("an inconsistent tree still needs a source");
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
    let source = from.join("lib.rs");
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
    assert_eq!(
        fs::read(&destination).unwrap(),
        include_bytes!("fixtures/completed-recovery/lib.rs")
    );
    assert!(!reflinks.worth_trying());
    assert!(!shared.worth_trying());
    fs::remove_file(&destination).unwrap();
    copy_file_for_sync_with(&source, &destination, &shared, |_, _| {
        panic!("the failed clone capability is shared across the operation")
    })
    .unwrap();
    assert_eq!(
        fs::read(&destination).unwrap(),
        include_bytes!("fixtures/completed-recovery/lib.rs")
    );
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
    let failure = sync_entry(&from.join("lib.rs"), &to.join("stale"), &Reflinks::new())
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
    fs::write(&to, "destination is not a directory").unwrap();
    let failure = sync_or_copy(&from, &to, &from.join("target"), CopyOptions::default())
        .expect_err("an existing ordinary file cannot become a scratch root");
    assert_eq!(failure.message(), format!("could not create the scratch tree at `{to}`"));
    let nested = to.join("missing/file");
    let failure =
        sync_entry(&from.join("lib.rs"), &nested, &Reflinks::new()).expect_err("missing ancestors beneath a file cannot be created");
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
    fs::create_dir_all(&to).unwrap();
    let destination = to.join("directory");
    fs::write(&destination, "locked type mismatch").unwrap();
    let held = File::options().read(true).share_mode(0).open(&destination).unwrap();
    let failure = sync_entry(&from, &destination, &Reflinks::new()).expect_err("a type mismatch must propagate removal failure");
    assert_eq!(failure.message(), format!("could not remove stale entry at `{destination}`"));
    drop(held);
    fs::remove_file(&destination).unwrap();
    fs::write(to.join("file"), include_bytes!("fixtures/completed-recovery/lib.rs")).unwrap();
    let held = File::options().read(true).share_mode(0).open(to.join("file")).unwrap();
    let failure = sync_entry(&from.join("lib.rs"), &to.join("file"), &Reflinks::new())
        .expect_err("an inaccessible equal-length input is not unchanged");
    assert_eq!(failure.message(), format!("could not read `{}`", to.join("file")));
    drop(held);
    let invalid = Utf8Path::new("invalid\0destination");
    let failure = sync_entry(&from, invalid, &Reflinks::new()).expect_err("an invalid destination is not a nonexistent destination");
    assert_eq!(failure.message(), format!("could not inspect `{invalid}`"));
}

#[test]
fn entry_sync_replaces_directories_and_distinct_file_contents_and_propagates_metadata_failures() {
    let directory = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(directory.path()).unwrap();
    let source = fixture().join("changed.rs");
    for (name, old) in [
        ("same", include_bytes!("fixtures/completed-recovery/changed.rs").to_vec()),
        (
            "same-length",
            include_bytes!("fixtures/completed-recovery/changed.rs")
                .iter()
                .map(|byte| byte ^ 1)
                .collect(),
        ),
        ("shorter", b"a".to_vec()),
    ] {
        let destination = root.join(name);
        fs::write(&destination, old).unwrap();
        sync_entry(&source, &destination, &Reflinks::new()).unwrap();
        assert!(destination.is_file());
        assert_eq!(
            fs::read(&destination).unwrap(),
            include_bytes!("fixtures/completed-recovery/changed.rs")
        );
    }
    let directory_destination = root.join("old-directory");
    fs::create_dir_all(directory_destination.join("nested")).unwrap();
    fs::write(directory_destination.join("nested/stale"), "old").unwrap();
    sync_entry(&source, &directory_destination, &Reflinks::new()).unwrap();
    assert!(directory_destination.is_file());
    assert_eq!(
        fs::read(&directory_destination).unwrap(),
        include_bytes!("fixtures/completed-recovery/changed.rs")
    );
    let nested = root.join("new/parents/file");
    sync_entry(&source, &nested, &Reflinks::new()).unwrap();
    assert_eq!(fs::read(&nested).unwrap(), include_bytes!("fixtures/completed-recovery/changed.rs"));
    let missing = root.join("missing");
    let failure = sync_entry(&missing, &root.join("unpublished"), &Reflinks::new()).expect_err("a missing source cannot be synchronized");
    assert_eq!(failure.message(), format!("could not read `{missing}`"));
    let failure = sync_entry(&source, &source.join("not-a-directory"), &Reflinks::new())
        .expect_err("a regular file cannot contain the synchronization destination");
    assert!(failure.message().contains(source.as_str()));
    assert!(!root.join("unpublished").exists());
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

fn fixture() -> Utf8PathBuf {
    Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery")
}
