// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

struct Unavailable<'a> {
    files: &'a MemoryFiles,
    directory: Option<&'a Utf8Path>,
    file: Option<&'a Utf8Path>,
}

impl Files for Unavailable<'_> {
    fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
        if self.file == Some(path) { None } else { self.files.read(path) }
    }

    fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        if self.directory == Some(root) {
            None
        } else {
            self.files.files(root, excluded)
        }
    }
}

struct DisappearingFile<'a> {
    files: &'a MemoryFiles,
    path: &'a Utf8Path,
    remaining: Cell<usize>,
}

impl Files for DisappearingFile<'_> {
    fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
        if path == self.path {
            self.remaining.set(self.remaining.get().checked_sub(1)?);
        }
        self.files.read(path)
    }

    fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        self.files.files(root, excluded)
    }
}

#[test]
fn snapshots_capture_sibling_executables_and_decline_unreadable_toolchain_or_search_directories() {
    let (files, mut unit) = fixture("unit");
    let cargo = unit.compiler.with_file_name("cargo.exe");
    files.put(&cargo, b"cargo");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    assert!(unit.borrowed().reusable(&files, &record));
    files.put(&cargo, b"changed cargo");
    assert!(!unit.borrowed().reusable(&files, &record));
    let bin = unit.compiler.parent().unwrap();
    let library = bin.parent().unwrap().join("lib");
    unit.searches = vec![unit.target.clone()];
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_some());
    for directory in [bin, &library, &unit.target, &unit.root] {
        let failed = Unavailable {
            files: &files,
            directory: Some(directory),
            file: None,
        };
        assert!(unit.borrowed().snapshot(&failed, Vec::new(), Vec::new()).is_none(), "{directory}");
    }
}

#[test]
fn extern_and_transitive_search_inventories_reject_new_neighboring_artifacts() {
    let (files, consumer, mut dependency, _) = super::regressions::dependency_fixture();
    let record = consumer.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    assert!(consumer.borrowed().reusable(&files, &record));
    let neighbor = consumer.target.join("new-sibling.rmeta");
    files.put(&neighbor, b"rust\0new neighbor");
    assert!(!consumer.borrowed().reusable(&files, &record));
    let _removed = files.0.borrow_mut().remove(&neighbor);
    let search = consumer.target.join("dependency-search");
    files.put(search.join("input.rmeta"), b"rust\0search input");
    dependency.searches.push(search.clone());
    let record = dependency.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(dependency.captures.join("dependency.json"), serde_json::to_vec(&record).unwrap());
    assert!(consumer.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_some());
    files.put(search.join("new.rmeta"), b"rust\0new transitive search input");
    assert!(consumer.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none());
}

#[test]
fn nested_capture_metadata_cannot_invalidate_its_own_search_inventory() {
    let (files, mut unit) = fixture("unit");
    let nested = unit.captures.join("nested");
    unit.searches.push(nested.clone());
    files.put(nested.join("previous.json"), b"older capture");
    let script = snapshot_io::Script::from_files(
        files
            .0
            .borrow()
            .iter()
            .map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())),
    );
    snapshot_io::with(script, || {
        let record = unit.borrowed().snapshot(&NativeFiles, Vec::new(), Vec::new()).unwrap();
        assert!(unit.borrowed().reusable(&NativeFiles, &record));
        snapshot_io::write(nested.join("next.json"), b"new capture").unwrap();
        assert!(unit.borrowed().reusable(&NativeFiles, &record));
    });
}

#[test]
fn snapshots_decline_external_source_dependencies_even_when_their_tokens_are_closed() {
    let (files, unit) = fixture("unit");
    let external = drive().join("external").join("module.rs");
    files.put(&external, b"pub fn external() {}");
    files.put(&unit.invocation.source, format!("#[path = {:?}] mod external;", external.as_str()));
    files.put(
        unit.target.join("unit-hash.d"),
        format!(
            "{}: {} {external}\n",
            unit.target.join("libunit-hash.rmeta"),
            unit.invocation.source
        ),
    );
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none());
}

#[test]
fn missing_compiler_parents_and_unreadable_captured_directories_do_not_panic() {
    let (files, mut unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    for compiler in [Utf8PathBuf::new(), Utf8PathBuf::from("rustc"), drive()] {
        unit.compiler = compiler.clone();
        files.put(&compiler, b"compiler");
        assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none(), "{compiler}");
        let mut malformed = record.clone();
        malformed.compiler = compiler.clone();
        malformed.inputs.push((compiler, *blake3::hash(b"compiler").as_bytes()));
        assert_eq!(record_current(&files, &malformed, &unit.root, &unit.target, &unit.captures), None);
    }
    let compiler = &record.compiler;
    for directory in [
        compiler.parent().unwrap().to_owned(),
        compiler.parent().unwrap().parent().unwrap().join("lib"),
    ] {
        let failed = Unavailable {
            files: &files,
            directory: Some(&directory),
            file: None,
        };
        assert_eq!(record_current(&failed, &record, &unit.root, &unit.target, &unit.captures), None);
    }
}

#[test]
fn toolchain_inventory_requires_each_path_even_when_two_executables_have_identical_bytes() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(true)
    );
    files.put(unit.compiler.with_file_name("cargo.exe"), files.read(&unit.compiler).unwrap());
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(false)
    );
}

#[test]
fn toolchain_inventory_does_not_require_unrelated_files_outside_bin_and_lib() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(
        unit.compiler.parent().unwrap().parent().unwrap().join("share").join("README"),
        b"toolchain installation documentation",
    );
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(true)
    );
}

#[test]
fn source_context_rejects_open_externs_even_when_their_current_record_outputs_are_complete() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let artifact = unit.target.join("libunit-hash.rmeta");
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[artifact]}).to_string();
    let path = unit.captures.join("unit.json");
    files.put(&path, serde_json::to_vec(&record).unwrap());
    assert!(source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).is_some());
    for boundary in ["extern", "source"] {
        let mut record = record.clone();
        if boundary == "extern" {
            record.externs.push(unit.target.join("unproven.rmeta"));
        } else {
            record.source = drive().join("external").join("lib.rs");
            files.put(&record.source, b"external source");
            record.inputs.push((record.source.clone(), digest(&files, &record.source).unwrap()));
        }
        files.put(&path, serde_json::to_vec(&record).unwrap());
        assert!(
            source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).is_none(),
            "{boundary}"
        );
    }
}

#[test]
fn source_context_checks_nonmetadata_artifacts_and_build_scripts_after_valid_metadata() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let artifact = unit.target.join("libunit-hash.rmeta");
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[artifact]}).to_string();
    let dep_info = record
        .outputs
        .iter()
        .find(|(path, _)| path.extension() == Some("d"))
        .unwrap()
        .0
        .clone();
    for extra in [
        serde_json::json!({"reason":"build-script-executed"}).to_string(),
        serde_json::json!({"reason":"compiler-artifact","filenames":[dep_info]}).to_string(),
    ] {
        assert!(
            source_only_context_with(
                &files,
                &unit.captures,
                &format!("{stdout}\n{extra}"),
                &unit.root,
                &unit.target,
                None
            )
            .is_none()
        );
    }
}

#[test]
fn source_context_and_dependency_closure_decline_unreadable_capture_inventory_and_records() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("libunit-hash.rmeta")]}).to_string();
    assert!(source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).is_some());
    for (directory, file) in [(Some(unit.captures.as_path()), None), (None, Some(unit.captures.join("unit.json")))] {
        let failed = Unavailable {
            files: &files,
            directory,
            file: file.as_deref(),
        };
        assert!(source_only_context_with(&failed, &unit.captures, &stdout, &unit.root, &unit.target, None).is_none());
    }
    let (files, unit, dependency, _) = super::regressions::dependency_fixture();
    let no_inventory = Unavailable {
        files: &files,
        directory: Some(unit.captures.as_path()),
        file: None,
    };
    assert!(
        unit.borrowed().snapshot(&no_inventory, Vec::new(), Vec::new()).is_some(),
        "an indexed dependency needs its specific record, not a scan of unrelated captures"
    );
    for path in [
        dependency.captures.join("dependency.json"),
        output_index(&unit.captures, &dependency.target.join("libdependency-hash.rmeta")),
    ] {
        let failed = Unavailable {
            files: &files,
            directory: None,
            file: Some(&path),
        };
        assert!(unit.borrowed().snapshot(&failed, Vec::new(), Vec::new()).is_none(), "{path}");
    }
}

#[test]
fn runtime_closure_checks_artifacts_after_runtime_or_duplicate_dependencies() {
    for duplicate in [false, true] {
        let (files, mut unit, command, environment) = super::runtime_regressions::runtime_fixture();
        let (dependency_files, mut dependency) = fixture("dependency");
        let (leaf_files, leaf) = fixture("leaf");
        files.0.borrow_mut().extend(dependency_files.0.into_inner());
        files.0.borrow_mut().extend(leaf_files.0.into_inner());
        let runtime = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
        files.put(
            unit.captures.parent().unwrap().parent().unwrap().join("runtime-provenance"),
            serde_json::to_vec(&runtime).unwrap(),
        );
        unit.runtime = Some(runtime.root.clone());
        dependency.runtime = Some(runtime.root);
        let leaf_capture = leaf.captures.join("leaf.json");
        let record = leaf.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
        files.put(&leaf_capture, serde_json::to_vec(&record).unwrap());
        let artifact = unit.target.join("libgamma_rt-runtime.rmeta");
        dependency.invocation.externs = if duplicate {
            vec![leaf.target.join("libleaf-hash.rmeta"), artifact.clone(), artifact]
        } else {
            vec![leaf.target.join("libleaf-hash.rmeta"), artifact]
        };
        let record = dependency.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
        files.put(dependency.captures.join("dependency.json"), serde_json::to_vec(&record).unwrap());
        unit.invocation.externs = vec![dependency.target.join("libdependency-hash.rmeta")];
        let _record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
        let _removed = files.0.borrow_mut().remove(&leaf_capture);
        assert!(
            unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none(),
            "duplicate={duplicate}"
        );
    }
}

#[test]
fn runtime_externs_require_the_compiler_that_produced_their_provenance() {
    let (files, mut unit, command, environment) = super::runtime_regressions::runtime_fixture();
    let runtime = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
    files.put(
        unit.captures.parent().unwrap().parent().unwrap().join("runtime-provenance"),
        serde_json::to_vec(&runtime).unwrap(),
    );
    unit.runtime = Some(runtime.root);
    unit.invocation.externs.push(unit.target.join("libgamma_rt-runtime.rmeta"));
    unit.compiler = drive().join("other-toolchain").join("bin").join("rustc.exe");
    files.put(&unit.compiler, b"another compiler");
    files.put(drive().join("other-toolchain").join("lib").join("libstd.rlib"), b"another sysroot");
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none());
}

#[test]
fn runtime_source_context_retains_source_and_each_artifact_and_checks_outputs_after_the_runtime() {
    for uses_runtime in [false, true] {
        let (files, mut unit, command, environment) = super::runtime_regressions::runtime_fixture();
        let runtime = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
        let captures = unit.captures.parent().unwrap().parent().unwrap();
        files.put(captures.join("runtime-provenance"), serde_json::to_vec(&runtime).unwrap());
        unit.runtime = Some(runtime.root.clone());
        let runtime_artifact = unit.target.join("libgamma_rt-runtime.rmeta");
        if uses_runtime {
            unit.invocation.externs.push(runtime_artifact.clone());
        }
        let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
        files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
        let stdout = |last: &Utf8Path| serde_json::json!({"reason":"compiler-artifact","filenames":[runtime_artifact,last]}).to_string();
        let context = source_only_context_with(
            &files,
            &unit.captures,
            &stdout(&unit.target.join("libunit-hash.rmeta")),
            &unit.root,
            &unit.target,
            unit.runtime.as_deref(),
        )
        .unwrap();
        for stamp in runtime.files.iter().chain(&runtime.artifacts) {
            assert!(
                context.toolchain.contains(stamp),
                "runtime input {} must constrain subsequent proof reuse, uses_runtime={uses_runtime}",
                stamp.0
            );
        }
        assert_eq!(context.runtime, unit.runtime);
        assert!(context.units.contains(&runtime.context));
        assert!(
            source_only_context_with(
                &files,
                &unit.captures,
                &stdout(&unit.target.join("missing.rmeta")),
                &unit.root,
                &unit.target,
                unit.runtime.as_deref()
            )
            .is_none()
        );
    }
}

#[test]
fn source_context_rejects_runtime_artifacts_rebuilt_after_the_consuming_unit() {
    for metadata_only in [false, true] {
        let (files, mut unit, command, environment) = super::runtime_regressions::runtime_fixture();
        let artifact = unit.target.join("libgamma_rt-runtime.rmeta");
        if metadata_only {
            files.put(
                unit.target.join("gamma_rt-runtime.d"),
                format!(
                    "{artifact}: {}\n",
                    Utf8Path::new(&environment[RUNTIME_VAR]).join("src").join("lib.rs")
                ),
            );
        }
        let runtime = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
        let provenance = unit.captures.parent().unwrap().parent().unwrap().join("runtime-provenance");
        files.put(&provenance, serde_json::to_vec(&runtime).unwrap());
        unit.runtime = Some(runtime.root.clone());
        unit.invocation.externs.push(artifact.clone());
        let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
        files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
        let stdout = serde_json::json!({
            "reason": "compiler-artifact",
            "filenames": [unit.target.join("libunit-hash.rmeta")]
        })
        .to_string();
        assert!(source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, unit.runtime.as_deref()).is_some());

        files.put(&artifact, b"rust\0rebuilt runtime metadata");
        let rebuilt = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
        assert!(rebuilt.valid(&files, &runtime.root, Some(&unit.compiler)));
        assert!(rebuilt.contains(&files, &artifact));
        files.put(&provenance, serde_json::to_vec(&rebuilt).unwrap());
        assert_eq!(
            record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
            Some(true),
            "unit currency deliberately excludes mutable target artifacts"
        );
        assert!(
            source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, unit.runtime.as_deref()).is_none(),
            "current runtime provenance must not validate a unit built against older artifact bytes, metadata_only={metadata_only}"
        );
    }
}

#[test]
fn source_context_uses_current_runtime_sources_inside_target_without_rechecking_obsolete_unit_stamps() {
    let (files, mut unit, command, mut environment) = super::runtime_regressions::runtime_fixture();
    let previous_root = Utf8PathBuf::from(&environment[RUNTIME_VAR]);
    let runtime_root = unit.target.join("runtime");
    let moved = files
        .0
        .borrow()
        .iter()
        .filter_map(|(path, bytes)| {
            path.strip_prefix(&previous_root)
                .ok()
                .map(|relative| (runtime_root.join(relative), bytes.clone()))
        })
        .collect::<Vec<_>>();
    for (path, bytes) in moved {
        files.put(path, bytes);
    }
    let previous_source = previous_root.join("src").join("lib.rs");
    let source = runtime_root.join("src").join("lib.rs");
    let mut relocated = Command::new(command.get_program());
    let _command = relocated.args(command.get_args().map(|argument| {
        if argument == previous_source.as_str() {
            OsString::from(source.as_str())
        } else {
            argument.to_owned()
        }
    }));
    for name in [RUNTIME_VAR, "CARGO_MANIFEST_DIR"] {
        let _previous = environment.insert(name.into(), runtime_root.to_string());
    }
    let dep_info = unit.target.join("gamma_rt-runtime.d");
    let dependencies = String::from_utf8(files.read(&dep_info).unwrap()).unwrap();
    files.put(&dep_info, dependencies.replace(previous_root.as_str(), runtime_root.as_str()));
    let runtime = runtime::capture_with(&files, &relocated, &environment, &unit.cwd).unwrap();
    let provenance = unit.captures.parent().unwrap().parent().unwrap().join("runtime-provenance");
    files.put(&provenance, serde_json::to_vec(&runtime).unwrap());
    unit.runtime = Some(runtime_root);
    unit.invocation.externs.push(unit.target.join("libgamma_rt-runtime.rmeta"));
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let stdout = serde_json::json!({
        "reason": "compiler-artifact",
        "filenames": [unit.target.join("libunit-hash.rmeta")]
    })
    .to_string();
    assert!(source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, unit.runtime.as_deref()).is_some());

    files.put(
        &source,
        "pub fn selected() -> bool { false }\n// Source-only edit preserves runtime artifact bytes.\n",
    );
    let current = runtime::capture_with(&files, &relocated, &environment, &unit.cwd).unwrap();
    assert_eq!(current.artifacts, runtime.artifacts);
    files.put(&provenance, serde_json::to_vec(&current).unwrap());
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(true)
    );
    let context = source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, unit.runtime.as_deref()).unwrap();
    assert!(context.toolchain.contains(&(source.clone(), digest(&files, &source).unwrap())));
    assert!(
        record
            .inputs
            .iter()
            .any(|(path, hash)| path == &source && Some(*hash) != digest(&files, &source)),
        "the old unit must actually contain the superseded source stamp"
    );
}

#[test]
fn source_context_declines_toolchain_inventory_failures_after_reading_a_complete_record() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("libunit-hash.rmeta")]}).to_string();
    let _context = source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).unwrap();
    let bin = unit.compiler.parent().unwrap();
    for directory in [bin.to_owned(), bin.parent().unwrap().join("lib")] {
        let failed = Unavailable {
            files: &files,
            directory: Some(&directory),
            file: None,
        };
        assert!(source_only_context_with(&failed, &unit.captures, &stdout, &unit.root, &unit.target, None).is_none());
    }
}

#[test]
fn source_context_rechecks_advertised_outputs_and_immutable_inputs_before_publication() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let artifact = unit.target.join("libunit-hash.rmeta");
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[artifact]}).to_string();
    let _context = source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).unwrap();
    let library = unit
        .compiler
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lib")
        .join("rustlib")
        .join("host")
        .join("lib")
        .join("libstd.rlib");
    for path in [&artifact, &library, &unit.compiler] {
        let bytes = files.0.borrow_mut().remove(path).unwrap();
        assert!(
            source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).is_none(),
            "{path}"
        );
        files.put(path, bytes);
        assert!(
            source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).is_some(),
            "stamps must be observed again for each publication attempt"
        );
    }
}

#[test]
fn record_currency_requires_a_compiler_stamp_when_target_excludes_the_compiler_inventory() {
    let (files, mut unit) = fixture("unit");
    unit.target = unit.compiler.parent().unwrap().to_owned();
    unit.invocation.out_dir = unit.target.clone();
    files.put(unit.target.join("libunit-hash.rmeta"), b"rust\0metadata");
    files.put(
        unit.target.join("unit-hash.d"),
        format!("{}: {}\n", unit.target.join("libunit-hash.rmeta"), unit.invocation.source),
    );
    let mut record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(true)
    );
    record.inputs.retain(|(path, _)| path != &record.compiler);
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(false),
        "the library inventory cannot supply the excluded compiler's input stamp"
    );
}

#[test]
fn record_currency_declines_a_toolchain_inventory_that_empties_after_the_input_pass() {
    struct EmptyToolchain<'a> {
        files: &'a MemoryFiles,
        bin: &'a Utf8Path,
        libraries: Utf8PathBuf,
    }

    impl Files for EmptyToolchain<'_> {
        fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
            self.files.read(path)
        }

        fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
            if root == self.bin || root == self.libraries {
                Some(Vec::new())
            } else {
                self.files.files(root, excluded)
            }
        }
    }

    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(true)
    );
    let bin = unit.compiler.parent().unwrap();
    let vanished = EmptyToolchain {
        files: &files,
        bin,
        libraries: bin.parent().unwrap().join("lib"),
    };
    assert_eq!(
        record_current(&vanished, &record, &unit.root, &unit.target, &unit.captures),
        Some(false)
    );
}

#[test]
fn record_currency_rechecks_toolchain_bytes_after_the_input_pass() {
    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    assert_eq!(
        record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
        Some(true)
    );
    let library = record.inputs.iter().find(|(path, _)| path.extension() == Some("rlib")).unwrap();
    let failed = DisappearingFile {
        files: &files,
        path: &library.0,
        remaining: Cell::new(1),
    };
    assert_eq!(
        record_current(&failed, &record, &unit.root, &unit.target, &unit.captures),
        Some(false)
    );
    assert_eq!(failed.remaining.get(), 0);
}

#[test]
fn snapshots_decline_externs_that_disappear_after_provenance_validation() {
    let (files, unit, dependency, _) = super::regressions::dependency_fixture();
    let _record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let artifact = dependency.target.join("libdependency-hash.rmeta");
    execute(
        &unit.borrowed(),
        &files,
        None,
        || {
            assert!(files.0.borrow_mut().remove(&artifact).is_some());
            Ok(success())
        },
        |_| panic!("an extern lost during compilation must not publish replay data"),
        |_, _| Ok(()),
    )
    .unwrap();
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none());
}

#[test]
fn source_context_retains_a_workspace_owned_compiler_for_inventory_validation() {
    let (files, mut unit) = fixture("unit");
    let compiler = unit.root.join("toolchain").join("bin").join("rustc.exe");
    files.put(&compiler, files.read(&unit.compiler).unwrap());
    files.put(unit.root.join("toolchain").join("lib").join("libstd.rlib"), b"workspace sysroot");
    unit.compiler = compiler.clone();
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    let stdout = serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("libunit-hash.rmeta")]}).to_string();
    let context = source_only_context_with(&files, &unit.captures, &stdout, &unit.root, &unit.target, None).unwrap();
    assert_eq!(context.toolchain, vec![(compiler.clone(), digest(&files, &compiler).unwrap())]);
}

#[test]
fn native_inventory_enforces_the_actual_entry_budget_at_both_sides_of_its_boundary() {
    use super::snapshot_io::{self, Kind, Script};

    let root = drive().join("entry-budget");
    for count in [MAX_FILES, MAX_FILES + 1] {
        let mut script = Script::default();
        let entries = (0..count)
            .map(|index| Ok((root.join(format!("file-{index}.rs")).into_std_path_buf(), Ok(Kind::File))))
            .collect();
        let _previous = script.directories.insert(root.clone().into_std_path_buf(), Ok(entries));
        snapshot_io::with(script, || {
            assert_eq!(
                NativeFiles.files(&root, Utf8Path::new("")).map(|files| files.len()),
                (count == MAX_FILES).then_some(MAX_FILES)
            );
        });
    }
}
