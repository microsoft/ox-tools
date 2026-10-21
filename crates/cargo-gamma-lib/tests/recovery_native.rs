// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

use core::time::Duration;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_lib::Result;
use cargo_gamma_lib::internals::commands::recovery_test_util::*;
use cargo_gamma_lib::internals::commands::{RunArgs, When};
use cargo_gamma_lib::internals::discover::{Plan, TargetFile};
use cargo_gamma_lib::internals::exec::Session;
use cargo_gamma_lib::internals::exec::native_storage_test_util::*;
use cargo_gamma_lib::internals::model::{Mutant, Outcome};
use cargo_gamma_lib::internals::report::Styler;
use cargo_gamma_lib::internals::{commands, discover, exec, model, ops, parse};
use cargo_gamma_lib::testing::{Sink, private_system_tempdir, workdir};
use rustc_hash::FxHashMap as HashMap;

fn survivor_resume_checkpoint() -> (tempfile::TempDir, RunArgs, serde_json::Value) {
    let directory = private_system_tempdir("resumed-command-checkpoint-");
    let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).expect("fixture roots can be normalized"))
        .expect("fixture roots are UTF-8");
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&root)
            .expect("private_system_tempdir created the recovery output directory")
            .permissions()
            .mode()
            & 0o077,
        0,
        "redirected-cache fixtures must not inherit checkout permissions"
    );
    let mut plan = plan_at(&root);
    let text = fs::read_to_string(root.join("src/lib.rs")).expect("actual fixture source exists");
    let parsed = parse::SourceFile::parse("src/lib.rs", text).expect("actual fixture source parses");
    let selection = ops::Selection::parse("all").expect("the built-in selection is valid");
    let candidates = ops::collect_candidates(&parsed, &selection);
    plan.mutants = ops::into_mutants(&parsed, "subject", candidates).into_iter().take(3).collect();
    assert_eq!(plan.mutants.len(), 3);
    for (index, (mutant, outcome)) in plan
        .mutants
        .iter_mut()
        .zip([Outcome::CompileError, Outcome::Timeout, Outcome::Survived])
        .enumerate()
    {
        mutant.ordinal = u32::try_from(index + 1).expect("the fixture has only three mutants");
        mutant.outcome = outcome;
    }
    let base = root.join("target").join("campaign");
    let prepared = base.join("workspace");
    let runtime = base.join("rt");
    create_private_dir(&runtime);
    fs::create_dir_all(base.join("target")).expect("compiled-output fixture directory can be created");
    for file in ["Cargo.toml", "src/lib.rs"] {
        let destination = prepared.join(file);
        fs::create_dir_all(destination.parent().expect("fixture files have parents")).expect("prepared parents can be created");
        fs::copy(root.join(file), &destination).expect("the prepared generation copies actual source");
    }
    mark_cache_owned(&root, &base);
    let mut measured = session();
    measured.tests = Some(3);
    measured.scratch = base.clone();
    measured.filtered = 1;
    measured.widened = true;
    let mut artifacts = Vec::new();
    for name in ["Cargo.toml", "src/lib.rs"] {
        let path = prepared.join(name);
        #[cfg(unix)]
        let executable = fs::metadata(&path).expect("prepared file exists").permissions().mode() & 0o111;
        #[cfg(not(unix))]
        let executable = 0;
        artifacts.push(serde_json::json!({
            "path": path, "digest": digest(&fs::read(&path).expect("prepared bytes are readable")),
            "generated_output": false, "metadata": { "kind": "File", "executable": executable, "link": null }
        }));
    }
    let state = survivor_resume_snapshot(&plan, &measured, &base, &prepared, &runtime, &artifacts);
    let args = RunArgs {
        select: commands::SelectArgs {
            dir: root.clone(),
            ..commands::SelectArgs::default()
        },
        measure: commands::MeasureArgs {
            cache_dir: Some(base.clone()),
            ..commands::MeasureArgs::default()
        },
        resume_testing: true,
        artifact_dir: Some(root.join("reports")),
        diag: true,
        dashboard: true,
        ..RunArgs::default()
    };
    survivor_write_resume_snapshot(&base, &state);
    {
        let (claimed_root, claimed_base, _lock) =
            claim_testing_state(&root, Some(&base)).expect("the fixture's actual testing-state owner and native lock are valid");
        assert!(
            same_existing_path(&claimed_root, &root),
            "claimed root {claimed_root:?} differs from fixture root {root:?}"
        );
        assert!(
            same_existing_path(&claimed_base, &base),
            "claimed base {claimed_base:?} differs from fixture base {base:?}"
        );
    }
    let (loaded, _) = load_testing_state(&base).expect("the checkpoint passes the actual version, identity, confidence and journal reader");
    loaded
        .validate_artifacts()
        .expect("the actual prepared files match the fixture's retained native metadata and bytes");
    (directory, args, state)
}

fn survivor_resume_snapshot(
    plan: &Plan,
    measured: &Session,
    base: &Utf8Path,
    prepared: &Utf8Path,
    runtime: &Utf8Path,
    artifacts: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "version": 2, "tool": env!("CARGO_PKG_VERSION"),
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "plan": plan, "confidences": {}, "session": measured,
        "oracle": { "packages": ["subject"], "package_local": true, "whole_workspace": false },
        "workspace": {
            "base": base, "root": prepared, "target": base.join("target"), "runtime": runtime,
            "libraries": [], "cargo": exec::CargoOptions::default(), "nextest": null, "test_environment": null,
            "scheduler_version": 2,
            "launch": { "loader": null, "stack": "2097152" }, "harness_threads": null,
            "resources": { "capacities": {}, "binaries": {} }
        },
        "settings": { "jobs": 1, "mutators": "all", "timeout_floor": Duration::from_secs(1),
            "confirm": false, "narrowed": false, "incremental": false },
        "killers": empty_execution_killers(),
        "record": {
            "report_info": serde_json::json!({"startedAt": 77, "populationComplete": true}),
            "base": base, "context": discover::record_context(&discover::RecordContext {
                toolchain: Some("fixture"), ..discover::RecordContext::default()
            }).expect("the fixture names its compiler and captures the real inherited environment"),
            "inputs": snapshot_inputs(&plan.root, base), "killers": discover::Killers::default()
        },
        "stuck": [], "dropped": ["missing-fixture-package"], "artifact_roots": [prepared, runtime], "artifacts": artifacts
    })
}

fn survivor_write_resume_snapshot(base: &Utf8Path, state: &serde_json::Value) {
    let bytes = serde_json::to_vec(state).expect("the concrete checkpoint fixture serializes");
    fs::write(base.join("testing-session.json"), &bytes).expect("checkpoint fixture is writable");
    fs::write(base.join("testing-verdicts.jsonl"), "").expect("settled fixture has no uncommitted verdict");
    fs::write(
        base.join("testing-phase.json"),
        serde_json::to_vec(&serde_json::json!({
            "phase": "Testing", "snapshot": digest(&bytes)
        }))
        .expect("phase fixture serializes"),
    )
    .expect("phase fixture is writable");
}

fn recovery_journal(base: &Utf8Path, state: &mut serde_json::Value, finished: bool) {
    #[derive(serde::Serialize)]
    struct Verdict {
        mutant: Mutant,
        probe: Option<discover::Killer>,
    }
    let mut journal = Vec::new();
    for mutant in state["plan"]["mutants"].as_array_mut().expect("fixture has mutants") {
        let verdict = Verdict {
            mutant: serde_json::from_value(mutant.clone()).expect("the checkpoint stores a serialized mutant"),
            probe: None,
        };
        let encoded = serde_json::to_vec(&verdict).expect("journal verdicts contain JSON-compatible values");
        let mut committed = serde_json::to_value(&verdict).expect("journal verdicts contain JSON-compatible values");
        committed["checksum"] = serde_json::json!(digest(&encoded));
        journal.extend_from_slice(&serde_json::to_vec(&committed).expect("committed journal entries contain JSON-compatible values"));
        journal.push(b'\n');
        mutant["outcome"] = serde_json::json!(Outcome::Pending);
    }
    survivor_write_resume_snapshot(base, state);
    fs::write(base.join("testing-verdicts.jsonl"), journal).expect("the private checkpoint directory is writable");
    if finished {
        fs::write(
            base.join("testing-phase.json"),
            serde_json::to_vec(&serde_json::json!({
                "phase": "FinishedTesting",
                "snapshot": digest(&fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above"))
            }))
            .expect("the finished-phase checkpoint contains JSON-compatible values"),
        )
        .expect("the private checkpoint directory is writable");
    }
}

#[test]
fn recovery_old_compiler_policy_remains_resumable_without_restamping_its_exclusions() {
    let mut hasher = blake3::Hasher::new();
    for part in [
        b"tool".as_slice(),
        env!("CARGO_PKG_VERSION").as_bytes(),
        &model::MUTANT_ID_VERSION.to_le_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let legacy_tool = hasher.finalize().to_hex().to_string();
    for pending in [false, true] {
        let (_directory, args, mut state) = survivor_resume_checkpoint();
        let base = args.measure.cache_dir.as_ref().unwrap();
        state["record"]["context"]["tool"] = serde_json::json!(legacy_tool);
        if pending {
            state["plan"]["mutants"][2]["outcome"] = serde_json::json!(Outcome::Pending);
        }
        assert!(
            state["session"]["binaries"].as_array().unwrap().is_empty(),
            "the execution seam launches no processes"
        );
        survivor_write_resume_snapshot(base, &state);
        let old_context: discover::ContextDigest = serde_json::from_value(state["record"]["context"].clone()).unwrap();
        let inputs = state["record"]["inputs"].clone();
        if !pending {
            fs::remove_dir_all(base.join("workspace")).unwrap();
            fs::remove_dir_all(base.join("rt")).unwrap();
        }
        let executed = resume_with_support(
            &mut Sink::default(),
            &args,
            When::Never,
            Styler::new(false),
            || -> Result<(), std::io::Error> { panic!("this unmetered checkpoint requires no execution capabilities") },
        )
        .expect("a compiler-evidence policy change does not invalidate a compatible active checkpoint");
        let plan = executed.plan.unwrap();
        assert_eq!(
            plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
            [
                Outcome::CompileError,
                Outcome::Timeout,
                if pending { Outcome::NoCoverage } else { Outcome::Survived }
            ]
        );
        let published = load_record(base).unwrap();
        assert_eq!(published.context().term(discover::Term::Tool), legacy_tool);
        let current = discover::record_context(&discover::RecordContext {
            toolchain: Some("fixture"),
            ..discover::RecordContext::default()
        })
        .unwrap();
        let old = settled_record_against(&published, &plan.root, &old_context, &inputs).unwrap();
        assert_eq!(old.get(&plan.mutants[0].id).copied(), Some(Outcome::CompileError));
        let fresh = settled_record_against(&published, &plan.root, &current, &inputs).unwrap();
        assert!(
            fresh.is_empty(),
            "publishing old-policy observations must not make their exclusions reusable"
        );
    }
}

#[test]
fn recovery_incompatible_predecessor_refuses_before_discovery_and_preserves_both_generations() {
    let (_directory, mut args, mut state) = survivor_resume_checkpoint();
    let base = args.measure.cache_dir.clone().unwrap();
    recovery_journal(&base, &mut state, true);
    let previous = include_bytes!("fixtures/completed-recovery/legacy-record.json");
    fs::write(base.join("last-gamma-run.json"), previous).unwrap();
    let snapshot = fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above");
    let journal = fs::read(base.join("testing-verdicts.jsonl")).unwrap();
    args.resume_testing = false;
    args.measure.cargo_args = vec!["--definitely-invalid-cargo-option".to_owned()];
    let mut host = Sink::default();
    let Err(failure) = measure(&mut host, &args, When::Never, Styler::new(false)) else {
        panic!("an incompatible predecessor must refuse before cargo discovery");
    };
    let message = failure.to_string();
    assert!(message.contains("unsupported version 10"), "{message}");
    assert!(
        message.contains("separate archive path") && message.contains("--resume-testing"),
        "{message}"
    );
    assert!(host.err().is_empty(), "not even the analysis announcement should precede refusal");
    assert_eq!(fs::read(base.join("last-gamma-run.json")).unwrap(), previous);
    assert_eq!(
        fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above"),
        snapshot
    );
    assert_eq!(fs::read(base.join("testing-verdicts.jsonl")).unwrap(), journal);

    args.resume_testing = true;
    resume(&mut host, &args, When::Never, Styler::new(false)).expect_err("recovery must refuse an unsupported publication");
    fs::rename(base.join("last-gamma-run.json"), base.join("archived-version-10.json")).unwrap();
    fs::remove_dir_all(base.join("workspace")).unwrap();
    fs::remove_dir_all(base.join("rt")).unwrap();
    resume_with_support(
        &mut host,
        &args,
        When::Never,
        Styler::new(false),
        || -> Result<(), std::io::Error> { panic!("publication must not inspect host memory support") },
    )
    .expect("archiving the predecessor unblocks completed-journal publication without execution artifacts");
    assert_eq!(fs::read(base.join("archived-version-10.json")).unwrap(), previous);
    assert_eq!(
        fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above"),
        snapshot
    );
    assert_eq!(fs::read(base.join("testing-verdicts.jsonl")).unwrap(), journal);
    assert_eq!(load_record(&base).unwrap().iter().count(), 3);
}

#[test]
fn recovery_settled_journal_publishes_captured_sources_without_prepared_artifacts_or_host_capabilities() {
    for finished in [false, true] {
        for missing in [false, true] {
            let (_directory, args, mut state) = survivor_resume_checkpoint();
            let base = args.measure.cache_dir.as_ref().unwrap();
            state["session"]["metered"] = serde_json::json!(true);
            state["record"]["context"]["environment"] = serde_json::json!("not-the-current-environment");
            recovery_journal(base, &mut state, finished);
            let journal = fs::read(base.join("testing-verdicts.jsonl")).unwrap();
            if missing {
                fs::remove_dir_all(base.join("workspace")).unwrap();
                fs::remove_dir_all(base.join("rt")).unwrap();
                fs::remove_dir_all(base.join("target")).unwrap();
            } else {
                fs::copy(
                    Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery/changed.toml"),
                    base.join("workspace/Cargo.toml"),
                )
                .unwrap();
            }
            fs::copy(
                Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery/changed.rs"),
                args.select.dir.join("src/lib.rs"),
            )
            .unwrap();
            let mut host = Sink::default();
            let executed = resume_with_support(
                &mut host,
                &args,
                When::Never,
                Styler::new(false),
                || -> Result<(), std::io::Error> { panic!("settled publication has no execution requirements") },
            )
            .expect("the fully replayed checkpoint publishes without constructing an execution workspace");
            let plan = executed.plan.unwrap();
            assert_eq!(
                plan.mutants.iter().map(|mutant| mutant.outcome).collect::<Vec<_>>(),
                [Outcome::CompileError, Outcome::Timeout, Outcome::Survived]
            );
            assert_eq!(
                fs::read_to_string(base.join(".cargo-gamma-report-source/src/lib.rs")).unwrap(),
                include_str!("fixtures/completed-recovery/lib.rs")
            );
            assert_eq!(fs::read(base.join("testing-verdicts.jsonl")).unwrap(), journal);
            assert_eq!(load_record(base).unwrap().iter().count(), 3);
            let phase: serde_json::Value = serde_json::from_slice(&fs::read(base.join("testing-phase.json")).unwrap()).unwrap();
            assert_eq!(phase["phase"], "Completed");
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(args.artifact_dir.as_ref().unwrap().join("gamma-report.json")).unwrap()).unwrap();
            assert_eq!(report["config"]["startedAt"], 77);
        }
    }
}

#[test]
fn recovery_publication_rejects_corrupt_snapshot_journal_identity_and_source_provenance() {
    for case in ["snapshot", "journal", "identity", "source"] {
        let (_directory, args, mut state) = survivor_resume_checkpoint();
        let base = args.measure.cache_dir.as_ref().unwrap();
        match case {
            "identity" => state["record"]["base"] = serde_json::json!(args.select.dir),
            "source" => {
                state["plan"]["files"][0]["source"] = serde_json::json!(include_str!("fixtures/completed-recovery/changed.rs"));
            }
            _ => {}
        }
        recovery_journal(base, &mut state, true);
        let expected = match case {
            "snapshot" => {
                let mut snapshot = fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above");
                snapshot.push(b' ');
                fs::write(base.join("testing-session.json"), snapshot).unwrap();
                "snapshot"
            }
            "journal" => {
                let text = fs::read_to_string(base.join("testing-verdicts.jsonl")).unwrap();
                let mut lines: Vec<serde_json::Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
                lines[0]["checksum"] = serde_json::json!("wrong");
                let mut corrupt = lines.iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n");
                corrupt.push('\n');
                fs::write(base.join("testing-verdicts.jsonl"), corrupt).unwrap();
                "integrity check"
            }
            "identity" => "campaign identity",
            "source" => "captured source generation",
            _ => unreachable!(),
        };
        let Err(failure) = resume_with_support(
            &mut Sink::default(),
            &args,
            When::Never,
            Styler::new(false),
            || -> Result<(), std::io::Error> { panic!("invalid publication must fail before execution checks") },
        ) else {
            panic!("corrupt {case} must not publish");
        };
        assert!(failure.to_string().contains(expected), "{case}: {failure}");
        assert!(!base.join("last-gamma-run.json").exists());
    }
}

#[test]
fn recovery_finished_sweep_with_pending_mutants_reenters_execution_instead_of_publishing() {
    let (_directory, args, mut state) = survivor_resume_checkpoint();
    let base = args.measure.cache_dir.as_ref().unwrap();
    state["session"]["metered"] = serde_json::json!(true);
    recovery_journal(base, &mut state, true);
    fs::write(base.join("testing-verdicts.jsonl"), "").unwrap();
    let (loaded, _) = load_testing_state(base).expect("a finished sweep can legitimately leave pending mutants");
    assert!(!loaded.is_settled());
    assert!(loaded.plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Pending));
    let snapshot = fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above");
    let phase = fs::read(base.join("testing-phase.json")).unwrap();
    let called = std::cell::Cell::new(false);
    let Err(failure) = resume_before_execution(
        &mut Sink::default(),
        &args,
        When::Never,
        Styler::new(false),
        || {
            called.set(true);
            Err(std::io::Error::other("injected execution-support refusal"))
        },
        || panic!("execution support must be checked before configuring execution"),
    ) else {
        panic!("pending execution must honor the injected support refusal");
    };
    assert!(called.get(), "pending work must not take the settled publication-only path");
    assert!(failure.to_string().contains("cannot resume the campaign's memory-controlled tests"));
    assert!(!base.join("last-gamma-run.json").exists());
    assert_eq!(
        fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above"),
        snapshot
    );
    assert_eq!(fs::read(base.join("testing-phase.json")).unwrap(), phase);
    assert!(fs::read(base.join("testing-verdicts.jsonl")).unwrap().is_empty());
}

#[test]
fn recovery_pending_execution_retains_artifact_environment_and_workspace_validation() {
    for case in ["missing", "changed", "environment", "workspace"] {
        let (_directory, args, mut state) = survivor_resume_checkpoint();
        let base = args.measure.cache_dir.as_ref().unwrap();
        state["plan"]["mutants"][2]["outcome"] = serde_json::json!(Outcome::Pending);
        let expected = match case {
            "missing" => {
                fs::remove_dir_all(base.join("workspace")).unwrap();
                "prepared artifact"
            }
            "changed" => {
                fs::copy(
                    Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery/changed.rs"),
                    base.join("workspace/src/lib.rs"),
                )
                .unwrap();
                "prepared artifact"
            }
            "environment" => {
                state["record"]["context"]["environment"] = serde_json::json!("different-environment");
                "inherited environment differs"
            }
            "workspace" => {
                state["workspace"]["target"] = serde_json::json!(base.join("foreign-target"));
                "prepared testing workspace does not belong"
            }
            _ => unreachable!(),
        };
        survivor_write_resume_snapshot(base, &state);
        let snapshot = fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above");
        let phase = fs::read(base.join("testing-phase.json")).unwrap();
        let mut host = Sink::default();
        let Err(failure) = resume(&mut host, &args, When::Never, Styler::new(false)) else {
            panic!("pending {case} validation must refuse before execution");
        };
        assert!(failure.to_string().contains(expected), "{case}: {failure}");
        assert!(host.err().is_empty(), "pending validation precedes the resume announcement");
        assert!(!base.join("last-gamma-run.json").exists());
        assert_eq!(
            fs::read(base.join("testing-session.json")).expect("the checkpoint snapshot was published above"),
            snapshot
        );
        assert_eq!(fs::read(base.join("testing-phase.json")).unwrap(), phase);
    }
}

#[test]
fn recovery_preflight_checks_the_resolved_default_campaign_but_not_dry_runs() {
    let (_directory, mut args, _) = survivor_resume_checkpoint();
    let base = args.measure.cache_dir.take().unwrap();
    args.resume_testing = false;
    fs::write(
        base.join("last-gamma-run.json"),
        include_bytes!("fixtures/completed-recovery/legacy-record.json"),
    )
    .unwrap();
    let called = std::cell::Cell::new(false);
    let error = campaign_preflight(&args, || {
        called.set(true);
        Ok(base.clone())
    })
    .unwrap_err();
    assert!(called.get());
    assert!(error.to_string().contains("unsupported version 10"));
    args.dry_run = true;
    campaign_preflight(&args, || panic!("a dry run has no completed ledger to overwrite")).unwrap();
}

fn plan_at(root: &Utf8Path) -> Plan {
    let fixture = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery");
    fs::create_dir_all(root.join("src")).expect("the private recovery output directory is writable");
    fs::copy(fixture.join("Cargo.toml"), root.join("Cargo.toml"))
        .expect("the immutable recovery manifest can be copied into private output");
    fs::copy(fixture.join("lib.rs"), root.join("src/lib.rs")).expect("the immutable recovery source can be copied into private output");
    Plan {
        skipped: Vec::new(),
        digests: HashMap::from_iter([("src/lib.rs".into(), digest(include_bytes!("fixtures/completed-recovery/lib.rs")))]),
        root: root.to_owned(),
        files: vec![TargetFile {
            path: "src/lib.rs".into(),
            absolute: root.join("src/lib.rs"),
            package: "subject".into(),
            source: Some(include_str!("fixtures/completed-recovery/lib.rs").to_owned()),
        }],
        mutants: Vec::new(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        reach: HashMap::default(),
        specs: HashMap::default(),
    }
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn session() -> Session {
    Session {
        census: Vec::new(),
        baseline: Duration::from_millis(20),
        baseline_wall: Duration::from_millis(20),
        tests: None,
        quiet: Duration::from_millis(10),
        stall: Some(Duration::from_millis(30)),
        build: Duration::from_millis(40),
        metered: false,
        unbounded: None,
        withdrawn: 0,
        rounds: 1,
        rounds_taken: Vec::new(),
        binaries: Vec::new(),
        peak: None,
        scratch: Utf8PathBuf::new(),
        filtered: 0,
        widened: false,
        ordering: exec::OrderingHints::default(),
        phases: exec::Phases::default(),
    }
}

#[test]
fn confirmation_deltas_survive_accounting_only_checkpoints_and_legacy_snapshots() {
    use exec::{SweepAccounting, SweepCost};

    let (_directory, args, mut state) = survivor_resume_checkpoint();
    let base = args.measure.cache_dir.as_ref().unwrap();
    let population: Plan = serde_json::from_value(state["plan"].clone()).unwrap();
    let first = population.mutants[0].id.clone();
    let second = population.mutants[1].id.clone();
    state["session"]["phases"]["sweep"] = serde_json::to_value(SweepCost {
        confirmed_initial_killers: [first.clone()].into(),
        accounting: Some(SweepAccounting {
            complete: true,
            initial_killer_confirmed: Some(1),
            ..SweepAccounting::default()
        }),
        ..SweepCost::default()
    })
    .unwrap();
    survivor_write_resume_snapshot(base, &state);
    for confirmed in [vec![second.clone()], Vec::new(), vec![first.clone()], Vec::new()] {
        publish_journal_cost(
            base,
            SweepCost {
                confirmed_initial_killers: confirmed.into_iter().collect(),
                launches: 7,
                accounting: Some(SweepAccounting {
                    complete: true,
                    initial_killer_confirmed: Some(2),
                    ..SweepAccounting::default()
                }),
                ..SweepCost::default()
            },
        )
        .unwrap();
    }
    let path = base.join("testing-verdicts.jsonl");
    let bytes = fs::read(&path).unwrap();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        assert_eq!(roundtrip_journal(line, &path).unwrap(), line);
    }
    let (plan, cost, valid) = load_checkpoint_cost(base).unwrap();
    assert_eq!(valid, bytes.len() as u64);
    assert!(plan.mutants.iter().all(|mutant| mutant.outcome != Outcome::Killed));
    let cost = cost.unwrap();
    assert_eq!(cost.confirmed_initial_killers, [first, second].into());
    assert_eq!(cost.launches, 7, "only identities are deltas; counters remain cumulative snapshots");
    assert_eq!(cost.accounting.as_ref().unwrap().initial_killer_confirmed, Some(2));
    assert!(cost.accounting.as_ref().unwrap().complete);
    assert_eq!(
        checkpoint_resumed_cost(
            plan.root,
            base,
            cost.clone(),
            &SweepCost {
                launches: 3,
                accounting: Some(SweepAccounting {
                    complete: true,
                    initial_killer_confirmed: Some(2),
                    ..SweepAccounting::default()
                }),
                ..SweepCost::default()
            }
        )
        .unwrap(),
        cost.confirmed_initial_killers,
    );
    let bytes = fs::read(&path).unwrap();
    let last = bytes.split(|byte| *byte == b'\n').rfind(|line| !line.is_empty()).unwrap();
    assert!(accounting_cost(last, &path).unwrap().confirmed_initial_killers.is_empty());
    let (_, resumed, _) = load_checkpoint_cost(base).unwrap();
    let resumed = resumed.unwrap();
    assert_eq!(resumed.confirmed_initial_killers, cost.confirmed_initial_killers);
    assert_eq!(resumed.launches, 10);
    assert!(resumed.accounting.unwrap().complete);
}

#[test]
fn partial_exact_id_ledger_cannot_suppress_unrecorded_same_mutator_sites() {
    use commands::SuppressArgs;
    use commands::suppression_test_util::{apply_partial_plan, partial_plan};
    use discover::{ContextDigest, RunRecord};

    let directory = workdir("suppress-partial-population-");
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let path = root.join("src/lib.rs");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let fixture = Utf8Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../cargo-gamma-engine/src/ops/collect/collector/resource/fixtures/campaign/report/text.rs");
    fs::copy(&fixture, &path).unwrap();
    let source = fs::read_to_string(&fixture).unwrap();
    let parsed = parse::SourceFile::parse("src/lib.rs", source.clone()).unwrap();
    let selection = ops::Selection::parse("arith.add_to_sub").unwrap();
    let candidates = ops::into_mutants(&parsed, "subject", ops::collect_candidates(&parsed, &selection));
    let first = candidates
        .iter()
        .find(|candidate| candidates.iter().filter(|other| other.line == candidate.line).count() > 1)
        .expect("the immutable source has two additions in one syntactic scope");
    let mut observed = first.clone();
    observed.outcome = Outcome::Timeout;
    let record = RunRecord::from_run(&root, &[observed], &ContextDigest::default(), &[root.join("src")]);
    let (plan, stale) = partial_plan(&root, &record).unwrap();
    assert!(stale.is_empty());
    assert!(plan.mutants.len() > 1);
    assert_eq!(plan.mutants.iter().filter(|mutant| mutant.outcome == Outcome::Timeout).count(), 1);
    assert!(
        plan.mutants
            .iter()
            .any(|mutant| mutant.outcome == Outcome::Pending && mutant.line == first.line)
    );
    let args = SuppressArgs {
        dir: root.clone(),
        cache_dir: Some(root),
        apply: true,
        dry_run_suppress: false,
        allow_dirty: true,
        eligible: "timeout".into(),
    };
    let mut host = Sink::default();
    let failure = apply_partial_plan(&mut host, &args, &plan, &record).unwrap_err();
    let message = failure.to_string();
    assert!(message.contains("reverted"), "{message}");
    assert!(
        message.contains("additional mutant") && message.contains("syntactic scopes"),
        "{message}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), source);
}
