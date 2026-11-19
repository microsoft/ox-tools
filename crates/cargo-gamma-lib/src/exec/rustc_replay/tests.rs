// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::cell::{Cell, RefCell};

use super::*;

mod closure_regressions;
mod native_process;
pub(super) mod process_io;
mod regressions;
mod runtime_regressions;
pub(in crate::exec) mod snapshot_io;

#[derive(Default)]
struct MemoryFiles(RefCell<BTreeMap<Utf8PathBuf, Vec<u8>>>);

impl MemoryFiles {
    fn put(&self, path: impl Into<Utf8PathBuf>, bytes: impl AsRef<[u8]>) {
        let _previous = self.0.borrow_mut().insert(path.into(), bytes.as_ref().to_vec());
    }
}

impl Files for MemoryFiles {
    fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
        self.0.borrow().get(path).cloned()
    }

    fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        Some(
            self.0
                .borrow()
                .keys()
                .filter(|path| path.starts_with(root) && (excluded.as_str().is_empty() || !path.starts_with(excluded)))
                .cloned()
                .collect(),
        )
    }
}

#[test]
fn live_stream_forwarding_is_exact_bounded_and_drains_after_a_broken_writer() {
    struct BrokenWriter;
    impl io::Write for BrokenWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("Cargo stopped reading"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let input = b"original rustc diagnostic";
    let mut output = Vec::new();
    assert_eq!(capture_stream(&input[..], &mut output, input.len()).unwrap(), Some(input.to_vec()));
    assert_eq!(output, input);
    output.clear();
    assert_eq!(capture_stream(&input[..], &mut output, 3).unwrap(), None);
    assert_eq!(output, input, "oversized streams are still fully forwarded to Cargo");
    let mut reader = io::Cursor::new(input);
    let failure = capture_stream(&mut reader, BrokenWriter, input.len()).expect_err("the relay failure is not a compiler success");
    assert_eq!(failure.to_string(), "Cargo stopped reading");
    assert_eq!(
        reader.position(),
        input.len() as u64,
        "the failed writer does not leave rustc blocked on its pipe"
    );
}

#[test]
fn argument_environment_boundaries_cannot_alias_the_compilation_identity() {
    let mut first = Command::new("rustc");
    let _first = first.current_dir("owned-scratch").arg("a");
    let mut second = Command::new("rustc");
    let _second = second.current_dir("owned-scratch").args(["a", "environment", "b"]);
    assert_ne!(
        command_fingerprint(&first, [("b".into(), "environment".into()), ("c".into(), "d".into())]),
        command_fingerprint(&second, [("c".into(), "d".into())]),
    );
}

fn drive() -> Utf8PathBuf {
    if cfg!(windows) {
        r"C:\".into()
    } else {
        std::path::MAIN_SEPARATOR_STR.into()
    }
}

fn fixture(name: &str) -> (MemoryFiles, OwnedUnit) {
    let files = MemoryFiles::default();
    let root = drive().join("scratch");
    let target = drive().join("outputs");
    let compiler = drive().join("toolchain").join("bin").join("rustc.exe");
    let source = root.join(format!("{name}.rs"));
    let invocation = RustcInvocation {
        crate_name: name.to_owned(),
        crate_types: vec!["lib".into()],
        test: false,
        source: source.clone(),
        out_dir: target.clone(),
        extra_filename: "-hash".into(),
        externs: Vec::new(),
        opaque_extern: false,
    };
    files.put(&source, b"pub fn value() -> u32 { 1 }");
    files.put(root.join("included.txt"), b"included bytes");
    files.put(&compiler, b"compiler identity");
    files.put(
        drive()
            .join("toolchain")
            .join("lib")
            .join("rustlib")
            .join("host")
            .join("lib")
            .join("libstd.rlib"),
        b"sysroot",
    );
    files.put(target.join(format!("lib{name}-hash.rmeta")), b"rust\0metadata");
    files.put(
        target.join(format!("{name}-hash.d")),
        format!(
            "{}: {} {}\n\n{}:\n",
            target.join(format!("lib{name}-hash.rmeta")),
            source,
            root.join("included.txt"),
            source,
        ),
    );
    let captures = target.join("captures").join("replay").join("generation");
    let unit = OwnedUnit {
        context: [1; 32],
        invocation,
        cwd: root.clone(),
        root,
        target,
        compiler,
        searches: Vec::new(),
        captures,
        runtime: None,
    };
    (files, unit)
}

#[test]
fn implicit_externs_and_unstable_source_extensions_cannot_hide_proc_macro_execution() {
    for source in [
        "extern crate hidden;",
        "macro_rules! hidden { () => { extern crate plugin; }; }",
        "#![feature(compiler_private)]",
        "#![cfg_attr(custom, feature(compiler_private))]",
    ] {
        let (files, unit) = fixture("opaque");
        files.put(&unit.invocation.source, source);
        let cache = RefCell::new(None);
        let calls = Cell::new(0);
        let _first = attempt(&files, &unit, &cache, &calls, success());
        let _second = attempt(&files, &unit, &cache, &calls, success());
        assert_eq!(calls.get(), 2, "{source} always reaches the actual compiler");
        assert!(cache.borrow().is_none());
    }
}

fn success() -> CompilerOutput {
    CompilerOutput {
        code: Some(0),
        stdout: b"artifact stream".to_vec(),
        stderr: br#"{"$message_type":"diagnostic","message":"warning","level":"warning"}"#.to_vec(),
        capturable: true,
        forwarded: false,
    }
}

fn attempt(
    files: &MemoryFiles,
    unit: &OwnedUnit,
    cache: &RefCell<Option<Record>>,
    calls: &Cell<usize>,
    output: CompilerOutput,
) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let previous = cache.borrow().clone();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = execute(
        &unit.borrowed(),
        files,
        previous.as_ref(),
        || {
            calls.set(calls.get() + 1);
            Ok(output)
        },
        |record| *cache.borrow_mut() = Some(record.clone()),
        |out, err| {
            stdout.extend_from_slice(out);
            stderr.extend_from_slice(err);
            Ok(())
        },
    )
    .expect("scripted IO succeeds");
    (code, stdout, stderr)
}

#[test]
fn unchanged_siblings_avoid_two_of_three_compiler_invocations_and_relay_diagnostics() {
    let files = MemoryFiles::default();
    let units: Vec<_> = ["affected", "sibling", "other"]
        .map(|name| {
            let (unit_files, mut unit) = fixture(name);
            files.0.borrow_mut().extend(unit_files.0.into_inner());
            unit.searches.push(unit.target.clone());
            unit
        })
        .into_iter()
        .collect();
    let caches: Vec<_> = (0..3).map(|_| RefCell::new(None)).collect();
    let calls = Cell::new(0);
    for (unit, cache) in units.iter().zip(&caches) {
        assert_eq!(attempt(&files, unit, cache, &calls, success()).0, Some(0));
    }
    assert_eq!(calls.get(), 3);
    files.put(&units[0].invocation.source, b"pub fn value() -> u32 { 2 }");
    files.put(units[0].target.join("libaffected-hash.rmeta"), b"rust\0changed schema metadata");
    calls.set(0);
    for (unit, cache) in units.iter().zip(&caches) {
        let (code, stdout, stderr) = attempt(&files, unit, cache, &calls, success());
        assert_eq!(code, Some(0));
        assert_eq!(stdout, success().stdout);
        assert_eq!(
            stderr,
            success().stderr,
            "Cargo receives the original diagnostic stream for attribution"
        );
    }
    assert_eq!(calls.get(), 1, "three scheduled units require only one actual rustc invocation");
}

#[test]
fn include_changes_negative_module_dependencies_toolchain_and_artifacts_force_real_compilation() {
    for changed in [
        drive().join("scratch").join("included.txt"),
        drive().join("scratch").join("new_module").join("mod.rs"),
        drive()
            .join("toolchain")
            .join("lib")
            .join("rustlib")
            .join("host")
            .join("lib")
            .join("libstd.rlib"),
        drive()
            .join("toolchain")
            .join("lib")
            .join("rustlib")
            .join("host")
            .join("lib")
            .join("new-library.rlib"),
        drive().join("outputs").join("libunit-hash.rmeta"),
    ] {
        let (files, unit) = fixture("unit");
        let cache = RefCell::new(None);
        let calls = Cell::new(0);
        let _first = attempt(&files, &unit, &cache, &calls, success());
        files.put(&changed, b"changed");
        let _second = attempt(&files, &unit, &cache, &calls, success());
        assert_eq!(calls.get(), 2, "{changed} invalidates the successful result");
    }
}

#[test]
fn dependency_search_directory_shape_is_checked_without_hashing_unrelated_output_contents() {
    let (files, mut unit) = fixture("unit");
    unit.searches.push(unit.target.clone());
    let unrelated = unit.target.join("libunrelated.rmeta");
    files.put(&unrelated, b"rust\0metadata");
    let cache = RefCell::new(None);
    let calls = Cell::new(0);
    let _first = attempt(&files, &unit, &cache, &calls, success());
    files.put(&unrelated, b"rust\0changed unrelated metadata");
    let _second = attempt(&files, &unit, &cache, &calls, success());
    assert_eq!(calls.get(), 1, "an existing unrelated artifact's bytes cannot affect this unit");
    files.put(unit.target.join("new-transitive.rmeta"), b"new transitive artifact");
    let _third = attempt(&files, &unit, &cache, &calls, success());
    assert_eq!(
        calls.get(),
        2,
        "new candidates in a rustc search directory invalidate the closed snapshot"
    );
}

#[test]
fn failed_uncaptured_missing_outputs_and_incomplete_dep_info_never_replay_success() {
    for mode in ["failure", "missing", "incomplete"] {
        let (files, unit) = fixture("unit");
        let cache = RefCell::new(None);
        let calls = Cell::new(0);
        let mut output = success();
        if mode == "failure" {
            output.code = Some(1);
        } else if mode == "missing" {
            let _removed = files.0.borrow_mut().remove(&unit.target.join("libunit-hash.rmeta"));
        } else {
            files.put(unit.target.join("unit-hash.d"), b"unparseable dependencies");
        }
        let _first = attempt(&files, &unit, &cache, &calls, output);
        assert!(cache.borrow().is_none(), "{mode} cannot establish a reusable success");
        let _second = attempt(&files, &unit, &cache, &calls, success());
        assert_eq!(calls.get(), 2);
    }
}

#[test]
fn schema_transitions_compare_all_actual_inputs_not_only_the_last_written_subset() {
    let (files, unit) = fixture("unit");
    let cache = RefCell::new(None);
    let calls = Cell::new(0);
    for source in ["schema A", "schema B", "schema A"] {
        files.put(&unit.invocation.source, source);
        let _result = attempt(&files, &unit, &cache, &calls, success());
    }
    assert_eq!(calls.get(), 3);
    let _unchanged = attempt(&files, &unit, &cache, &calls, success());
    assert_eq!(calls.get(), 3);
}

#[test]
fn relay_failure_cannot_turn_a_cached_result_into_success() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let failure = execute(
        &unit.borrowed(),
        &files,
        Some(&record),
        || panic!("a closed hit does not start the compiler"),
        |_| panic!("a hit does not republish"),
        |_, _| Err(io::Error::other("broken Cargo pipe")),
    );
    assert_eq!(
        failure.expect_err("the Cargo pipe failure propagates").to_string(),
        "broken Cargo pipe"
    );
}

#[test]
fn full_environment_arguments_compiler_and_cwd_participate_in_identity_without_plaintext_storage() {
    let command = |compiler: &str, cwd: &str, argument: &str| {
        let mut command = Command::new(compiler);
        let _command = command.current_dir(cwd).arg(argument);
        command
    };
    let environment = |value: &str| [(OsString::from("SECRET"), OsString::from(value))];
    let base = command_fingerprint(&command("rustc", r"C:\one", "--test"), environment("one")).unwrap();
    for other in [
        command_fingerprint(&command("other-rustc", r"C:\one", "--test"), environment("one")),
        command_fingerprint(&command("rustc", r"C:\two", "--test"), environment("one")),
        command_fingerprint(&command("rustc", r"C:\one", "--cfg=x"), environment("one")),
        command_fingerprint(&command("rustc", r"C:\one", "--test"), environment("two")),
    ] {
        assert_ne!(other, Some(base));
    }
}

#[test]
fn proc_macro_consumers_build_scripts_custom_tooling_and_nonmetadata_modes_decline_reuse() {
    let (_files, mut unit) = fixture("unit");
    let args: Vec<OsString> = [
        "--crate-name",
        "unit",
        "--crate-type=lib",
        "unit.rs",
        "--out-dir",
        r"C:\outputs",
        "--emit=dep-info,metadata",
    ]
    .map(OsString::from)
    .into();
    assert!(eligible(&args, &unit.invocation, &BTreeMap::new()));
    unit.invocation.externs.push(r"C:\outputs\macro.dll".into());
    assert!(!eligible(&args, &unit.invocation, &BTreeMap::new()));
    unit.invocation.externs.clear();
    for name in ["OUT_DIR", ORIGINAL_WRAPPER_VAR, "RUSTC_WORKSPACE_WRAPPER", "CARGO_BIN_EXE_helper"] {
        assert!(!eligible(
            &args,
            &unit.invocation,
            &BTreeMap::from([(name.into(), "opaque".into())])
        ));
    }
    for extra in [
        "--sysroot=custom",
        "-Zinstrument-coverage",
        "@response",
        "--unknown-option",
        "--emit=dep-info,link",
    ] {
        let mut args = args.clone();
        args.push(extra.into());
        assert!(!eligible(&args, &unit.invocation, &BTreeMap::new()), "{extra}");
    }
}

#[test]
fn proof_context_requires_every_cargo_artifact_and_validates_actual_source_and_toolchain_inputs() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let directory = unit.target.join("captures");
    files.put(directory.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let artifact = unit.target.join("libunit-hash.rmeta");
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[artifact]}).to_string();
    let identity = source_only_context_with(&files, &directory, &stdout, &unit.root, &unit.target, None)
        .expect("all required compiler units were captured");
    assert_ne!(identity.toolchain, []);
    assert_eq!(identity.units, vec![unit.context]);
    let extra = serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("uncaptured.rmeta")]}).to_string();
    assert!(source_only_context_with(&files, &directory, &format!("{stdout}\n{extra}"), &unit.root, &unit.target, None).is_none());
    files.put(&unit.invocation.source, b"changed since the successful compilation");
    assert!(source_only_context_with(&files, &directory, &stdout, &unit.root, &unit.target, None).is_none());
}

#[test]
fn inputs_changing_during_compilation_and_oversized_streams_never_establish_a_capture() {
    for changed in ["source", "module-resolution", "streams"] {
        let (files, unit) = fixture("unit");
        let published = Cell::new(false);
        let _code = execute(
            &unit.borrowed(),
            &files,
            None,
            || {
                let mut output = success();
                if changed == "source" {
                    files.put(&unit.invocation.source, b"changed during compilation");
                } else if changed == "module-resolution" {
                    files.put(unit.root.join("new.rs"), b"new negative dependency");
                } else {
                    output.capturable = false;
                }

                Ok(output)
            },
            |_| published.set(true),
            |_, _| Ok(()),
        )
        .unwrap();
        assert!(!published.get(), "{changed} cannot establish a closed result");
    }
}

#[test]
fn dependent_reuse_requires_closed_transitive_artifact_provenance_not_just_metadata_suffixes() {
    let (files, dependency) = fixture("dependency");
    let (consumer_files, mut consumer) = fixture("consumer");
    files.0.borrow_mut().extend(consumer_files.0.into_inner());
    let artifact = dependency.target.join("libdependency-hash.rmeta");
    consumer.invocation.externs.push(artifact.clone());
    let record = dependency.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(dependency.captures.join("dependency.json"), serde_json::to_vec(&record).unwrap());
    let cache = RefCell::new(None);
    let calls = Cell::new(0);
    let _first = attempt(&files, &consumer, &cache, &calls, success());
    assert!(
        cache.borrow().is_some(),
        "a captured source-only dependency closes this unit's extern graph"
    );
    let _hit = attempt(&files, &consumer, &cache, &calls, success());
    assert_eq!(calls.get(), 1);
    let mut macro_consumer = record;
    macro_consumer.externs.push(dependency.target.join("untracked-proc-macro.dll"));
    files.put(
        dependency.captures.join("dependency.json"),
        serde_json::to_vec(&macro_consumer).unwrap(),
    );
    let _transitive_macro = attempt(&files, &consumer, &cache, &calls, success());
    assert_eq!(calls.get(), 2, "an opaque proc-macro reexport forces real compilation");
    files.put(&artifact, b"rust\0changed dependency without a complete capture");
    let _missing = attempt(&files, &consumer, &cache, &calls, success());
    assert_eq!(calls.get(), 3);
}

#[test]
fn instrumented_units_can_reuse_the_tool_owned_runtime_without_replaying_or_guessing_external_cargo_units() {
    let (files, mut unit) = fixture("subject");
    let runtime_root = drive().join("rt");
    let runtime_source = runtime_root.join("src").join("lib.rs");
    files.put(
        runtime_root.join("Cargo.toml"),
        "[package]\nname='cargo-gamma-rt'\nversion='0.1.0'\n[lib]\nname='gamma_rt'\n",
    );
    files.put(&runtime_source, b"pub fn selected() -> bool { false }");
    let artifact = unit.target.join("libgamma_rt-runtime.rmeta");
    files.put(&artifact, b"rust\0guard metadata");
    files.put(unit.target.join("gamma_rt-runtime.d"), format!("{artifact}: {runtime_source}\n"));
    let mut command = Command::new(&unit.compiler);
    let _command = command.current_dir(&unit.root).args([
        OsString::from("--crate-name"),
        OsString::from("gamma_rt"),
        OsString::from("--crate-type=lib"),
        OsString::from("--emit=dep-info,metadata"),
        OsString::from("--out-dir"),
        unit.target.clone().into(),
        OsString::from("-Cextra-filename=-runtime"),
        runtime_source.into(),
    ]);
    let environment = BTreeMap::from([
        (RUNTIME_VAR.into(), runtime_root.to_string()),
        (TARGET_VAR.into(), unit.target.to_string()),
        (COMMAND_VAR.into(), "complete parent Cargo command identity".into()),
        ("CARGO_MANIFEST_DIR".into(), runtime_root.to_string()),
    ]);
    let runtime = runtime::capture_with(&files, &command, &environment, &unit.root).expect("vendored leaf runtime provenance");
    let captures = unit.captures.parent().unwrap().parent().unwrap();
    files.put(captures.join("runtime-provenance"), serde_json::to_vec(&runtime).unwrap());
    unit.runtime = Some(runtime_root.clone());
    unit.invocation.externs.push(artifact.clone());
    let cache = RefCell::new(None);
    let calls = Cell::new(0);
    let _initial = attempt(&files, &unit, &cache, &calls, success());
    let _hit = attempt(&files, &unit, &cache, &calls, success());
    assert_eq!(calls.get(), 1, "an actual instrumented workspace unit has a production reuse path");
    files.put(
        unit.captures.join("subject.json"),
        serde_json::to_vec(cache.borrow().as_ref().unwrap()).unwrap(),
    );
    let stdout = [
        serde_json::json!({"reason":"compiler-artifact","filenames":[artifact]}).to_string(),
        serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("libsubject-hash.rmeta")]}).to_string(),
    ]
    .join("\n");
    let context = source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, Some(&runtime_root))
        .expect("complete instrumented source-only proof context");
    assert_eq!(context.runtime, Some(runtime_root.clone()));
    let mut expected_contexts = vec![unit.context, runtime.context];
    expected_contexts.sort_unstable();
    expected_contexts.dedup();
    assert_eq!(context.units, expected_contexts);
    {
        let cached = cache.borrow();
        let record = cached.as_ref().unwrap();
        for (path, hash) in &runtime.files {
            assert!(
                record.inputs.contains(&(path.clone(), *hash)),
                "owned runtime sources participate in capture stability"
            );
        }
        assert!(record.inputs.contains(&(artifact.clone(), digest(&files, &artifact).unwrap())));
        let mut expected_toolchain = runtime.files.clone();
        expected_toolchain.extend(runtime.artifacts.iter().cloned());
        expected_toolchain.extend(
            record
                .inputs
                .iter()
                .filter(|(path, _)| !path.starts_with(&unit.root) && !path.starts_with(&unit.target))
                .cloned(),
        );
        expected_toolchain.sort();
        expected_toolchain.dedup();
        assert_eq!(context.toolchain, expected_toolchain);
    }
    assert!(source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).is_none());
    files.put(runtime_root.join("src").join("lib.rs"), b"changed guard implementation");
    let _changed = attempt(&files, &unit, &cache, &calls, success());
    assert_eq!(
        calls.get(),
        2,
        "changed owned-runtime sources require fresh provenance and real compilation"
    );
}
