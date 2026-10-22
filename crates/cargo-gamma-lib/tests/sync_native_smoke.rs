// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(all(not(miri), windows))]

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::exec::sync_test_util::*;

#[test]
#[cfg(windows)]
fn file_sync_refuses_a_locked_native_link_before_following_or_overwriting_its_target() {
    let directory = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(directory.path()).unwrap();
    let source = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery/lib.rs");
    let target = root.join("external");
    let destination = root.join("destination-link");
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
