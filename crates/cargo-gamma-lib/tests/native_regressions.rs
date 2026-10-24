// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

use core::error::Error as _;
use core::time::Duration;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::internals::cfg::{Build, CfgSet, with_empty_build_environment};
use cargo_gamma_lib::internals::commands::journal_test_util::{Journal, TEST_SELECTION_LOG, TESTING_PROGRESS_LOG};
use cargo_gamma_lib::internals::commands::recovery_test_util::{
    campaign_preflight, configure, load_record, prepared_report_info, publication_lock, report_source_root, resumed_publication,
    resumed_report, resumed_report_sources,
};
use cargo_gamma_lib::internals::commands::suppression_test_util::{outcome_files, partial_plan, physical, suppress};
use cargo_gamma_lib::internals::commands::{RunArgs, SuppressArgs, When};
use cargo_gamma_lib::internals::discover::acceptance::native_test_util::{
    checked_survey, link_resource_dependency, parse_declarations, scan_excluding, scan_with_inactive_declaration, set_dependency_roots,
    set_diff,
};
use cargo_gamma_lib::internals::discover::acceptance::native_timestamp;
use cargo_gamma_lib::internals::discover::{Diff, Plan, RecordContext, RunRecord, TargetFile, record_context};
use cargo_gamma_lib::internals::elements::RunInfo;
use cargo_gamma_lib::internals::elements::native_test_util::remove_if_unchanged_locked;
#[cfg(windows)]
use cargo_gamma_lib::internals::elements::write;
use cargo_gamma_lib::internals::exec::Session;
use cargo_gamma_lib::internals::model::{MUTANT_ID_VERSION, Mutant, Outcome};
use cargo_gamma_lib::internals::ops::Selection;
use cargo_gamma_lib::internals::report::Styler;
use cargo_gamma_lib::internals::{discover, exec, ops, parse};
use cargo_gamma_lib::testing::Sink;
use rustc_hash::FxHashMap as HashMap;
use serde_json::{Value, from_value, json, to_value};
use syn::Item;

fn plan(mutants: Vec<Mutant>) -> Plan {
    Plan {
        root: "workspace".into(),
        files: Vec::new(),
        mutants,
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        digests: HashMap::default(),
        skipped: Vec::new(),
        reach: HashMap::default(),
        specs: HashMap::default(),
    }
}

fn mutant() -> Mutant {
    cargo_gamma_lib::testing::ci_fixture::mutant("src/lib.rs", 1, "relational.lt_to_le", Outcome::Killed)
}

fn session() -> Session {
    Session {
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
        scratch: Utf8PathBuf::new(),
        filtered: 0,
        widened: false,
        ordering: exec::OrderingHints::default(),
        phases: exec::Phases::default(),
    }
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[test]
fn recovered_prepared_record_retains_provenance_without_claiming_a_test_count() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery");
    let mut args = RunArgs::default();
    args.measure.profile = Some("retained-profile".into());
    let mut info = prepared_report_info(&args, root).unwrap();
    assert_eq!(info.tests, None);
    info.started_at = 0;
    assert_eq!(
        to_value(info).unwrap(),
        to_value(RunInfo {
            tests: None,
            started_at: 0,
            mutant_id_version: Some(MUTANT_ID_VERSION),
            merged: false,
            population_complete: true,
            name_filtered: false,
            shard: None,
            not_built: None,
            dropped_test_packages: Vec::new(),
            merge_provenance: None,
        })
        .unwrap()
    );
}

#[test]
fn recovered_publication_only_resume_retains_the_exact_native_workspace_lock() {
    let fixture = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("lib.rs");
    let lock = File::open(&fixture).unwrap();
    let length = lock.metadata().unwrap().len();
    #[cfg(windows)]
    let identity = lock.as_raw_handle();
    let retained = publication_lock(lock).unwrap().unwrap();
    assert_eq!(retained.metadata().unwrap().len(), length);
    #[cfg(windows)]
    assert_eq!(retained.as_raw_handle(), identity);
}

#[test]
fn recovered_native_timestamp_propagates_invalid_path_errors_for_both_follow_modes() {
    let path = Utf8Path::new("invalid\0timestamp");
    let expected = fs::metadata(path).unwrap_err();
    for follow in [false, true] {
        let actual = native_timestamp(path, follow).unwrap_err();
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.raw_os_error(), expected.raw_os_error());
    }
}

#[test]
#[cfg(windows)]
fn recovered_native_conditional_removal_returns_a_sharing_violation_without_modifying_owned_output() {
    const FILE_SHARE_READ: u32 = 1;
    let directory = tempfile::tempdir().unwrap();
    let fixture = Utf8Path::from_path(directory.path()).unwrap().join("owned-publication.txt");
    let expected = "owned native publication output\n";
    write(&fixture, expected).unwrap();
    let _deny_deletion = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&fixture)
        .unwrap();
    let failure = remove_if_unchanged_locked(&fixture, &fixture, expected).unwrap_err();
    assert_eq!(failure.message(), format!("could not remove `{fixture}`"));
    assert_eq!(fs::read(&fixture).unwrap(), expected.as_bytes());
}

#[test]
fn recovered_conditional_removal_returns_invalid_path_read_errors_without_deleting() {
    let path = Utf8Path::new("unreadable\0source.rs");
    let error = remove_if_unchanged_locked(path, path, "expected").unwrap_err();
    assert_eq!(
        error.source().unwrap().downcast_ref::<io::Error>().unwrap().kind(),
        ErrorKind::InvalidInput
    );
    assert!(
        error
            .to_string()
            .starts_with("could not check `unreadable\0source.rs` before removing it:")
    );
}

#[test]
fn oct25_public_build_resolution_preserves_the_requested_profile() {
    for (profile, expected) in [(None, true), (Some("release"), false)] {
        let build = with_empty_build_environment(|| Build::resolve(Utf8Path::new("invalid\0profile-root"), profile, &[]));
        assert_eq!(build.debug_assertions, Some(expected));
    }
}

#[test]
fn recovered_native_suppress_returns_physical_path_errors_before_claiming_campaign_state() {
    let dir = Utf8PathBuf::from("invalid\0suppression-root");
    let expected = physical(&dir).unwrap_err().message().to_owned();
    let args = SuppressArgs {
        dir,
        cache_dir: None,
        apply: false,
        dry_run_suppress: true,
        allow_dirty: false,
        eligible: "timeout".into(),
    };
    let mut host = Sink::default();
    assert_eq!(
        suppress(&mut host, &args, When::Never, Styler::new(false)).unwrap_err().message(),
        expected
    );
    assert!(host.out.is_empty());
    assert!(host.err.is_empty());
}

#[test]
fn recovered_record_planning_continues_past_unaffected_and_missing_sources_to_the_later_eligible_file() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery");
    let text = include_str!("fixtures/completed-recovery/lib.rs");
    let selection = ops::Selection::parse("relational.lt_to_le").unwrap();
    let mut mutants = Vec::new();
    for (file, outcome) in [
        ("00-unaffected.rs", Outcome::Killed),
        ("01-missing.rs", Outcome::Timeout),
        ("lib.rs", Outcome::Timeout),
    ] {
        let parsed = parse::SourceFile::parse(file, text.into()).unwrap();
        let candidates = ops::collect_candidates(&parsed, &selection);
        let mut collected = ops::into_mutants(&parsed, "subject", candidates);
        assert_eq!(collected.len(), 1);
        collected[0].ordinal = u32::try_from(mutants.len() + 1).unwrap();
        collected[0].outcome = outcome;
        mutants.extend(collected);
    }
    let retained_id = mutants[2].id.clone();
    let actual = RunRecord::from_run(
        &root,
        &mutants[2..],
        &record_context(&RecordContext {
            toolchain: Some("fixture"),
            ..RecordContext::default()
        })
        .unwrap(),
        &[root.join("src")],
    );
    let mut value = to_value(actual).unwrap();
    let template = value["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| !file["mutants"].as_array().unwrap().is_empty())
        .unwrap()
        .clone();
    value["files"] = Value::Array(
        mutants
            .iter()
            .map(|mutant| {
                let mut file = template.clone();
                file["path"] = to_value(&mutant.file).unwrap();
                file["mutants"][0]["id"] = to_value(&mutant.id).unwrap();
                file["mutants"][0]["outcome"] = to_value(mutant.outcome).unwrap();
                file
            })
            .collect(),
    );
    let record: RunRecord = from_value(value).unwrap();
    assert_eq!(outcome_files(&record), ["00-unaffected.rs", "01-missing.rs", "lib.rs"]);
    let (plan, stale) = partial_plan(&root, &record).unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].path, Utf8Path::new("lib.rs"));
    assert_eq!(plan.mutants.len(), 1);
    assert_eq!(plan.mutants[0].id, retained_id);
    assert_eq!(plan.mutants[0].outcome, Outcome::Timeout);
    assert_eq!(
        stale,
        ["01-missing.rs: source file is missing; no edit was planned for 1 eligible outcome"]
    );
}

#[test]
fn oct25_journal_resume_propagates_progress_recovery_failure_before_opening_selection_output() {
    let output = tempfile::tempdir_in(".").unwrap();
    let root = Utf8Path::from_path(output.path()).unwrap();
    let calls = std::cell::Cell::new(0);
    let mut log = Journal::default();
    let failure = log
        .resume_with_progress(
            root,
            100,
            |_| panic!("selection inspection must not follow progress failure"),
            |_| panic!("selection metadata must not follow progress failure"),
            |file, path| {
                assert_eq!(path, root.join(TESTING_PROGRESS_LOG));
                assert_eq!(file.metadata().unwrap().len(), 0);
                calls.set(calls.get() + 1);
                Err(cargo_gamma_lib::internals::error::Error::new("owned progress recovery failure")
                    .caused_by(std::io::Error::from(std::io::ErrorKind::InvalidData)))
            },
        )
        .unwrap_err();
    assert_eq!(failure.message(), "owned progress recovery failure");
    assert_eq!(calls.get(), 1);
    assert!(log.is_disabled());
    assert!(root.join(TESTING_PROGRESS_LOG).is_file());
    assert!(!root.join(TEST_SELECTION_LOG).exists());
}

#[test]
fn recovered_native_resume_journal_stages_return_handle_and_reopen_errors_without_writing_any_fixture() {
    let fixture = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("completed-recovery")
        .join("lib.rs");
    let before = std::fs::read(&fixture).unwrap();
    assert_eq!(before.last(), Some(&b'\n'));
    let path = Utf8Path::new("invalid\0native-resume-log");
    let failure = Journal::resume_progress_file(File::open(&fixture).unwrap(), path).unwrap_err();
    assert_eq!(
        failure.message(),
        "could not reopen testing progress log `invalid\0native-resume-log`"
    );
    let failure = Journal::resume_selection_file(File::open(&fixture).unwrap(), path, usize::MAX, |_| {}, File::metadata).unwrap_err();
    assert_eq!(
        failure.message(),
        "could not reopen test-selection log `invalid\0native-resume-log`"
    );
    #[cfg(windows)]
    {
        const FILE_READ_ATTRIBUTES: u32 = 0x0080;
        let attributes_only = File::options().read(true).access_mode(FILE_READ_ATTRIBUTES).open(&fixture).unwrap();
        let failure = Journal::resume_progress_file(attributes_only, path).unwrap_err();
        assert_eq!(
            failure.message(),
            "could not recover testing progress log `invalid\0native-resume-log`; check that it is writable"
        );
    }
    assert_eq!(std::fs::read(&fixture).unwrap(), before);
}

#[test]
fn oct25_resumed_record_publication_preserves_learning_in_real_owned_output() {
    let output = tempfile::tempdir_in(".").unwrap();
    let base = Utf8Path::from_path(output.path()).unwrap();
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery");
    let source = include_str!("fixtures/completed-recovery/lib.rs");
    let mut mutant = mutant();
    mutant.file = std::sync::Arc::from(Utf8Path::new("lib.rs"));
    mutant.outcome = Outcome::Killed;
    let id = mutant.id.clone();
    let mut plan = plan(vec![mutant]);
    plan.root = root.clone();
    plan.files = vec![TargetFile {
        path: "lib.rs".into(),
        absolute: root.join("lib.rs"),
        package: "subject".into(),
        source: Some(source.into()),
    }];
    plan.digests
        .insert("lib.rs".into(), blake3::hash(source.as_bytes()).to_hex().to_string());
    let killer = discover::Killer {
        package: "subject".into(),
        target: "lib".into(),
        test: "owned_killer".into(),
    };
    let calls = std::cell::Cell::new(0);
    let (promotable, suppressible, retained, source_root) = resumed_publication(
        &plan,
        base,
        &[(id.clone(), killer.clone())],
        |selected, published| {
            assert_eq!(selected, root);
            assert_eq!(published, base);
            calls.set(calls.get() + 1);
            false
        },
        Some(session()),
    )
    .unwrap();
    assert!(!promotable);
    assert!(!suppressible);
    assert_eq!(retained.tests, session().tests);
    assert_eq!(source_root, base.join(".cargo-gamma-report-source"));
    assert_eq!(calls.get(), 1);
    let stored = load_record(base).unwrap();
    assert_eq!(stored.probes().get(&id), Some(&killer));
}

#[test]
fn recovered_resumed_report_source_preparation_returns_the_actual_native_snapshot_error() {
    let root = Utf8PathBuf::from("invalid\0report-work");
    let target = Utf8Path::new("invalid\0report-base").join("target");
    let expected = format!(
        "could not inspect report source directory `{}`",
        report_source_root(root.clone(), target.clone())
    );
    let Err(failure) = resumed_report_sources(root, target, session(), &plan(Vec::new())) else {
        panic!("an invalid native snapshot destination must not yield report sources");
    };
    assert_eq!(failure.message(), expected);
}

#[test]
fn recovered_resumed_report_contents_reads_the_captured_root_instead_of_the_changed_checkout() {
    let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("completed-recovery");
    let mut mutant = mutant();
    mutant.file = Utf8PathBuf::from("lib.rs").into();
    let mut plan = plan(vec![mutant]);
    plan.root = root.clone();
    plan.files.push(TargetFile {
        path: "lib.rs".into(),
        absolute: root.join("changed.rs"),
        package: "subject".into(),
        source: None,
    });
    let dropped = vec!["retained-drop".into()];
    let (report, advice) = resumed_report(&plan, &root, "retained-advice", Some(17), &dropped).unwrap();
    let captured = include_str!("fixtures/completed-recovery/lib.rs");
    let changed = include_str!("fixtures/completed-recovery/changed.rs");
    assert_ne!(captured, changed);
    assert_eq!(report.files["lib.rs"].source, captured);
    assert_ne!(report.files["lib.rs"].source, changed);
    assert_eq!(report.files["lib.rs"].mutants[0].id, plan.mutants[0].id);
    assert_eq!(advice.as_deref(), Some("retained-advice"));
    let config = report.config.unwrap();
    assert_eq!(config.tests, Some(17));
    assert_eq!(config.dropped_test_packages, dropped);
}

#[test]
fn recovered_configuration_returns_reporting_read_and_duplicate_resource_errors_without_panicking() {
    let mut host = Sink::default();
    let mut args = RunArgs {
        resume_testing: true,
        ..RunArgs::default()
    };
    args.select.dir = "invalid\0workspace".into();
    args.select.config.path = Some("invalid\0gamma.toml".into());
    let failure = configure(&mut host, &mut args, Styler::new(false)).unwrap_err();
    assert!(failure.to_string().contains("could not read `invalid"), "{failure}");
    let mut args = RunArgs::default();
    args.select.dir = "invalid\0workspace".into();
    args.select.config.no_config = true;
    args.measure.resource_concurrency = vec![
        exec::ResourceLimit::new("database", 1).unwrap(),
        exec::ResourceLimit::new("database", 2).unwrap(),
    ];
    let failure = configure(&mut host, &mut args, Styler::new(false)).unwrap_err();
    assert_eq!(
        failure.message(),
        "`--resource-concurrency` specifies `database` more than once; each resource may have only one command-line capacity"
    );
    assert!(host.out.is_empty());
    assert!(host.err.is_empty());
}

#[test]
fn recovered_campaign_preflight_propagates_default_resolution_and_honors_an_explicit_cache() {
    let mut args = RunArgs::default();
    let failure = campaign_preflight(&args, || {
        Err(cargo_gamma_lib::internals::error::Error::new("default resolution denied"))
    })
    .unwrap_err();
    assert_eq!(failure.message(), "default resolution denied");
    args.select.dir = "invalid\0workspace".into();
    args.measure.cache_dir = Some("invalid\0explicit-cache".into());
    let mut resolutions = 0;
    let failure = campaign_preflight(&args, || {
        resolutions += 1;
        Err(cargo_gamma_lib::internals::error::Error::new(
            "explicit cache must bypass the default resolver",
        ))
    })
    .unwrap_err();
    assert_eq!(resolutions, 0);
    assert!(failure.to_string().contains("explicit-cache"), "{failure}");
}

#[test]
fn recovered_declaration_parsing_returns_module_identity_failures_after_reading_the_exact_source() {
    let path = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery/lib.rs");
    let cfg = CfgSet::unconditional();
    let failure = parse_declarations(&path, &cfg, |seen_path, ast, _| {
        assert_eq!(seen_path, path);
        assert_eq!(ast.items.len(), 1);
        let Item::Fn(function) = &ast.items[0] else {
            panic!("the immutable source contains exactly the less function");
        };
        assert_eq!(function.sig.ident, "less");
        Err(cargo_gamma_lib::internals::error::Error::new("injected module-identity failure"))
    })
    .unwrap_err();
    assert_eq!(failure.message(), "injected module-identity failure");
}

#[test]
fn recovered_scan_applies_compiler_exclusions_before_assigning_live_ordinals() {
    let survey = checked_survey(Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let selection = Selection::parse("relational.lt_to_le").unwrap();
    let initial = survey.scan(None, &selection, &mut 0).unwrap();
    assert_eq!(initial.mutants.len(), 1);
    let excluded = &initial.mutants[0];
    let exclusions = from_value(json!({
        "version": 1,
        "mutants": { excluded.id.to_string(): "exact compiler evidence" },
    }))
    .unwrap();
    let mut ordinal = 19;
    let scanned = scan_excluding(&survey, &selection, &mut ordinal, &exclusions).unwrap();
    assert_eq!(ordinal, 19);
    assert_eq!(scanned.settled_out, 1);
    assert_eq!(scanned.sharded_out, 0);
    assert_eq!(scanned.mutants.len(), 1);
    let mutant = &scanned.mutants[0];
    assert_eq!(mutant.id, excluded.id);
    assert_eq!(mutant.outcome, Outcome::CompileError);
    assert_eq!(mutant.ordinal, 0);
    assert_eq!(
        mutant.note.as_deref(),
        Some("historical compiler exclusion: exact compiler evidence")
    );
}

#[test]
fn recovered_scan_diff_checks_both_exact_extent_boundaries() {
    let mut survey = checked_survey(Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let selection = Selection::parse("relational.lt_to_le").unwrap();
    let initial = survey.scan(None, &selection, &mut 0).unwrap();
    assert_eq!(initial.mutants.len(), 1);
    let mutant = &initial.mutants[0];
    assert_eq!(mutant.line, mutant.end_line);
    for (changed, expected) in [(mutant.line - 1, 0), (mutant.line, 1), (mutant.end_line + 1, 0)] {
        set_diff(
            &mut survey,
            Diff::parse(&format!(
                "--- a/{path}\n+++ b/{path}\n@@ -0,0 +{changed},1 @@\n+changed\n",
                path = mutant.file
            )),
        );
        let scanned = survey.scan(None, &selection, &mut 0).unwrap();
        assert_eq!(scanned.mutants.len(), expected, "changed line {changed}");
    }
}

#[test]
fn oct25_scan_retains_a_selected_root_even_when_another_package_declares_it_inactive() {
    let survey = checked_survey(Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let selected = &survey.files[0];
    let outside = survey.root.join("not-read-declaration-evidence.rs");
    let cfg = CfgSet::default();
    let scanned = scan_with_inactive_declaration(selected, &outside, &cfg, |inputs| {
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].0, outside);
        assert_eq!(inputs[0].2, "beta");
        Ok(())
    })
    .unwrap();
    assert_eq!(scanned.mutants.len(), 1);
    assert_eq!(scanned.mutants[0].file.as_ref(), selected.path.as_path());
    assert_eq!(scanned.mutants[0].mutator.as_ref(), "relational.lt_to_le");
    assert_eq!(scanned.digests.len(), 1);
    assert_eq!(
        scanned.digests[&selected.path],
        digest(include_bytes!("fixtures/completed-recovery/lib.rs"))
    );
}

#[test]
fn recovered_scan_resource_roots_include_only_reachable_package_declarations() {
    let mut survey = checked_survey(Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let dependency = survey.root.join("tests/fixtures/completed-recovery/changed.rs");
    let unrelated = survey.root.join("tests/fixtures/isolation-graph/ordinary/src/lib.rs");
    set_dependency_roots(&mut survey, vec![dependency.clone(), unrelated.clone()], vec![dependency.clone()]);
    let selection = Selection::parse("relational.lt_to_le").unwrap();
    let isolated = survey.scan(Some("alpha"), &selection, &mut 0).unwrap();
    assert_eq!(isolated.mutants.len(), 1);
    assert_eq!(isolated.digests.len(), 1);
    link_resource_dependency(&mut survey, &dependency);
    let linked = survey.scan(Some("alpha"), &selection, &mut 0).unwrap();
    assert_eq!(linked.mutants.len(), 1);
    assert_eq!(linked.digests.len(), 2);
    let relative = dependency.strip_prefix(&survey.root).unwrap();
    assert_eq!(
        linked.digests.get(relative),
        Some(&digest(include_bytes!("fixtures/completed-recovery/changed.rs")))
    );
    assert!(!linked.digests.contains_key(unrelated.strip_prefix(&survey.root).unwrap()));
}

#[test]
fn recovered_scan_external_resource_evidence_returns_a_specific_path_error() {
    let mut survey = checked_survey(Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let dependency = survey.root.join("tests/fixtures/completed-recovery/changed.rs");
    set_dependency_roots(&mut survey, vec![dependency.clone()], vec![dependency.clone()]);
    survey.root = survey.root.join("tests/fixtures/completed-recovery/lib.rs");
    let error = survey
        .scan(None, &Selection::parse("relational.lt_to_le").unwrap(), &mut 0)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        discover::acceptance::native_test_util::resource_path(&survey.root, &dependency)
            .unwrap_err()
            .to_string()
    );
    assert!(error.to_string().contains("package `resource evidence`"), "{error}");
}
