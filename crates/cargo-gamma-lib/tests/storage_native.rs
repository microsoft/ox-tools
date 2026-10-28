// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

use core::error::Error as _;
use core::time::Duration;
use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;
use std::rc::Rc;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::Result;
use cargo_gamma_lib::internals::discover::{ContextDigest, Killers};
use cargo_gamma_lib::internals::error::Error;
use cargo_gamma_lib::internals::exec::native_storage_test_util::*;
use cargo_gamma_lib::internals::exec::{Config, Events, OrderingHints, Phases, Session, Workspace};
use cargo_gamma_lib::internals::model::Mutant;
use serde_json::{Value, from_slice, json};

fn native_non_utf8_name() -> OsString {
    #[cfg(unix)]
    {
        OsString::from_vec(vec![b'n', 0xff, b'.', b's', b'o'])
    }
    #[cfg(windows)]
    {
        OsString::from_wide(&[0xd800, u16::from(b'.'), u16::from(b'd'), u16::from(b'l'), u16::from(b'l')])
    }
}

fn frozen_testing_snapshot() -> Value {
    let base = Utf8PathBuf::from("invalid\0campaign");
    let session = Session {
        baseline: Duration::ZERO,
        baseline_wall: Duration::ZERO,
        tests: None,
        quiet: Duration::ZERO,
        stall: None,
        build: Duration::ZERO,
        peak: None,
        metered: false,
        unbounded: None,
        withdrawn: 0,
        census: Vec::new(),
        rounds: 0,
        rounds_taken: Vec::new(),
        binaries: Vec::new(),
        scratch: base.clone(),
        filtered: 0,
        widened: false,
        ordering: OrderingHints::default(),
        phases: Phases::default(),
    };
    json!({
        "version": 2, "tool": env!("CARGO_PKG_VERSION"),
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "confidences": {},
        "plan": {
            "root": "adopted", "files": [], "mutants": [], "suppressed": 0, "idle": [],
            "sharded_out": 0, "settled_out": 0, "digests": {}, "skipped": [], "reach": {}, "specs": {}
        },
        "session": session,
        "oracle": { "packages": [], "package_local": false, "whole_workspace": false },
        "workspace": {},
        "settings": {
            "jobs": 1, "mutators": null, "timeout_floor": Duration::ZERO,
            "confirm": false, "narrowed": false, "incremental": false
        },
        "killers": empty_execution_killers(),
        "record": {
            "report_info": null, "name_filtered": false, "base": base,
            "context": ContextDigest::default(),
            "inputs": { "complete": true, "files": [], "excluded": [], "external": [] },
            "killers": Killers::default()
        },
        "stuck": [], "dropped": [], "artifact_roots": [], "artifacts": []
    })
}

#[test]
fn recovered_prepared_file_kind_policy_retains_native_files_and_directories() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = fs::symlink_metadata(root).unwrap().file_type();
    let file = fs::symlink_metadata(root.join("src").join("lib.rs")).unwrap().file_type();
    for (file_type, kind, leaf) in [(directory, "Directory", false), (file, "File", true)] {
        let (symlink, file, directory, actual_kind, actual_leaf) = prepared_native_kind(file_type);
        assert_eq!(symlink, file_type.is_symlink());
        assert_eq!(file, file_type.is_file());
        assert_eq!(directory, file_type.is_dir());
        assert_eq!(actual_kind, kind);
        assert_eq!(actual_leaf, leaf);
    }
}

#[test]
fn recovered_prepared_tree_walk_disables_both_native_symlink_following_modes() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("completed-recovery");
    let (paths, observed) = with_tree_walk_policy(|| prepared_tree_paths(std::slice::from_ref(&root), &root.join("target")).unwrap());
    assert!(paths.contains(&root.join("lib.rs")));
    assert_eq!(observed, [(false, false)]);
}

#[test]
fn recovered_native_artifact_link_read_returns_the_real_invalid_path_error() {
    let path = Utf8Path::new("invalid\0prepared-link");
    let failure = read_artifact_link(path).unwrap_err();
    assert_eq!(
        failure.message(),
        "could not read prepared symlink `invalid\0prepared-link`; restore it or start a new campaign"
    );
}

#[test]
fn recovered_native_journal_replay_returns_open_errors_without_changing_the_frozen_testing_state() {
    let before = frozen_testing_snapshot();
    let base = Utf8Path::new("invalid\0journal-replay");
    let (result, after) = replay_testing_journal(before.clone(), base);
    let failure = result.unwrap_err();
    assert_eq!(
        failure.message(),
        format!("could not read testing verdicts `{}`", base.join("testing-verdicts.jsonl"))
    );
    assert_eq!(after, before);
}

#[test]
fn recovered_native_artifact_capture_returns_root_inventory_errors_before_enumeration_or_publication() {
    for invalid_environment in [false, true] {
        let mut work = Workspace::adopt("invalid\0prepared-artifact-root".into(), "invalid\0prepared-artifact-target".into());
        if invalid_environment {
            work.test_set_environment("subject", "OUT_DIR", native_non_utf8_name());
        }
        let expected = if invalid_environment {
            "a prepared test artifact has a non-UTF-8 path"
        } else {
            "could not inspect prepared testing tree `invalid\0prepared-artifact-root`"
        };
        let Err(failure) = capture_native_artifacts(work, frozen_testing_snapshot()) else {
            panic!("the invalid prepared artifact root or environment must return its inventory error");
        };
        assert_eq!(failure.message(), expected);
    }
}

#[test]
fn recovered_native_workspace_snapshot_round_trips_non_unicode_loader_and_environment_values() {
    let value = native_non_utf8_name();
    let mut work = Workspace::adopt("never-read-snapshot-root".into(), "never-written-snapshot-target".into());
    work.test_set_environment("subject", "OUT_DIR", value.clone());
    let snapshot = work.test_snapshot_with_native_values(value.clone(), "33554432".into()).unwrap();
    let (loader, stack) = snapshot_launch(snapshot["launch"].clone()).unwrap();
    assert_eq!(loader, Some(value));
    assert_eq!(stack, "33554432");
    assert!(snapshot["test_environment"].is_object());
}

#[test]
fn recovered_native_artifact_metadata_routes_follow_and_link_queries_independently_before_classifying_the_result() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = fs::metadata(root).unwrap();
    let file = fs::metadata(root.join("tests").join("fixtures").join("completed-recovery").join("lib.rs")).unwrap();
    let path = Utf8Path::new("never-inspected-artifact");
    with_artifact_stats(directory.clone(), file.clone(), || {
        assert_eq!(inspect_native_artifact(path, false).unwrap(), "Directory");
        assert_eq!(inspect_native_artifact(path, true).unwrap(), "File");
    });
    with_artifact_stats(file, directory, || {
        let metadata = read_artifact_metadata(path).unwrap();
        assert_eq!(metadata["kind"], "File");
        assert_eq!(metadata["link"], Value::Null);
    });
}

#[test]
fn recovered_native_artifact_inspection_reads_only_immutable_file_and_directory_paths() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
    for follow in [false, true] {
        assert_eq!(inspect_native_artifact(root, follow).unwrap(), "Directory");
        assert_eq!(inspect_native_artifact(&root.join("Cargo.toml"), follow).unwrap(), "File");
    }
    assert_eq!(
        inspect_native_artifact(Utf8Path::new("invalid\0artifact"), true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn recovered_json_file_limit_forwards_the_exact_boundary_to_the_reader() {
    let path = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("correctness")
        .join("resource-dependencies.json");
    let bytes = include_bytes!("fixtures/correctness/resource-dependencies.json");
    let limit = u64::try_from(bytes.len()).unwrap();
    let actual = read_native_json_with_limit(&path, limit).unwrap();
    assert_eq!(actual, from_slice::<Value>(bytes).unwrap());
    assert!(
        read_native_json_with_limit(&path, limit - 1)
            .unwrap_err()
            .message()
            .contains("exceeds the supported size")
    );
}

#[test]
fn oct25_prepared_workspace_retains_both_owned_output_lock_handles() {
    let output = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(output.path()).unwrap();
    let workspace_lock = File::create(root.join("workspace.lock")).unwrap();
    let cache_lock = File::create(root.join("cache.lock")).unwrap();
    let workspace = retain_prepared_locks(
        Workspace::adopt(root.to_owned(), root.join("target")),
        workspace_lock,
        Some(cache_lock),
    );
    let (workspace_lock, cache_lock) = retained_cache_locks(&workspace);
    assert!(workspace_lock.as_ref().unwrap().metadata().unwrap().is_file());
    assert!(cache_lock.as_ref().unwrap().metadata().unwrap().is_file());
}

#[test]
fn oct25_cache_claim_propagates_its_actual_owner_publication_error() {
    let output = tempfile::tempdir_in(".").unwrap();
    let base = Utf8Path::from_path(output.path()).unwrap();
    let source = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
    let calls = Cell::new(0);
    let failure = publish_cache_owner(
        source,
        base,
        |_| panic!("the owner does not exist before publication"),
        |seen| {
            assert_eq!(seen, base);
            Ok(false)
        },
        |file, selected, owner| {
            assert_eq!(selected, physical_workspace(source));
            assert_eq!(owner, base.join(".cargo-gamma-owner"));
            assert_eq!(file.metadata().unwrap().len(), 0);
            calls.set(calls.get() + 1);
            Err(Error::new("owned cache publication failure").caused_by(io::Error::from(io::ErrorKind::WriteZero)))
        },
    )
    .unwrap_err();
    assert_eq!(failure.message(), "owned cache publication failure");
    assert_eq!(calls.get(), 1);
    assert_eq!(fs::metadata(base.join(".cargo-gamma-owner")).unwrap().len(), 0);
}

#[cfg(windows)]
#[test]
fn oct25_manifest_anchoring_continues_after_an_unrepresentable_native_path() {
    let output = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(output.path()).unwrap();
    let missing = root.join("missing-manifest.toml");
    let invalid = std::path::PathBuf::from(OsString::from_wide(&[0xd800]));
    let failure = anchor_native_manifest_paths(
        Utf8Path::new(env!("CARGO_MANIFEST_DIR")),
        root,
        root,
        [invalid, missing.clone().into_std_path_buf()],
    )
    .unwrap_err();
    assert_eq!(failure.message(), format!("could not read `{missing}`"));
}

#[test]
fn oct26_manifest_anchoring_continues_after_an_out_of_tree_native_path() {
    let output = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(output.path()).unwrap();
    let missing = root.join("missing-manifest.toml");
    let outside = Utf8Path::new("outside\0manifest").join("Cargo.toml");
    let failure = anchor_native_manifest_paths(
        Utf8Path::new(env!("CARGO_MANIFEST_DIR")),
        root,
        root,
        [outside.into_std_path_buf(), missing.clone().into_std_path_buf()],
    )
    .unwrap_err();
    assert_eq!(failure.message(), format!("could not read `{missing}`"));
}

#[derive(Default)]
struct PreparationEvents {
    calls: Vec<String>,
}

impl Events for PreparationEvents {
    fn phase(&mut self, verb: &str, _: &str) {
        self.calls.push(verb.to_owned());
    }

    fn mutant(&mut self, _: &Mutant) {
        panic!("workspace preparation does not settle mutants");
    }
}

#[test]
fn recovered_preparation_returns_the_default_campaign_guard_error_after_workspace_identity_changes() {
    let source = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).to_owned();
    let moved = source.join("moved-workspace-identity");
    let home = std::env::current_dir().unwrap().join("invalid\0changing-identity-cache");
    let home = Utf8PathBuf::from_path_buf(home).unwrap();
    let before = home.join("cargo-gamma").join(workspace_digest(&source));
    let after = home.join("cargo-gamma").join(workspace_digest(&moved));
    let calls = Rc::new(Cell::new(0));
    let mut events = PreparationEvents::default();
    with_workspace_resolver(
        {
            let calls = Rc::clone(&calls);
            let source = source.clone();
            move |path| {
                assert_eq!(path, source);
                calls.set(calls.get() + 1);
                match calls.get() {
                    1..=3 => source.clone(),
                    4 => moved.clone(),
                    _ => panic!("the rejected campaign identity must stop before any copy or locking"),
                }
            }
        },
        || {
            with_cache_home_at(home, || {
                let Err(failure) = prepare_native_workspace(&source, &source.join("target"), &Config::default(), &mut events, None) else {
                    panic!("changed workspace identity must not prepare a cache");
                };
                assert_eq!(
                    failure.message(),
                    format!("cargo-gamma's self-test campaign cache path `{before}` escaped its private cache base `{after}`")
                );
            });
        },
    );
    assert_eq!(calls.get(), 4);
    assert!(events.calls.is_empty());
}

#[test]
fn recovered_redirected_cache_claim_returns_the_second_guard_error_after_the_native_directory_stage() {
    let base = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("completed-recovery");
    let source = base.join("lib.rs");
    let before = fs::read(&source).unwrap();
    let marker = fs::metadata(&source).unwrap();
    let mut inspections = [fs::metadata(&base).unwrap(), marker.clone()].into_iter();
    let calls = Rc::new(Cell::new(0));
    let mut owners = 0;
    let failure = with_cache_metadata(
        {
            let calls = Rc::clone(&calls);
            let base = base.clone();
            move |path| {
                assert_eq!(path, base);
                calls.set(calls.get() + 1);
                Ok(inspections
                    .next()
                    .expect("redirected claiming must inspect only its pre- and post-creation cache path"))
            }
        },
        || {
            claim_native_redirected_cache(
                &base,
                &base,
                |path| {
                    owners += 1;
                    assert_eq!(path, base.join(".cargo-gamma-owner"));
                    Ok(marker)
                },
                |_| panic!("an inspected existing owner needs no unowned-directory inventory"),
            )
            .unwrap_err()
        },
    );
    assert_eq!(
        failure.message(),
        format!("the cargo-gamma cache at `{base}` is not a directory.\nChoose a directory for cargo-gamma's cache state.")
    );
    assert_eq!((calls.get(), owners), (2, 1));
    assert_eq!(fs::read(&source).unwrap(), before);
}

#[test]
fn recovered_non_utf8_manifest_does_not_hide_the_later_manifest_containment_error() {
    #[cfg(windows)]
    let invalid = OsString::from_wide(&[0xd800]);
    #[cfg(unix)]
    let invalid = OsString::from_vec(vec![0xff]);
    let root = Utf8Path::new("invalid\0manifest-walk");
    let path = root.join("later").join("Cargo.toml");
    let expected = require_native_manifest_within(&path, root).unwrap_err();
    let paths = [root.as_std_path().join(invalid).join("Cargo.toml"), path.into_std_path_buf()];
    let failure = anchor_native_manifest_paths(Utf8Path::new("source"), root, Utf8Path::new("runtime"), paths).unwrap_err();
    assert_eq!(failure.message(), expected.message());
    assert!(failure.message().contains("Cargo.toml"));
}

#[test]
fn recovered_campaign_claim_returns_the_native_workspace_lock_error_before_re_resolving_state() {
    let root = Utf8Path::new("invalid\0campaign-lock-root");
    let mut resolutions = 0;
    let home = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap())
        .unwrap()
        .join("invalid\0campaign-lock-cache");
    with_cache_home_at(home, || {
        let base = native_gamma_base(root);
        let failure = claim_native_state_with(false, root, None, |selected, cache| {
            assert_eq!(selected, root);
            assert!(cache.is_none());
            resolutions += 1;
            Ok((root.to_owned(), "never-read-campaign-base".into()))
        })
        .unwrap_err();
        assert_eq!(
            failure.message(),
            format!("could not create cargo-gamma's workspace cache at `{base}`")
        );
    });
    assert_eq!(resolutions, 1);
}

#[test]
fn oct25_testing_claim_preserves_the_lock_error_before_a_second_resolution() {
    let root = Utf8Path::new("invalid\0testing-lock-root");
    let mut resolutions = 0;
    let home = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap())
        .unwrap()
        .join("invalid\0testing-lock-cache");
    with_cache_home_at(home, || {
        let base = native_gamma_base(root);
        let failure = claim_native_state_with(true, root, None, |selected, cache| {
            assert_eq!(selected, root);
            assert!(cache.is_none());
            resolutions += 1;
            Ok((root.to_owned(), "never-read-testing-base".into()))
        })
        .unwrap_err();
        assert_eq!(
            failure.message(),
            format!("could not create cargo-gamma's workspace cache at `{base}`")
        );
        assert_eq!(
            failure.source().unwrap().downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
    });
    assert_eq!(resolutions, 1);
}

#[test]
fn recovered_manifest_anchoring_returns_the_final_native_configuration_error_when_no_manifests_are_walkable() {
    let root = Utf8Path::new("invalid\0manifest-anchor-root");
    let source = Utf8Path::new("never-read-anchor-source");
    let expected = anchor_native_cargo_config(root, source).unwrap_err().message().to_owned();
    let failure = anchor_native_manifests(source, root, Utf8Path::new("never-read-anchor-runtime")).unwrap_err();
    assert_eq!(failure.message(), expected);
    assert!(failure.message().contains(".cargo"));
}

#[test]
fn recovered_prepared_workspace_retains_both_exact_native_lock_handles() {
    let fixture = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("completed-recovery")
        .join("lib.rs");
    let workspace_lock = File::open(&fixture).unwrap();
    let redirected_lock = workspace_lock.try_clone().unwrap();
    #[cfg(windows)]
    let handles = (workspace_lock.as_raw_handle(), redirected_lock.as_raw_handle());
    let mut workspace = Workspace::adopt("never-prepared-lock-root".into(), "never-written-lock-target".into());
    retain_cache_locks(&mut workspace, workspace_lock, Some(redirected_lock));
    let locks: [_; 2] = retained_cache_locks(&workspace).into();
    #[cfg(windows)]
    {
        assert_eq!(
            (
                locks[0].as_ref().unwrap().as_raw_handle(),
                locks[1].as_ref().unwrap().as_raw_handle()
            ),
            handles
        );
    }
    for handle in locks {
        assert_eq!(
            handle.as_ref().unwrap().metadata().unwrap().len(),
            include_bytes!("fixtures/completed-recovery/lib.rs").len() as u64
        );
    }
}

#[test]
fn recovered_native_preparation_reports_the_exact_retained_lock_identity_before_any_copy_or_log_publication() {
    struct StopBeforeCopy {
        base: Utf8PathBuf,
        phases: Vec<String>,
        logs: usize,
    }
    impl Events for StopBeforeCopy {
        fn phase(&mut self, verb: &str, _detail: &str) {
            self.phases.push(verb.to_owned());
        }
        fn mutant(&mut self, _mutant: &Mutant) {
            panic!("workspace preparation does not settle mutants");
        }
        fn testing_log(&mut self, base: &Utf8Path) -> Result<()> {
            assert_eq!(base, self.base);
            self.logs += 1;
            Err(Error::new("the in-process native preparation observer stops before copying"))
        }
    }
    let fixture_base = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
    let source = fixture_base.join("completed-recovery");
    let before = fs::read(source.join("lib.rs")).unwrap();
    let locks = (File::open(source.join("lib.rs")).unwrap(), None, 73);
    let mut config = Config::default();
    config.cache_dir = Some(fixture_base.clone());
    let mut events = StopBeforeCopy {
        base: fixture_base,
        phases: Vec::new(),
        logs: 0,
    };
    let (result, observed) = observe_workspace_preparation(&source, || {
        prepare_native_workspace(&source, &source.join("target"), &config, &mut events, Some(locks))
    });
    assert_eq!(
        result.unwrap_err().message(),
        "the in-process native preparation observer stops before copying"
    );
    assert_eq!(observed, Some(73));
    assert_eq!(events.phases, ["Copying"]);
    assert_eq!(events.logs, 1);
    assert_eq!(fs::read(source.join("lib.rs")).unwrap(), before);
}

#[test]
fn recovered_cache_link_inspection_returns_the_native_invalid_path_error_without_panicking() {
    let path = Utf8Path::new("invalid\0cache");
    let failure = reject_native_linked_cache(path).unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("could not inspect the cargo-gamma cache at `invalid\0cache`")
    );
}
