// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(miri))]

use core::time::Duration;
use std::fs;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt as _;

use camino::{Utf8Path, Utf8PathBuf};
use cargo_gamma_engine::ops::collect::Shape;
use cargo_gamma_lib::internals::discover;
use cargo_gamma_lib::internals::discover::{Plan, TargetFile};
use cargo_gamma_lib::internals::exec::build_test_util::*;
use cargo_gamma_lib::internals::exec::{BuildLimits, Events, Workspace};
use cargo_gamma_lib::internals::model::{Mutant, Outcome};
use cargo_gamma_lib::internals::ops::{Selection, collect_candidates, into_mutants};
use cargo_gamma_lib::internals::parse::SourceFile;
use cargo_gamma_lib::testing::{self, Recorder};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use serde_json::Value;

macro_rules! error {
    ($($args:tt)*) => {
        cargo_gamma_lib::internals::error::Error::new(format!($($args)*))
    };
}

#[path = "native_build/isolation.rs"]
mod isolation;

fn scripted_output(success: bool, stdout: &str, stderr: &str) -> std::process::Output {
    #[cfg(unix)]
    let status = std::process::ExitStatus::from_raw(if success { 0 } else { 256 });
    #[cfg(windows)]
    let status = std::process::ExitStatus::from_raw(u32::from(!success));
    std::process::Output {
        status,
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn scripted_limits() -> BuildLimits {
    BuildLimits {
        timeout: Some(Duration::from_millis(19)),
        multiplier: Some(3.0),
        rollback_rounds: 2,
    }
}

fn trivial_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
    let directory = testing::workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(directory.path().join("schema")).expect("the private output directory is UTF-8");
    fs::create_dir_all(root.join("src")).expect("the private schema output directory is writable");
    let manifest = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/completed-recovery/Cargo.toml");
    fs::copy(manifest, root.join("Cargo.toml")).expect("the immutable manifest can be copied into private schema output");
    let target = Utf8PathBuf::from_path_buf(directory.path().join("target")).expect("the private target output directory is UTF-8");
    (directory, Workspace::adopt(root, target))
}

fn probe_plan(work: &Workspace, unviable: usize, viable: usize) -> Plan {
    assert_eq!(unviable, 0, "these compiler-script scenarios do not need generated const fixtures");
    let original = Utf8Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/correctness/type_evidence.rs");
    let source = SourceFile::parse("src/lib.rs", include_str!("fixtures/correctness/type_evidence.rs").to_owned())
        .expect("the immutable type-evidence source is valid Rust");
    let selection = Selection::parse("literal.int_to_zero").expect("the literal selector is registered");
    let mut plan = empty_plan(work);
    let mut spans = HashSet::default();
    plan.mutants = into_mutants(&source, "trivial", collect_candidates(&source, &selection))
        .into_iter()
        .filter(|mutant| spans.insert((mutant.span.start, mutant.span.end)))
        .take(viable)
        .enumerate()
        .map(|(index, mutant)| Mutant {
            ordinal: u32::try_from(index + 1).expect("the immutable source has fewer than u32::MAX mutation sites"),
            item_path: "pkg::f".into(),
            mutator: "lit.true_to_false".into(),
            ..mutant
        })
        .collect();
    assert_eq!(
        plan.mutants.len(),
        viable,
        "the immutable source must supply every requested proof site"
    );
    fs::copy(&original, work.test_root().join("src/lib.rs")).expect("the immutable source can be copied into private schema output");
    plan.files.push(TargetFile {
        path: "src/lib.rs".into(),
        absolute: original,
        package: "trivial".into(),
        source: None,
    });
    plan
}

fn marked_native_plan(work: &Workspace, count: usize) -> Plan {
    let mut plan = probe_plan(work, 0, count);
    for mutant in &mut plan.mutants {
        mutant.replacement = format!("regression_marker_{}_end", mutant.ordinal).into();
    }
    plan
}

fn native_marker_diagnostic(work: &Workspace, ordinal: u32) -> String {
    let text = fs::read_to_string(work.test_root().join("src/lib.rs"))
        .expect("marked_native_plan writes src/lib.rs before invoking the compiler callback");
    let marker = format!("regression_marker_{ordinal}_end");
    let offset = text
        .find(&marker)
        .expect("the real instrumented schema must contain the blamed replacement");
    let prefix = &text[..offset];
    let line = u32::try_from(prefix.bytes().filter(|byte| *byte == b'\n').count())
        .expect("the bounded regression fixture contains fewer than u32::MAX lines")
        + 1;
    let column = u32::try_from(prefix.rsplit_once('\n').map_or(prefix, |(_before, tail)| tail).chars().count())
        .expect("each bounded regression fixture line contains fewer than u32::MAX characters")
        + 1;
    let mut message: Value = serde_json::from_str(&reason_message(
        Some("E0425"),
        "unresolved replacement",
        &[span("src/lib.rs", line, column, line, column + 1, true)],
        &[],
    ))
    .expect("coded_message serializes a JSON Value, so its output is valid JSON");
    message["target"] = serde_json::json!({"name":"subject", "kind":["lib"]});
    message.to_string()
}

fn native_target_discovery(work: &mut Workspace) -> String {
    let root = work.test_root();
    assert!(root.is_absolute(), "the native fixture root must already be absolute: {root}");
    let output = root.join("target/deps");
    let captures = root.join("captures");
    fs::create_dir_all(&output).expect("the fixture's writable root permits creating its native output directory");
    fs::create_dir(&captures).expect("each fixture uses a fresh root with no prior capture generation");
    let source = root.join("tests/integration.rs");
    fs::write(
        output.join("integration-native.d"),
        format!("artifact: {source} {}\n", root.join("src/lib.rs")),
    )
    .expect("the fixture created the native output directory immediately above");
    let capture = serde_json::json!({
        "crate_name": "integration", "crate_types": ["bin"], "test": true,
        "source": source, "out_dir": output, "extra_filename": "-native",
        "externs": [], "opaque_extern": false,
    });
    fs::write(
        captures.join("unit.json"),
        serde_json::to_vec(&capture).expect("capture fields are all JSON-compatible strings, arrays and booleans"),
    )
    .expect("the fresh fixture capture directory is writable");
    work.test_set_rustc_captures(captures.into());
    let executable = output.join(format!("integration-native{}", std::env::consts::EXE_SUFFIX));
    serde_json::json!({
        "reason":"compiler-artifact", "package_id":"path+file:///fixture#trivial@0.0.0",
        "profile":{"test":true},
        "target":{"name":"integration","kind":["test"],"src_path":source},
        "filenames":[executable], "executable":executable,
    })
    .to_string()
}

fn split_native_isolation_plan(work: &Workspace) -> Plan {
    let mut plan = marked_native_plan(work, 3);
    let template = plan.files[0].clone();
    plan.files.clear();
    for name in ["first", "second"] {
        let mut file = template.clone();
        file.path = format!("src/{name}.rs").into();
        fs::copy(&file.absolute, work.test_root().join(&file.path)).expect("the immutable source can be copied into private schema output");
        plan.files.push(file);
    }
    plan.mutants[0].file = plan.files[0].path.clone().into();
    for mutant in &mut plan.mutants[1..] {
        mutant.file = plan.files[1].path.clone().into();
    }
    plan
}

fn unpositioned_native_context(name: &str) -> String {
    serde_json::json!({
        "reason":"compiler-message", "package_id":"path+file:///fixture#trivial@0.0.0",
        "target":{"name":name,"kind":["test"]},
        "message":{"level":"error","code":{"code":"E0308"},"message":"incompatible complete context",
            "spans":[{"file_name":format!("src/{name}.rs"),"line_start":999,"column_start":1,
                "line_end":999,"column_end":2,"is_primary":true}]}
    })
    .to_string()
}

fn empty_plan(work: &Workspace) -> Plan {
    Plan {
        skipped: Vec::new(),
        digests: HashMap::default(),
        root: work.test_root(),
        files: Vec::new(),
        mutants: Vec::new(),
        suppressed: 0,
        idle: Vec::new(),
        sharded_out: 0,
        settled_out: 0,
        reach: HashMap::default(),
        specs: HashMap::default(),
    }
}

fn mutant() -> Mutant {
    cargo_gamma_lib::testing::ci_fixture::mutant("src/lib.rs", 1, "literal.int_to_zero", Outcome::Pending)
}

fn span(file: &str, line_start: u32, column_start: u32, line_end: u32, column_end: u32, primary: bool) -> Value {
    serde_json::json!({
        "file_name": file,
        "line_start": line_start,
        "column_start": column_start,
        "line_end": line_end,
        "column_end": column_end,
        "is_primary": primary,
    })
}

fn reason_message(code: Option<&str>, message: &str, spans: &[Value], notes: &[Value]) -> String {
    serde_json::json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": message,
            "code": code.map(|code| serde_json::json!({ "code": code })),
            "rendered": "error: boom\n",
            "spans": spans,
            "children": [{ "level": "note", "spans": notes }],
        },
    })
    .to_string()
}

/// The mutants' outcomes after a build, keyed by ordinal, which is the population a hint may not move.
fn population(plan: &Plan) -> Vec<(u32, Outcome)> {
    let mut outcomes: Vec<(u32, Outcome)> = plan.mutants.iter().map(|mutant| (mutant.ordinal, mutant.outcome)).collect();

    outcomes.sort_unstable_by_key(|(ordinal, _outcome)| *ordinal);
    outcomes
}

fn causal_fixture_plan(work: &Workspace, fixture: &str, edits: &[(&str, &str, Shape)]) -> Plan {
    let original = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("exec")
        .join("build")
        .join("fixtures")
        .join(fixture);
    let text = fs::read_to_string(&original).expect("each named checked-in causal fixture exists and contains UTF-8 Rust source");
    fs::copy(&original, work.test_root().join("src/lib.rs"))
        .expect("the checked-in fixture exists and causal_fixture_workspace created its writable destination directory");
    let mut plan = empty_plan(work);
    plan.files.push(TargetFile {
        path: "src/lib.rs".into(),
        absolute: original,
        package: "trivial".into(),
        source: None,
    });
    let _ = plan.specs.insert("trivial".into(), (Utf8PathBuf::new(), "0.0.0".into()));
    for (index, (needle, replacement, shape)) in edits.iter().enumerate() {
        let start = text.find(needle).expect("each edit names an immutable fixture site");
        plan.mutants.push(Mutant {
            id: format!("causal-{index}").into(),
            ordinal: u32::try_from(index + 1).expect("each causal regression supplies only three edits, well within u32"),
            package: "trivial".into(),
            span: start..start + needle.len(),
            original: (*needle).into(),
            replacement: (*replacement).into(),
            shape: *shape,
            item_path: "trivial::subject".into(),
            ..mutant()
        });
    }
    plan
}

fn causal_fixture_workspace(prefix: &str) -> (tempfile::TempDir, Workspace) {
    let directory = testing::workdir(prefix);
    let root = Utf8PathBuf::from_path_buf(directory.path().join("tree"))
        .expect("workdir uses the UTF-8 Cargo manifest directory and an ASCII-generated child name");
    fs::create_dir_all(root.join("src")).expect("workdir created a writable private fixture directory");
    let target = Utf8PathBuf::from_path_buf(directory.path().join("target"))
        .expect("workdir uses the UTF-8 Cargo manifest directory and an ASCII-generated child name");
    (directory, Workspace::adopt(root, target))
}

#[derive(Default)]
struct NativeConvergenceEvents {
    publish_evidence: bool,
    progress: Vec<usize>,
    activity: Vec<(u32, usize, usize)>,
    build_progress: Vec<String>,
    evidence: Vec<(u32, Vec<String>, Vec<String>)>,
}

impl Events for NativeConvergenceEvents {
    fn phase(&mut self, _verb: &str, _detail: &str) {}
    fn mutant(&mut self, _mutant: &Mutant) {}
    fn build_progress(&mut self, text: &str) {
        self.build_progress.push(text.to_owned());
    }
    fn convergence_progress(&mut self, count: usize) {
        self.progress.push(count);
    }
    fn convergence_activity(&mut self, round: u32, provisional: usize, proof: usize) {
        self.activity.push((round, provisional, proof));
    }
    fn wants_convergence_evidence(&self) -> bool {
        self.publish_evidence
    }
    fn convergence_evidence(
        &mut self,
        round: u32,
        _written: &[&std::path::Path],
        _fresh: usize,
        _rebuilt: usize,
        rebuilt: &[String],
        failed: &[String],
    ) {
        assert!(self.publish_evidence, "evidence must not be published to an opted-out consumer");
        self.evidence.push((round, rebuilt.to_vec(), failed.to_vec()));
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the in-process regression checks the full pristine-to-codegen sequence and source preservation"
)]
fn frozen_exclusion_context_reuses_preflight_and_preserves_disconnected_source_during_proofs() {
    use std::cell::Cell;
    use std::rc::Rc;

    for connected in [false, true] {
        let expected_calls = if connected { 6 } else { 5 };
        let (_directory, mut work) = trivial_workspace("fixed-exclusion-context");
        work.test_cargo_mut().features.push("--all-features".into());
        let mut plan = split_native_isolation_plan(&work);
        for mutant in &mut plan.mutants[1..] {
            mutant.package = "independent".into();
        }
        plan.mutants.push(Mutant {
            id: "historical".into(),
            ordinal: 0,
            outcome: Outcome::CompileError,
            ..plan.mutants[0].clone()
        });
        plan.reach.insert("independent".into(), HashSet::from_iter(["independent".into()]));
        plan.reach.insert(
            "trivial".into(),
            if connected {
                HashSet::from_iter(["trivial".into(), "independent".into()])
            } else {
                HashSet::from_iter(["trivial".into()])
            },
        );
        let roots = ["trivial".to_owned(), "independent".to_owned()];
        let excluded_id = plan.mutants[0].id.clone();
        let mut converger = Converger::default();
        converger.freeze_context(Some(&roots));
        converger.historical_exclusions = 1;
        converger.compiler_exclusions = Some(discover::CompilerExclusions::load(work.test_base(), &work.test_root()).unwrap());
        let calls = Rc::new(Cell::new(0));
        let independent = work.test_root().join("src/second.rs");
        let mut preserved = None;
        let mut events = NativeConvergenceEvents::default();
        let built = with_compiler(
            {
                let calls = Rc::clone(&calls);
                move |work, arguments, _, _| {
                    let call = calls.get() + 1;
                    calls.set(call);
                    assert!(
                        call <= expected_calls,
                        "connected remaining candidates may need one extra proof; pristine validation is never repeated"
                    );
                    assert_eq!(arguments[0], if call == expected_calls { "build" } else { "check" });
                    assert!(arguments.iter().any(|arg| arg == "--tests"));
                    assert!(arguments.iter().any(|arg| arg == "--all-features"));
                    assert!(!arguments.iter().any(|arg| arg == "--test" || arg == "--workspace"));
                    for root in ["trivial", "independent"] {
                        assert!(arguments.windows(2).any(|pair| pair == ["--package", root]));
                    }
                    let first = fs::read_to_string(work.test_root().join("src/first.rs")).unwrap();
                    let second = fs::read(&independent).unwrap();
                    let modified = fs::metadata(&independent).unwrap().modified().unwrap();
                    if call == 1 {
                        assert!(!first.contains("regression_marker_1_end"));
                    } else if call == 2 {
                        preserved = Some((second.clone(), modified));
                    } else if !connected {
                        assert_eq!(
                            preserved.as_ref(),
                            Some(&(second.clone(), modified)),
                            "proofs must not rewrite a disconnected crate"
                        );
                    }
                    if call == 3 {
                        assert_eq!(String::from_utf8(second).unwrap().contains("regression_marker_2_end"), !connected);
                    }
                    if call >= 4 {
                        let inventory = discover::CompilerExclusions::load(work.test_base(), &work.test_root()).unwrap();
                        assert!(
                            inventory
                                .reason(&Mutant {
                                    id: excluded_id.clone(),
                                    ordinal: 1,
                                    ..mutant()
                                })
                                .is_some(),
                            "a singleton must be persisted before the next Cargo invocation"
                        );
                    }
                    let failed = call != 1 && first.contains("regression_marker_1_end");
                    let diagnostic = if failed {
                        unpositioned_native_context("first")
                    } else {
                        String::new()
                    };
                    Ok(Some(scripted_output(!failed, &diagnostic, "")))
                }
            },
            || {
                Converger::preflight(&work, &plan, Some(&roots), &roots, false, scripted_limits(), &mut events).unwrap();
                converger.finish(&work, &mut plan, None, scripted_limits(), &mut events)
            },
        )
        .unwrap();
        assert_eq!(calls.get(), expected_calls);
        assert!(built.stuck.is_none());
        assert!(!built.widened);
        assert_eq!(events.progress, [2]);
        assert_eq!(built.withdrawn, 2);
        assert_eq!(built.census.iter().map(|entry| entry.mutants).sum::<usize>(), 2);
        assert_eq!(plan.mutants[0].outcome, Outcome::CompileError);
        assert!(plan.mutants[1..3].iter().all(|mutant| mutant.outcome == Outcome::Pending));
        assert_eq!(plan.mutants[3].outcome, Outcome::CompileError);
    }
}

#[test]
fn finishing_keeps_check_withdrawals_and_frozen_roots_when_codegen_refuses_transport() {
    for whole_workspace in [false, true] {
        let (_directory, work) = trivial_workspace("native-finish-transport");
        let mut plan = marked_native_plan(&work, 2);
        let mut converger = Converger::default();
        let selected = ["trivial".into()];
        converger.freeze_context((!whole_workspace).then_some(selected.as_slice()));
        let mut events = NativeConvergenceEvents::default();
        let mut calls = 0;
        let result = with_compiler(
            move |work, args, _budget, _| {
                calls += 1;
                assert!(calls <= 3, "transport refusal must stop the frozen-context build");
                assert_eq!(args[0], if calls == 3 { "build" } else { "check" });
                assert_eq!(args.windows(2).any(|pair| pair == ["--package", "trivial"]), !whole_workspace);
                assert!(args.iter().any(|arg| arg == "--tests"));
                assert!(!args.iter().any(|arg| arg == "--test"));
                let source = fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap();
                assert_eq!(source.contains("regression_marker_1_end"), calls == 1);
                assert!(source.contains("regression_marker_2_end"));
                if calls == 3 {
                    return Err(error!("late frozen-context codegen transport refusal"));
                }
                let stdout = if calls == 1 {
                    native_marker_diagnostic(work, 1)
                } else {
                    String::new()
                };
                Ok(Some(scripted_output(calls == 2, &stdout, "")))
            },
            || {
                converger.finish(
                    &work,
                    &mut plan,
                    Some(&["not-the-frozen-root".into()]),
                    scripted_limits(),
                    &mut events,
                )
            },
        );
        assert_eq!(result.unwrap_err().message(), "late frozen-context codegen transport refusal");
        assert_eq!(events.progress, [1]);
        assert_eq!(events.activity, [(1, 0, 0), (2, 1, 0), (3, 1, 0)]);
        let source = fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap();
        assert!(!source.contains("regression_marker_1_end"));
        assert!(source.contains("regression_marker_2_end"));
    }
}

#[test]
fn finishing_check_and_build_publish_growth_once_for_narrow_and_whole_scopes_including_an_unchanged_census() {
    for whole in [false, true] {
        for selected in [false, true] {
            for known in [false, true] {
                let (_directory, mut work) = trivial_workspace("native-finish-progress");
                let mut plan = marked_native_plan(&work, 2);
                let discovery = native_target_discovery(&mut work);
                let mut converger = Converger::default();
                if whole {
                    converger.require_whole_workspace();
                }
                if known {
                    let _ = converger.census.insert(1, CompilerReason::isolated());
                }
                let mut events = NativeConvergenceEvents::default();
                let mut calls = [0; 2];
                let built = with_compiler(
                    move |work, args, _, _| {
                        let stage = usize::from(args[0] == "build");
                        calls[stage] += 1;
                        assert!(calls[stage] <= 2);
                        let stdout = if calls[stage] == 1 {
                            native_marker_diagnostic(work, u32::try_from(stage + 1).unwrap())
                        } else if stage == 0 {
                            discovery.clone()
                        } else {
                            String::new()
                        };
                        Ok(Some(scripted_output(calls[stage] == 2, &stdout, "")))
                    },
                    || {
                        converger.finish(
                            &work,
                            &mut plan,
                            selected.then_some(["trivial".into()].as_slice()),
                            scripted_limits(),
                            &mut events,
                        )
                    },
                )
                .unwrap();
                assert_eq!(events.progress, if known { vec![2] } else { vec![1, 2] });
                assert_eq!(built.rounds, 4);
                assert_eq!(built.withdrawn, 2);
                assert!(!built.widened);
                assert!(built.stuck.is_none());
                assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::CompileError));
            }
        }
    }
}

#[test]
fn finishing_a_failed_fixed_scope_retains_its_published_verdicts_without_widening() {
    for failed_stage in ["check", "build"] {
        let (_directory, mut work) = trivial_workspace("native-finish-wide-state");
        let mut plan = marked_native_plan(&work, 2);
        let discovery = native_target_discovery(&mut work);
        let mut events = NativeConvergenceEvents::default();
        let mut narrow_calls = 0;
        let mut wide_calls = 0;
        let built = with_compiler(
            move |work, args, _, _| {
                if args[0] != failed_stage {
                    return Ok(Some(scripted_output(true, if args[0] == "check" { &discovery } else { "" }, "")));
                }
                let narrow = args.iter().any(|arg| arg == "--package");
                let source = fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap();
                if narrow {
                    narrow_calls += 1;
                    assert!(narrow_calls <= 2);
                    assert_eq!(source.contains("regression_marker_1_end"), narrow_calls == 1);
                    Ok(Some(scripted_output(
                        false,
                        &if narrow_calls == 1 {
                            native_marker_diagnostic(work, 1)
                        } else {
                            String::new()
                        },
                        "narrow-context failure",
                    )))
                } else {
                    wide_calls += 1;
                    assert!(wide_calls <= 2);
                    assert!(
                        source.contains("regression_marker_1_end"),
                        "the narrow verdict must be rolled back before widening"
                    );
                    Ok(Some(scripted_output(
                        wide_calls == 2,
                        &if wide_calls == 1 {
                            native_marker_diagnostic(work, 2)
                        } else {
                            discovery.clone()
                        },
                        "",
                    )))
                }
            },
            || Converger::default().finish(&work, &mut plan, Some(&["trivial".into()]), scripted_limits(), &mut events),
        )
        .unwrap();
        assert_eq!(events.progress, [1]);
        assert!(!built.widened);
        assert!(built.stuck.is_some());
        assert_eq!(built.rounds, if failed_stage == "check" { 2 } else { 3 });
        assert_eq!(built.withdrawn, 1);
        assert_eq!(plan.mutants[0].outcome, Outcome::CompileError);
        assert_eq!(plan.mutants[1].outcome, Outcome::NotBuilt);
    }
}

#[test]
fn finishing_remembers_native_check_dep_info_before_a_later_build_without_any_source_artifacts() {
    let (_directory, mut work) = trivial_workspace("native-check-dep-info");
    let mut plan = marked_native_plan(&work, 2);
    let discovery = native_target_discovery(&mut work);
    let path = Utf8PathBuf::from("src/uncompiled.rs");
    fs::write(work.test_root().join(&path), fs::read(work.test_root().join("src/lib.rs")).unwrap()).unwrap();
    let mut file = plan.files[0].clone();
    file.path = path.clone();
    file.absolute = work.test_root().join(&path);
    plan.files.push(file);
    plan.mutants[1].file = path.into();
    let mut calls = 0;
    let built = with_compiler(
        move |_, args, _, _| {
            calls += 1;
            assert!(calls <= 2);
            Ok(Some(scripted_output(true, if args[0] == "check" { &discovery } else { "" }, "")))
        },
        || Converger::default().finish(&work, &mut plan, None, scripted_limits(), &mut NativeConvergenceEvents::default()),
    )
    .unwrap();
    assert!(built.stuck.is_none());
    assert_eq!(plan.mutants[0].outcome, Outcome::Pending);
    assert_eq!(plan.mutants[1].outcome, Outcome::NotBuilt);
}

#[test]
fn convergence_propagates_its_first_native_transport_refusal_without_publishing_a_round() {
    let (_directory, work) = trivial_workspace("compile-initial-transport-refusal");
    let plan = marked_native_plan(&work, 1);
    let mut converger = Converger::default();
    let mut events = NativeConvergenceEvents::default();
    let result = with_compiler(
        |_, _, _, _| Err(error!("initial native compiler transport refused")),
        || {
            converger.converge(
                &work,
                &plan,
                None,
                &["build", "--tests", "--keep-going"],
                scripted_limits(),
                &mut events,
            )
        },
    );
    assert_eq!(
        result.expect_err("the actual first compilation refused transport").to_string(),
        "initial native compiler transport refused"
    );
    assert!(converger.history.is_empty());
    assert!(converger.withdrawn.is_empty());
    assert!(converger.census.is_empty());
    assert!(events.progress.is_empty());
}

#[test]
fn local_proof_progress_leaves_other_items_for_the_next_ordinary_check_without_losing_them() {
    let (_directory, work) = trivial_workspace("native-isolation-local-progress");
    let mut plan = split_native_isolation_plan(&work);
    for mutant in &mut plan.mutants {
        mutant.item_path = format!("item_{}", mutant.ordinal).into();
    }
    let mut converger = Converger::default();
    let mut events = NativeConvergenceEvents::default();
    let result = with_compiler(
        |work, _, _, _| {
            for (name, ordinal) in [("first", 1), ("second", 3)] {
                let source = fs::read_to_string(work.test_root().join(format!("src/{name}.rs"))).unwrap();
                let marker = format!("regression_marker_{ordinal}_end");
                if let Some(offset) = source.find(&marker) {
                    let line = source[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1;
                    let mut diagnostic: serde_json::Value = serde_json::from_str(&unpositioned_native_context(name)).unwrap();
                    diagnostic["message"]["code"]["code"] = "E0382".into();
                    diagnostic["message"]["spans"][0]["line_start"] = line.into();
                    diagnostic["message"]["spans"][0]["line_end"] = line.into();
                    return Ok(Some(scripted_output(false, &diagnostic.to_string(), "")));
                }
            }
            Ok(Some(scripted_output(true, "", "")))
        },
        || {
            converger.converge_scoped(
                &work,
                &plan,
                BuildScope {
                    roots: None,
                    mutants: None,
                    publish_progress: true,
                },
                &["check", "--tests"],
                BuildLimits {
                    rollback_rounds: 4,
                    ..scripted_limits()
                },
                &mut events,
            )
        },
    )
    .unwrap();
    assert!(matches!(result, Convergence::Built(_)));
    assert_eq!(converger.withdrawn, HashSet::from_iter([1, 3]));
    assert!(converger.abandoned.is_empty());
    assert_eq!(converger.history.iter().map(|round| round.withdrew).collect::<Vec<_>>(), [1, 1, 0]);
    assert_eq!(events.progress, [1, 2]);
}

#[test]
fn actual_isolation_withdrawals_mix_a_proved_singleton_with_an_interaction_and_observe_progress_and_rollback_boundaries() {
    for publish in [false, true] {
        for known in [false, true] {
            for rounds in [1, 4] {
                let (_directory, work) = trivial_workspace("native-isolation-convergence");
                let plan = split_native_isolation_plan(&work);
                let mut converger = Converger::default();
                if known {
                    let _ = converger.census.insert(1, CompilerReason::isolated());
                }
                let mut events = NativeConvergenceEvents::default();
                let mut calls = 0;
                let result = with_compiler(
                    move |work, args, _, _| {
                        calls += 1;
                        assert!(
                            calls <= 64,
                            "the finite two-context proof graph must not revisit a failing schema forever"
                        );
                        let first = fs::read_to_string(work.test_root().join("src/first.rs")).unwrap();
                        let second = fs::read_to_string(work.test_root().join("src/second.rs")).unwrap();
                        let first_only = args.windows(2).any(|pair| pair == ["--test", "first"]);
                        let second_only = args.windows(2).any(|pair| pair == ["--test", "second"]);
                        let bad_first = !second_only && first.contains("regression_marker_1_end");
                        let bad_second =
                            !first_only && second.contains("regression_marker_2_end") && second.contains("regression_marker_3_end");
                        let mut diagnostics = Vec::new();
                        if bad_first {
                            diagnostics.push(unpositioned_native_context("first"));
                        }
                        if bad_second {
                            diagnostics.push(unpositioned_native_context("second"));
                        }
                        Ok(Some(scripted_output(diagnostics.is_empty(), &diagnostics.join("\n"), "")))
                    },
                    || {
                        converger.converge_scoped(
                            &work,
                            &plan,
                            BuildScope {
                                roots: None,
                                mutants: None,
                                publish_progress: publish,
                            },
                            &["check", "--tests"],
                            BuildLimits {
                                rollback_rounds: rounds,
                                ..scripted_limits()
                            },
                            &mut events,
                        )
                    },
                )
                .unwrap();
                if rounds == 1 {
                    assert!(matches!(result, Convergence::Stuck(_)));
                    assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
                    assert!(converger.abandoned.is_empty());
                    assert_eq!(converger.census.len(), 1);
                    assert_eq!(converger.per_round, [2]);
                    assert_eq!(converger.history[0].withdrew, 0);
                } else {
                    assert!(matches!(result, Convergence::Built(_)));
                    assert_eq!(converger.withdrawn, HashSet::from_iter([1, 2]));
                    assert_eq!(converger.abandoned, HashSet::from_iter([2]));
                    assert_eq!(converger.interactions.get(&2), Some(&vec![2, 3]));
                    assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));
                    assert_eq!(converger.history.iter().map(|round| round.withdrew).collect::<Vec<_>>(), [2, 0]);
                }
                assert_eq!(events.progress, if known { vec![] } else { vec![1] });
            }
        }
    }
}

#[test]
fn the_actual_convergence_caller_propagates_a_first_isolation_proof_transport_failure_without_publishing_a_verdict() {
    let (_directory, work) = trivial_workspace("native-isolation-caller-error");
    let plan = split_native_isolation_plan(&work);
    let mut converger = Converger::default();
    let mut calls = 0;
    let result = with_compiler(
        move |_, args, _, _| {
            calls += 1;
            assert!(calls <= 2);
            if args.iter().any(|arg| arg == "--test") {
                return Err(error!("native first isolation proof refused"));
            }
            Ok(Some(scripted_output(false, &unpositioned_native_context("first"), "")))
        },
        || {
            converger.converge_scoped(
                &work,
                &plan,
                BuildScope {
                    roots: None,
                    mutants: None,
                    publish_progress: true,
                },
                &["check", "--tests"],
                scripted_limits(),
                &mut NativeConvergenceEvents::default(),
            )
        },
    );
    assert_eq!(result.unwrap_err().message(), "native first isolation proof refused");
    assert!(converger.census.is_empty());
    assert!(converger.withdrawn.is_empty());
    assert!(converger.history.is_empty());
}

#[test]
fn convergence_propagates_native_instrumentation_and_compiler_transport_failures_without_completing_a_round() {
    for native_failure in [false, true] {
        let (_directory, work) = trivial_workspace("native-probe-refusals");
        let plan = marked_native_plan(&work, 4);
        if native_failure {
            fs::remove_file(work.test_root().join("src/lib.rs")).unwrap();
        }
        let mut converger = Converger::default();
        let mut events = NativeConvergenceEvents::default();
        let failure = with_compiler(
            move |_, _, budget, _| {
                assert!(!native_failure, "failed source instrumentation must precede compiler launch");
                assert_eq!(budget, Some(Duration::from_millis(19)));
                Err(error!("native convergence transport refusal"))
            },
            || {
                converger.converge_scoped(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: true,
                    },
                    &["check", "--tests"],
                    scripted_limits(),
                    &mut events,
                )
            },
        )
        .unwrap_err();
        if native_failure {
            assert_eq!(
                failure.message(),
                format!("could not read `{}`", work.test_root().join("src/lib.rs"))
            );
        } else {
            assert_eq!(failure.message(), "native convergence transport refusal");
        }
        assert_eq!(converger.total_rounds, 1);
        assert!(converger.history.is_empty());
        assert!(converger.withdrawn.is_empty());
        assert!(events.progress.is_empty());
    }
}

#[test]
fn a_native_attribution_at_the_rollback_limit_is_recorded_but_never_applied_and_timeout_verbs_keep_their_spaces() {
    for timeout in [false, true] {
        let (_directory, work) = trivial_workspace("native-convergence-limit");
        let plan = marked_native_plan(&work, 2);
        let mut converger = Converger::default();
        let mut events = NativeConvergenceEvents::default();
        let result = with_compiler(
            move |work, arguments, budget, _events| {
                assert_eq!(budget, Some(Duration::from_millis(19)));
                assert_eq!(&arguments[..2], ["check", "--tests"]);
                Ok((!timeout).then(|| scripted_output(false, &native_marker_diagnostic(work, 1), "")))
            },
            || {
                converger.converge_scoped(
                    &work,
                    &plan,
                    BuildScope {
                        roots: None,
                        mutants: None,
                        publish_progress: true,
                    },
                    &["check", "--tests"],
                    BuildLimits {
                        rollback_rounds: 1,
                        ..scripted_limits()
                    },
                    &mut events,
                )
            },
        );
        if timeout {
            let failure = result.unwrap_err();
            assert!(
                failure
                    .message()
                    .starts_with("the compiler invocation `cargo check --tests` was still running after 19ms"),
                "{failure}"
            );
            assert!(converger.history.is_empty());
        } else {
            let Convergence::Stuck(failure) = result.unwrap() else {
                panic!("the first blamed round already exhausts the explicit single-round budget");
            };
            assert!(failure.message().contains("after 1 of the 1 rollback rounds"), "{failure}");
            assert_eq!(converger.history.len(), 1);
            assert_eq!(converger.history[0].withdrew, 0);
            assert_eq!(converger.per_round, [1]);
        }
        assert!(converger.withdrawn.is_empty());
        assert!(converger.census.is_empty());
        assert!(events.progress.is_empty());
        assert_eq!(converger.total_rounds, 1);
    }
}

#[test]
fn convergence_preserves_scope_limits_library_mode_evidence_and_exact_withdrawal_progress() {
    for check in [false, true] {
        for test_lib in [false, true] {
            for (publish, known) in [(false, false), (true, false), (false, true), (true, true)] {
                let (_directory, work) = trivial_workspace("native-convergence-wrappers");
                let plan = marked_native_plan(&work, 2);
                let mut converger = Converger::default();
                converger.select_test_lib(test_lib);
                if known {
                    let _ = converger.census.insert(1, CompilerReason::isolated());
                }
                let mut events = NativeConvergenceEvents {
                    publish_evidence: publish,
                    ..NativeConvergenceEvents::default()
                };
                let mut calls = 0;
                let result = with_compiler(
                    move |work, arguments, budget, _events| {
                        calls += 1;
                        assert!(
                            calls <= 2,
                            "a successful second round must not trigger widening or additional compilation"
                        );
                        assert_eq!(budget, Some(Duration::from_millis(19)));
                        assert_eq!(
                            arguments[0],
                            if test_lib {
                                "test"
                            } else if check {
                                "check"
                            } else {
                                "build"
                            }
                        );
                        assert!(arguments.windows(2).any(|pair| pair == ["--package", "trivial"]));
                        assert_eq!(arguments.iter().any(|argument| argument == "--lib"), test_lib);
                        assert!(!arguments.iter().any(|argument| argument == "--bins"));
                        assert_eq!(arguments.iter().any(|argument| argument == "--tests"), !test_lib);
                        let artifact = serde_json::json!({
                            "reason":"compiler-artifact", "target":{"name":"subject","kind":["lib"]}, "profile":{"test":false}
                        })
                        .to_string();
                        let stdout = if calls == 1 {
                            format!("{}\n{artifact}", native_marker_diagnostic(work, 1))
                        } else {
                            artifact
                        };
                        Ok(Some(scripted_output(calls == 2, &stdout, "")))
                    },
                    || {
                        let verb = if check || test_lib {
                            converger.check_verb()
                        } else {
                            &["build", "--tests", "--keep-going"]
                        };
                        converger.converge_scoped(
                            &work,
                            &plan,
                            BuildScope {
                                roots: Some(&["trivial".into()]),
                                mutants: None,
                                publish_progress: publish,
                            },
                            verb,
                            scripted_limits(),
                            &mut events,
                        )
                    },
                )
                .unwrap();
                assert!(matches!(result, Convergence::Built(_)));
                assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
                assert_eq!(converger.census.len(), 1);
                assert_eq!(converger.total_rounds, 2);
                assert_eq!(converger.history.iter().map(|round| round.withdrew).collect::<Vec<_>>(), [1, 0]);
                assert_eq!(events.progress, if publish && !known { vec![1] } else { vec![] });
                assert_eq!(events.activity, [(1, usize::from(known), 0), (2, 1, 0)]);
                assert_eq!(events.evidence.len(), if publish { 2 } else { 0 });
                if publish {
                    assert_eq!(events.evidence[0].0, 1);
                    assert_eq!(events.evidence[0].1, ["subject (lib)"]);
                    assert_eq!(events.evidence[0].2, ["subject (lib)", "subject (test)"]);
                    assert_eq!(events.evidence[1], (2, vec!["subject (lib)".into()], vec![]));
                }
            }
        }
    }
}

#[test]
fn both_schema_adapters_record_every_unavailable_mutant_in_all_three_retirement_sets() {
    for active in [false, true] {
        let (_directory, work) = trivial_workspace("scripted-unavailable-adapters");
        let mut plan = probe_plan(&work, 0, 2);
        let original = fs::read(work.test_root().join("src/lib.rs")).unwrap();
        let _ = plan
            .digests
            .insert("src/lib.rs".into(), blake3::hash(&original).to_hex().to_string());
        fs::write(
            work.test_root().join("src/lib.rs"),
            "pub fn changed_since_discovery() -> u32 { 9 }\n",
        )
        .unwrap();
        let mut converger = Converger::default();
        if active {
            let written = converger
                .instrument_active_schema(&work, &plan, &HashSet::from_iter([1, 2]))
                .unwrap();
            assert!(written.is_empty());
        } else {
            let (guards, written) = converger.instrument_schema(&work, &plan, &HashSet::default()).unwrap();
            assert!(guards.is_empty());
            assert!(written.is_empty());
        }
        let expected = HashSet::from_iter([1, 2]);
        assert_eq!(converger.withdrawn, expected);
        assert_eq!(converger.abandoned, expected);
        assert_eq!(converger.unavailable, expected);
        assert_eq!(
            fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap(),
            "pub fn changed_since_discovery() -> u32 { 9 }\n"
        );
    }
}

#[test]
fn inconclusive_recovery_fails_without_abandoning_the_dependency_population() {
    let (_directory, work) = trivial_workspace("bounded-isolation-preserves-population");
    let plan = split_native_isolation_plan(&work);
    let mut converger = Converger {
        subset_oracle: Some(|active| active.is_empty().then_some(false)),
        ..Converger::default()
    };
    let result = with_compiler(
        |_, _, _, _| Ok(Some(scripted_output(false, &unpositioned_native_context("first"), ""))),
        || {
            converger.converge_scoped(
                &work,
                &plan,
                BuildScope {
                    roots: None,
                    mutants: None,
                    publish_progress: true,
                },
                &["check", "--tests"],
                scripted_limits(),
                &mut Recorder::default(),
            )
        },
    );
    let message = result.expect_err("unproved compiler failures must stop the campaign").to_string();
    assert!(message.contains("no unproven mutants were excluded"), "{message}");
    assert!(message.contains("--show-build"), "{message}");
    assert!(converger.withdrawn.is_empty());
    assert!(converger.abandoned.is_empty());
    assert!(converger.unresolved.is_empty());
    assert!(converger.census.is_empty());
    assert!(plan.mutants.iter().all(|mutant| mutant.outcome == Outcome::Pending));
}

#[test]
fn proven_progress_is_rechecked_before_an_inconclusive_neighbor_stops_recovery() {
    let (_directory, work) = trivial_workspace("bounded-isolation-partial-progress");
    let plan = split_native_isolation_plan(&work);
    let mut converger = Converger {
        subset_oracle: Some(|active| {
            if active.contains(&1) {
                Some(true)
            } else {
                active.is_empty().then_some(false)
            }
        }),
        ..Converger::default()
    };
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let result = with_compiler(
        {
            let calls = std::rc::Rc::clone(&calls);
            move |work, _, _, _| {
                calls.set(calls.get() + 1);
                let first =
                    fs::read_to_string(work.test_root().join("src/first.rs")).expect("the schema writer prepared the first fixture source");
                let mut diagnostics = vec![unpositioned_native_context("second")];
                if first.contains("regression_marker_1_end") {
                    diagnostics.push(unpositioned_native_context("first"));
                }
                Ok(Some(scripted_output(false, &diagnostics.join("\n"), "")))
            }
        },
        || {
            converger.converge_scoped(
                &work,
                &plan,
                BuildScope {
                    roots: None,
                    mutants: None,
                    publish_progress: true,
                },
                &["check", "--tests"],
                scripted_limits(),
                &mut Recorder::default(),
            )
        },
    );
    assert!(
        result
            .expect_err("an unproved neighbor eventually stops recovery")
            .to_string()
            .contains("no unproven")
    );
    assert_eq!(
        calls.get(),
        2,
        "the proven removal must be followed by an ordinary convergence check"
    );
    assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
    assert_eq!(converger.census.keys().copied().collect::<HashSet<_>>(), HashSet::from_iter([1]));
    assert!(converger.abandoned.is_empty());
    assert!(converger.unresolved.is_empty());
}

#[test]
fn broad_match_consequences_isolate_the_invalid_exit_without_excluding_viable_literal_neighbors() {
    use std::cell::RefCell;
    use std::rc::Rc;

    // The first arm becomes `()` when its mandatory continue is guarded. The type complaint at
    // the next arm and the enclosing match must not turn its two integer edits into culprits.
    let (_directory, work) = causal_fixture_workspace("causal-mandatory-exit-");
    let mut plan = causal_fixture_plan(
        &work,
        "mandatory_exit.rs",
        &[("continue;", "", Shape::Stmt), ("11", "12", Shape::Expr), ("22", "23", Shape::Expr)],
    );
    let mut converger = Converger::default();
    let (guards, _) = converger.instrument_schema(&work, &plan, &HashSet::default()).unwrap();
    let mut diagnostic: Value = serde_json::from_str(include_str!("../src/exec/build/fixtures/enclosing_type_error.json")).unwrap();
    let consequence = &guards[&2].1.site;
    diagnostic["message"]["spans"][0] = span(
        "src/lib.rs",
        consequence.start.line(),
        consequence.start.column(),
        consequence.end.line(),
        consequence.end.column(),
        true,
    );
    let diagnostic = diagnostic.to_string();
    assert!(blame(&diagnostic, &work.test_root(), &guards).is_empty());
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut events = NativeConvergenceEvents::default();
    let convergence = with_compiler(
        {
            let observed = Rc::clone(&observed);
            move |work, _, _, _| {
                let text = fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap();
                let active = (1..=3)
                    .filter(|ordinal| text.contains(&format!("a({ordinal}u32)")))
                    .collect::<HashSet<_>>();
                let failed = active.contains(&1);
                observed.borrow_mut().push(active);
                Ok(Some(scripted_output(!failed, if failed { &diagnostic } else { "" }, "")))
            }
        },
        || converger.converge(&work, &plan, None, &["check"], scripted_limits(), &mut events),
    )
    .unwrap();
    let observed = observed.borrow();
    assert!(matches!(convergence, Convergence::Built(_)));
    assert!(
        !observed.contains(&HashSet::default()),
        "initial validation replaces repeated pristine controls"
    );
    assert!(
        observed.contains(&HashSet::from_iter([1])),
        "the invalid exit needs a singleton failure"
    );
    assert_eq!(observed.last(), Some(&HashSet::from_iter([2, 3])));
    assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
    assert_eq!(converger.census.len(), 1);
    assert_eq!(converger.census[&1].evidence, AttributionEvidence::ProofBuild);
    assert!(converger.abandoned.is_empty());
    assert!(converger.interactions.is_empty());
    assert_eq!(events.progress, [1]);
    converger.settle(&mut plan);
    assert_eq!(
        population(&plan),
        [(1, Outcome::CompileError), (2, Outcome::Pending), (3, Outcome::Pending)]
    );
}

#[test]
fn enclosing_type_consequences_keep_guard_interactions_out_of_the_unviable_census() {
    use std::cell::RefCell;
    use std::rc::Rc;

    // Each suffix is valid with the original unsuffixed neighbor. Only their combined schema
    // constrains the same Option to incompatible u8/u16 types.
    let (_directory, work) = causal_fixture_workspace("causal-type-interaction-");
    let mut plan = causal_fixture_plan(
        &work,
        "inference_interaction.rs",
        &[("11", "12u8", Shape::Expr), ("22", "23u16", Shape::Expr), ("33", "34", Shape::Expr)],
    );
    let mut diagnostic: Value = serde_json::from_str(include_str!("../src/exec/build/fixtures/enclosing_type_error.json")).unwrap();
    diagnostic["message"]["spans"][0]["line_start"] = 3.into();
    diagnostic["message"]["spans"][0]["line_end"] = 4.into();
    diagnostic["message"]["spans"][0]["label"] = "expected `u8`, found `u16`".into();
    diagnostic["message"]["spans"][1]["line_start"] = 2.into();
    diagnostic["message"]["spans"][1]["column_start"] = 1.into();
    diagnostic["message"]["spans"][1]["line_end"] = 4.into();
    diagnostic["message"]["spans"][1]["column_end"] = 1.into();
    diagnostic["message"]["spans"][1]["label"] = "expected because of this".into();
    let diagnostic = diagnostic.to_string();
    let mut converger = Converger::default();
    let (guards, _) = converger.instrument_schema(&work, &plan, &HashSet::default()).unwrap();
    assert!(blame(&diagnostic, &work.test_root(), &guards).is_empty());
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut events = NativeConvergenceEvents::default();
    let convergence = with_compiler(
        {
            let observed = Rc::clone(&observed);
            move |work, _, _, _| {
                let text = fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap();
                let active = (1..=3)
                    .filter(|ordinal| text.contains(&format!("a({ordinal}u32)")))
                    .collect::<HashSet<_>>();
                let failed = active.contains(&1) && active.contains(&2);
                observed.borrow_mut().push(active);
                Ok(Some(scripted_output(!failed, if failed { &diagnostic } else { "" }, "")))
            }
        },
        || converger.converge(&work, &plan, None, &["check"], scripted_limits(), &mut events),
    )
    .unwrap();
    let observed = observed.borrow();
    assert!(matches!(convergence, Convergence::Built(_)));
    for singleton in [1, 2] {
        assert!(
            observed.contains(&HashSet::from_iter([singleton])),
            "each conflicting edit compiles alone"
        );
    }
    assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
    assert_eq!(converger.abandoned, HashSet::from_iter([1]));
    assert_eq!(converger.interactions.get(&1), Some(&vec![1, 2]));
    assert!(converger.census.is_empty());
    assert!(events.progress.is_empty());
    assert_eq!(observed.last(), Some(&HashSet::from_iter([2, 3])));
    converger.settle(&mut plan);
    assert_eq!(
        population(&plan),
        [(1, Outcome::NotBuilt), (2, Outcome::Pending), (3, Outcome::Pending)]
    );
}

#[test]
fn an_ownership_primary_inside_a_viable_replacement_isolates_the_guard_interaction_instead() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let (_directory, work) = causal_fixture_workspace("causal-ownership-interaction-");
    let mut plan = causal_fixture_plan(
        &work,
        "ownership_interaction.rs",
        &[
            ("value = String::new();", "", Shape::Stmt),
            ("break 'done;", "", Shape::Stmt),
            ("core::mem::drop(value)", "consume(value)", Shape::Expr),
        ],
    );
    let mut converger = Converger::default();
    let (guards, _) = converger.instrument_schema(&work, &plan, &HashSet::default()).unwrap();
    let replacement = guards[&3].1.mutated.as_ref().unwrap();
    let diagnostic = reason_message(
        Some("E0382"),
        "use of moved value: `value`",
        &[span(
            "src/lib.rs",
            replacement.start.line(),
            replacement.start.column(),
            replacement.end.line(),
            replacement.end.column(),
            true,
        )],
        &[span("src/lib.rs", 4, 14, 4, 19, false)],
    );
    let mut diagnostic: Value = serde_json::from_str(&diagnostic).unwrap();
    diagnostic["package_id"] = "path+file:///fixture#trivial@0.0.0".into();
    diagnostic["target"] = serde_json::json!({"name": "trivial", "kind": ["lib"]});
    let diagnostic = diagnostic.to_string();
    assert!(blame(&diagnostic, &work.test_root(), &guards).is_empty());
    let observed = Rc::new(RefCell::new(Vec::new()));
    let mut events = NativeConvergenceEvents::default();
    let convergence = with_compiler(
        {
            let observed = Rc::clone(&observed);
            move |work, _, _, _| {
                let text = fs::read_to_string(work.test_root().join("src/lib.rs")).unwrap();
                let active = (1..=3)
                    .filter(|ordinal| text.contains(&format!("a({ordinal}u32)")))
                    .collect::<HashSet<_>>();
                let failed = active.contains(&1) && active.contains(&2);
                observed.borrow_mut().push(active);
                Ok(Some(scripted_output(!failed, if failed { &diagnostic } else { "" }, "")))
            }
        },
        || converger.converge(&work, &plan, None, &["check"], scripted_limits(), &mut events),
    )
    .unwrap();
    assert!(matches!(convergence, Convergence::Built(_)));
    let observed = observed.borrow();
    assert!(observed.contains(&HashSet::from_iter([1])));
    assert!(observed.contains(&HashSet::from_iter([2])));
    assert_eq!(observed.last(), Some(&HashSet::from_iter([2, 3])));
    assert_eq!(converger.withdrawn, HashSet::from_iter([1]));
    assert_eq!(converger.interactions.get(&1), Some(&vec![1, 2]));
    assert!(
        converger.census.is_empty(),
        "neither a guarding artifact nor its consequence is individually unviable"
    );
    assert!(events.progress.is_empty());
    converger.settle(&mut plan);
    assert_eq!(
        population(&plan),
        [(1, Outcome::NotBuilt), (2, Outcome::Pending), (3, Outcome::Pending)]
    );
}
